//! rdcore-web — 浏览器 Viewer 的 WASM 核心（第一开发增量）。
//!
//! 把 `shared/*` 三个 crate（rdcore-proto / rdcore-crypto / rdcore-identity）封装成
//! wasm-bindgen facade，供浏览器端用原生 WebRTC 连接现有 Windows Host：
//!
//! - [`identity`]：Ed25519 身份生成 / 口令加密导出导入（KDF + XChaCha，思路与
//!   `rdcore-identity::persist` 一致）。**只维护内存身份**——`persist.rs` 的 `std::fs`
//!   路径在 wasm32 下虽能编译（std stub）但不可调用，持久化由 TS/IndexedDB 承接。
//! - [`handshake`]：Viewer 侧握手状态机（PeerHello → 签名 Offer → 验签 Answer →
//!   已签名的 X25519 临时密钥交换 → ECDH 派生会话密钥），镜像 `rdcore-session` 与
//!   `rdcore-app::Connection::establish` 的 Viewer 分支。
//! - [`pipeline`]：SCTP 分片重组（1 字节标签 + 16 KiB 切片，镜像 `rdcore-rtc`）、
//!   `[4 字节小端长度][postcard]` 帧格式（镜像 `rdcore-media`）、媒体帧 / 控制消息的
//!   E2E 加解密（镜像 `rdcore-app` 的 `send_media`/`recv_media`/`send_app`/`recv_app`）。
//!
//! 接口约定：字节进字节出（JS 侧 `Uint8Array`），结构化数据用 JSON 字符串。

pub mod handshake;
pub mod identity;
pub mod pipeline;

pub use rdcore_crypto;
pub use rdcore_identity;
pub use rdcore_proto;

/// facade 统一错误类型（纯 Rust 侧）。wasm 导出函数统一映射为 `JsError`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    /// 身份未初始化 / 导入口令错误 / 导出格式损坏。
    Identity(String),
    /// 握手消息没有携带签名（对端未认证）。
    MissingSignature,
    /// 签名校验失败（负载被篡改或密钥不匹配）。
    InvalidSignature,
    /// 发送方 `from` 未知（未先收到其 PeerHello）。
    UnknownPeer,
    /// 消息携带的 session_id 与本握手不符（防跨会话替换）。
    InvalidSession,
    /// 会话密钥尚未建立。
    NoSessionKey,
    /// 协议编解码 / 帧格式错误。
    Protocol(String),
    /// JSON 编解码错误。
    Json(String),
    /// 输入参数非法（hex 长度 / 字面值越界等）。
    BadInput(String),
    /// AEAD 解密失败（篡改 / 密钥不匹配）等密码学失败。
    Crypto(String),
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebError::Identity(s) => write!(f, "identity: {s}"),
            WebError::MissingSignature => write!(f, "握手缺少签名（对端未认证）"),
            WebError::InvalidSignature => write!(f, "签名校验失败（负载被篡改或密钥不匹配）"),
            WebError::UnknownPeer => write!(f, "未知对端（未先收到其 PeerHello）"),
            WebError::InvalidSession => write!(f, "会话 ID 不匹配（疑似跨会话替换密钥）"),
            WebError::NoSessionKey => write!(f, "会话密钥尚未建立"),
            WebError::Protocol(s) => write!(f, "protocol: {s}"),
            WebError::Json(s) => write!(f, "json: {s}"),
            WebError::BadInput(s) => write!(f, "bad input: {s}"),
            WebError::Crypto(s) => write!(f, "crypto: {s}"),
        }
    }
}

impl std::error::Error for WebError {}

impl From<rdcore_proto::ProtocolError> for WebError {
    fn from(e: rdcore_proto::ProtocolError) -> Self {
        WebError::Protocol(e.to_string())
    }
}

impl From<serde_json::Error> for WebError {
    fn from(e: serde_json::Error) -> Self {
        WebError::Json(e.to_string())
    }
}

// 注：无需手写 `From<WebError> for JsError` —— wasm-bindgen 0.2.126 已有
// `impl<E: StdError> From<E> for JsError` 的 blanket impl，WebError 实现 StdError 即自动可用。

/// 小写 hex 编码（DeviceId / 公钥 / 指纹的展示与 JSON 承载格式）。
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 解析小写/大写 hex；长度必须为偶数。
pub fn hex_decode(s: &str) -> Result<Vec<u8>, WebError> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(WebError::BadInput(format!(
            "hex 长度必须为偶数: {}",
            s.len()
        )));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| WebError::BadInput(format!("非法 hex 字符: {}", &s[i..i + 2])))
        })
        .collect()
}

/// 解析定长 hex（如 16 字节 DeviceId / 32 字节公钥）。
pub fn hex_decode_array<const N: usize>(s: &str) -> Result<[u8; N], WebError> {
    let v = hex_decode(s)?;
    v.try_into().map_err(|v: Vec<u8>| {
        WebError::BadInput(format!("hex 应解码为 {N} 字节，实际 {}", v.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes = [0x00u8, 0xab, 0xcd, 0xef, 0xff];
        let s = hex_encode(&bytes);
        assert_eq!(s, "00abcdefff");
        assert_eq!(hex_decode(&s).unwrap(), bytes);
        assert_eq!(
            hex_decode("00ABCDEFFF").unwrap(),
            bytes,
            "大写 hex 也应可解析"
        );
    }

    #[test]
    fn hex_rejects_bad_input() {
        assert!(hex_decode("abc").is_err(), "奇数长度应拒绝");
        assert!(hex_decode("zz").is_err(), "非法字符应拒绝");
        assert!(hex_decode_array::<16>(&hex_encode(&[1u8; 15])).is_err());
    }
}
