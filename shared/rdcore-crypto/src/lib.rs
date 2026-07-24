//! rdcore-crypto — 密码学类型与 `CryptoProvider` 接口。
//!
//! P0 只定义数据类型与 trait 边界（契约）。**P4 起提供具体后端
//! [`Ed25519CryptoProvider`]**（Ed25519 签名/验签 + SHA-256 指纹），
//! 类型与契约天然匹配：公钥/私钥 32 字节、签名 64 字节、指纹 32 字节。
//! 纯 Rust、对序列化友好。

use chacha20poly1305::aead::{generic_array::GenericArray, Aead, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;
use ed25519_dalek::{Signer, Verifier};
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use sha2::Digest;
pub use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519SecretKey};
use zeroize::Zeroize;

/// 公钥（32 字节，例如 Ed25519 / ECDSA P-256 的原始字节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKey(pub [u8; 32]);

/// 私钥（32 字节）。必须始终留在进程内部，**绝不序列化**（不实现 Serialize）。
///
/// 实现 `Zeroize` 并在 `Drop` 时主动清零内层字节（纵深防御）：无论密钥因何种路径离开作用域
/// （连接结束、进程退出），内存里的私钥材料都不会残留，防冷启动镜像 / 内存抓取泄露。
/// `X25519SecretKey`（x25519-dalek `StaticSecret`）由 dalek 自身在 `Drop` 时已清零，无需额外处理。
// 注意：`SecretKey` **不**派生 `Debug`——私钥字节一旦经 `{:?}` 落入日志/断言就泄露了。
// 下面手写一个脱敏 `Debug`，既保留 `assert_eq!` 失败时的可读性，又不暴露任何密钥字节。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(pub [u8; 32]);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(*** redacted ***)")
    }
}

impl Zeroize for SecretKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// 数字签名（64 字节，例如 Ed25519 签名）。
///
/// serde 默认只为长度 ≤32 的数组派生 Serialize/Deserialize，所以 64 字节字段
/// 借助 `serde-big-array` 的 `BigArray` 才能序列化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature(#[serde(with = "BigArray")] pub [u8; 64]);

/// AEAD 一次性随机数（12 字节，例如 AES-GCM 的 nonce）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nonce(pub [u8; 12]);

/// [`PublicKey`] 的 SHA-256 指纹，用于在带外（如当面/电话）展示给用户做人工核验
/// （TOFU 信任模型）。内部存原始字节，展示时用 [`Fingerprint::to_spaced_hex`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// 渲染成 `AB CD EF ...` 的空格分隔大写十六进制，便于人眼逐字节比对。
    pub fn to_spaced_hex(&self) -> String {
        self.0
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 密码学后端边界（trait）。
///
/// 真正的实现在后续里程碑落地（例如 ring / libsodium / 平台 TPM）。
/// 协议层与身份层只依赖这个 trait，永远不直接依赖某个具体后端。
pub trait CryptoProvider {
    /// 生成一对全新的密钥。
    fn generate_keypair(&self) -> (PublicKey, SecretKey);
    /// 用 `secret` 对 `msg` 签名。
    fn sign(&self, secret: &SecretKey, msg: &[u8]) -> Signature;
    /// 用 `pk` 校验 `msg` 上的 `sig` 是否有效。
    fn verify(&self, pk: &PublicKey, msg: &[u8], sig: &Signature) -> bool;
    /// 由 `pk` 派生出 SHA-256 [`Fingerprint`]（务必始终派生，绝不独立存储）。
    fn fingerprint(&self, pk: &PublicKey) -> Fingerprint;
}

/// 基于 Ed25519 的具体 [`CryptoProvider`] 实现（P4 落地）。
///
/// - 密钥：`ed25519-dalek` 的 32 字节种子即 [`SecretKey`]，其派生验证密钥即 [`PublicKey`]。
/// - 签名：Ed25519 签名正好是 64 字节，与 [`Signature`] 一致。
/// - 指纹：对公钥做 SHA-256，得到 32 字节 [`Fingerprint`]（带外展示给用户做人工核验）。
///
/// 这是真实的非对称签名：签名方持有 [`SecretKey`]，任何知道 [`PublicKey`] 的人都能验签，
/// 但无法伪造——正是远程桌面"设备身份 + 防冒充/MITM"所需要的。
pub struct Ed25519CryptoProvider;

impl CryptoProvider for Ed25519CryptoProvider {
    fn generate_keypair(&self) -> (PublicKey, SecretKey) {
        // 直接用系统随机数填 32 字节种子，避免引入 RNG trait 的版本耦合。
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("系统随机数不可用，无法生成密钥对");
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        (
            PublicKey(verifying.to_bytes()),
            SecretKey(signing.to_bytes()),
        )
    }

    fn sign(&self, secret: &SecretKey, msg: &[u8]) -> Signature {
        let signing = ed25519_dalek::SigningKey::from_bytes(&secret.0);
        let sig = signing.sign(msg);
        Signature(sig.to_bytes())
    }

    fn verify(&self, pk: &PublicKey, msg: &[u8], sig: &Signature) -> bool {
        // 公钥或签名字节非法时直接判失败（不抛），调用方据此拒绝握手。
        let Ok(verifying) = ed25519_dalek::VerifyingKey::from_bytes(&pk.0) else {
            return false;
        };
        // 注意：dalek 2.x 的 `Signature::from_bytes` 直接返回 `Signature`（任意 64 字节都是合法编码）。
        let sig = ed25519_dalek::Signature::from_bytes(&sig.0);
        verifying.verify(msg, &sig).is_ok()
    }

    fn fingerprint(&self, pk: &PublicKey) -> Fingerprint {
        let digest = sha2::Sha256::digest(pk.0);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Fingerprint(out)
    }
}

/// 端到端会话密钥（32 字节，由 X25519 共享密钥经 SHA-256 派生）。
///
/// 用于加密媒体/数据通道（对应架构文档里 DTLS 的等价物）。**绝不序列化落盘**——
/// 每次会话临时派生，会话结束即弃。因此本类型**不派生 `Serialize`/`Deserialize`，
/// `Debug` 也重写为脱敏形式**，从类型层面杜绝密钥经序列化或日志泄露。
///
/// 同样在 `Drop` 时清零（纵深防御）：会话一结束，对称密钥材料立即从内存抹除。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionKey(pub [u8; 32]);

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey(*** redacted ***)")
    }
}

impl Zeroize for SessionKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// AEAD 密文：`[24 字节随机 nonce][密文]`。nonce 随每条消息随机，
/// 因此同一明文每次加密结果都不同（抗重放 / 模式分析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ciphertext {
    /// XChaCha20Poly1305 的 24 字节 nonce。
    pub nonce: [u8; 24],
    /// 密文（含 16 字节 Poly1305 认证标签）。
    pub data: Vec<u8>,
}

/// 生成一对临时 X25519 密钥（用于本次会话的 ECDH，用完即弃，不绑定长期身份）。
pub fn ephemeral_x25519_keypair() -> (X25519PublicKey, X25519SecretKey) {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("系统随机数不可用，无法生成临时密钥");
    let secret = X25519SecretKey::from(bytes);
    let public = X25519PublicKey::from(&secret);
    (public, secret)
}

/// 取 X25519 公钥的原始字节（用于签名 / 传输）。
pub fn x25519_public_bytes(pk: &X25519PublicKey) -> [u8; 32] {
    pk.to_bytes()
}

/// 由 X25519 共享密钥派生会话密钥：两端各自 `a·B` 与 `b·A` 字节相同，
/// 再对共享字节做 SHA-256 得到 32 字节会话密钥（一次性 KDF）。
pub fn derive_session_key(our: &X25519SecretKey, their: &X25519PublicKey) -> SessionKey {
    let shared = our.diffie_hellman(their);
    SessionKey(sha2::Sha256::digest(shared.to_bytes()).into())
}

/// 用会话密钥 AEAD 加密明文，返回带随机 nonce 的密文。
pub fn aead_seal(key: &SessionKey, plaintext: &[u8]) -> Ciphertext {
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce).expect("系统随机数不可用，无法生成 nonce");
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key.0));
    let data = cipher
        .encrypt(GenericArray::from_slice(&nonce), plaintext)
        .expect("AEAD 加密不应失败（除非数据异常大）");
    Ciphertext { nonce, data }
}

/// 用会话密钥 AEAD 解密；任何篡改 / 错误密钥都返回 `None`（不抛，调用方据此断开）。
pub fn aead_open(key: &SessionKey, ct: &Ciphertext) -> Option<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key.0));
    cipher
        .decrypt(GenericArray::from_slice(&ct.nonce), ct.data.as_slice())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_sign_verify_roundtrip() {
        let p = Ed25519CryptoProvider;
        let (pk, sk) = p.generate_keypair();
        let msg = b"session abc / viewer / v=0...";
        let sig = p.sign(&sk, msg);
        assert!(p.verify(&pk, msg, &sig), "正确签名应验签通过");
        // 指纹对同一个公钥稳定且可展示。
        assert_eq!(p.fingerprint(&pk), p.fingerprint(&pk));
        assert!(!p.fingerprint(&pk).0.iter().all(|b| *b == 0));
    }

    #[test]
    fn ed25519_rejects_tampered_and_wrong_key() {
        let p = Ed25519CryptoProvider;
        let (pk, sk) = p.generate_keypair();
        let msg = b"hello";
        let sig = p.sign(&sk, msg);

        // 篡改消息 → 验签失败。
        assert!(!p.verify(&pk, b"goodbye", &sig));

        // 用另一把公钥验 → 失败（防冒充）。
        let (_pk2, sk2) = p.generate_keypair();
        let sig2 = p.sign(&sk2, msg);
        assert!(!p.verify(&pk, msg, &sig2));

        // 非法公钥/签名字节 → 安全失败而非 panic。
        assert!(!p.verify(&PublicKey([0u8; 32]), msg, &sig));
        assert!(!p.verify(&pk, msg, &Signature([0u8; 64])));
    }

    #[test]
    fn x25519_both_ends_derive_same_session_key() {
        let (viewer_pk, viewer_sk) = ephemeral_x25519_keypair();
        let (host_pk, host_sk) = ephemeral_x25519_keypair();
        // 两端各自用"自己的私钥 + 对方公钥"派生，结果必须一致。
        let k1 = derive_session_key(&viewer_sk, &host_pk);
        let k2 = derive_session_key(&host_sk, &viewer_pk);
        assert_eq!(k1, k2, "X25519 ECDH 两端应得到相同会话密钥");
        assert_ne!(k1.0, [0u8; 32], "会话密钥不应全零");
    }

    #[test]
    fn aead_roundtrip_and_tamper_rejected() {
        let key = SessionKey([7u8; 32]);
        let plain = b"screen frame / input event / clipboard";
        let ct = aead_seal(&key, plain);
        // roundtrip
        let back = aead_open(&key, &ct).expect("正常解密应成功");
        assert_eq!(back, plain);
        // 篡改密文 → 解密失败（认证失败）
        let mut tampered = ct.clone();
        if let Some(b) = tampered.data.last_mut() {
            *b ^= 0xFF;
        }
        assert!(aead_open(&key, &tampered).is_none(), "篡改密文必须解密失败");
        // 错误密钥 → 解密失败
        let wrong = SessionKey([9u8; 32]);
        assert!(aead_open(&wrong, &ct).is_none(), "错误密钥必须解密失败");
    }

    #[test]
    fn secret_key_zeroize_clears_bytes() {
        let mut k = SecretKey([0xABu8; 32]);
        k.zeroize();
        assert_eq!(k.0, [0u8; 32], "zeroize 后私钥字节必须清零");
    }

    #[test]
    fn session_key_zeroize_clears_bytes() {
        let mut k = SessionKey([0xCDu8; 32]);
        k.zeroize();
        assert_eq!(k.0, [0u8; 32], "zeroize 后会话密钥字节必须清零");
    }

    // 回归测试：密钥的 `Debug` 必须脱敏，绝不能把原始字节泄漏到日志/断言里。
    // 若有人误加回 `derive(Debug)`，此测试会立刻失败，防止密钥泄露回归。
    #[test]
    fn secret_key_debug_redacts_bytes() {
        let k = SecretKey([0xABu8; 32]);
        let s = format!("{k:?}");
        assert!(s.contains("redacted"), "Debug 应脱敏，实际：{s}");
        assert!(!s.contains("AB"), "Debug 输出不得包含密钥字节");
    }

    #[test]
    fn session_key_debug_redacts_bytes() {
        let k = SessionKey([0xCDu8; 32]);
        let s = format!("{k:?}");
        assert!(s.contains("redacted"), "Debug 应脱敏，实际：{s}");
        assert!(!s.contains("CD"), "Debug 输出不得包含密钥字节");
    }
}
