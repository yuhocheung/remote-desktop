//! 内存身份 + 口令加密导出导入。
//!
//! 与 `rdcore-identity::persist` 的差异：persist 走 `std::fs` 落盘（wasm32 下能编译但
//! 不可调用），本模块只维护**内存身份**，导出/导入以字节形式交给 TS（IndexedDB 持久化）。
//! 私钥保护复用 persist 的思路与格式：口令经 SHA-256 迭代 10 万次 KDF 派生 32 字节密钥
//! （直接复用 [`PassphraseKeyProvider`]），XChaCha20Poly1305 加密，布局
//! `[salt(16)][nonce(24)][密文]`。

use std::cell::RefCell;

use chacha20poly1305::aead::{generic_array::GenericArray, Aead, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;
use rdcore_crypto::{CryptoProvider, Ed25519CryptoProvider, PublicKey, SecretKey};
use rdcore_identity::{create_local_identity, KeyProvider, PassphraseKeyProvider, PeerIdentity};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crate::{hex_decode_array, hex_encode, WebError};

/// 盐长度（与 persist.rs 一致）。
const SALT_LEN: usize = 16;
/// XChaCha20Poly1305 nonce 长度（与 persist.rs 一致）。
const NONCE_LEN: usize = 24;

/// 内存中的本机身份：公开身份 + 私钥（私钥绝不序列化上线）。
#[derive(Clone)]
pub struct LocalIdentity {
    /// 公开身份（DeviceId + 展示名 + 公钥 + 指纹），可随 PeerHello 广播。
    pub public: PeerIdentity,
    /// 私钥（仅进程内，用于握手签名）。
    pub secret: SecretKey,
}

impl LocalIdentity {
    /// 用系统随机数生成全新身份（浏览器下经 getrandom/js 取 `crypto.getRandomValues`）。
    pub fn generate(display_name: &str) -> Self {
        let (public, secret) = create_local_identity(&Ed25519CryptoProvider, display_name);
        Self { public, secret }
    }

    /// 由固定种子确定性构造：Ed25519 种子即私钥，公钥 / 指纹全部现算（绝不独立信任）。
    ///
    /// 用途：黄金测试向量复现、开发调试。生产路径应优先 `generate` 或 `import`。
    pub fn from_seed(seed: [u8; 32], device_id: [u8; 16], display_name: &str) -> Self {
        let provider = Ed25519CryptoProvider;
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let public_key = PublicKey(signing.verifying_key().to_bytes());
        let fingerprint = provider.fingerprint(&public_key);
        Self {
            public: PeerIdentity {
                id: device_id,
                display_name: display_name.to_string(),
                public_key,
                fingerprint,
            },
            secret: SecretKey(seed),
        }
    }

    /// 公开信息 JSON：`{device_id_hex, public_key_hex, fingerprint, display_name}`。
    /// `fingerprint` 为 SHA-256(公钥) 的空格分隔大写 hex（与 `Fingerprint::to_spaced_hex` 一致）。
    pub fn public_json(&self) -> String {
        serde_json::json!({
            "device_id_hex": hex_encode(&self.public.id),
            "public_key_hex": hex_encode(&self.public.public_key.0),
            "fingerprint": self.public.fingerprint.to_spaced_hex(),
            "display_name": self.public.display_name,
        })
        .to_string()
    }

    /// 口令加密导出（随机盐 / nonce）：`[salt(16)][nonce(24)][密文]`。
    pub fn export(&self, passphrase: &str) -> Result<Vec<u8>, WebError> {
        let mut salt = [0u8; SALT_LEN];
        getrandom::getrandom(&mut salt)
            .map_err(|e| WebError::Identity(format!("系统随机数不可用: {e}")))?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| WebError::Identity(format!("系统随机数不可用: {e}")))?;
        self.export_with(passphrase, salt, nonce)
    }

    /// 指定盐与 nonce 的确定性导出变体（黄金测试向量用；语义与 [`LocalIdentity::export`] 一致）。
    pub fn export_with(
        &self,
        passphrase: &str,
        salt: [u8; SALT_LEN],
        nonce: [u8; NONCE_LEN],
    ) -> Result<Vec<u8>, WebError> {
        let payload = serde_json::to_vec(&IdentityExportPayload {
            device_id_hex: hex_encode(&self.public.id),
            secret_key_hex: hex_encode(&self.secret.0),
            display_name: self.public.display_name.clone(),
        })?;
        let key = PassphraseKeyProvider::new(passphrase).derive_key(&salt);
        let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));
        let ct = cipher
            .encrypt(GenericArray::from_slice(&nonce), payload.as_slice())
            .map_err(|_| WebError::Identity("身份导出加密失败".into()))?;
        let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
        out.extend_from_slice(&salt);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 从口令加密的导出字节恢复身份；口令错误或数据被篡改返回 `Err`。
    pub fn import(bytes: &[u8], passphrase: &str) -> Result<Self, WebError> {
        if bytes.len() < SALT_LEN + NONCE_LEN + 16 {
            return Err(WebError::Identity("身份导出数据过短".into()));
        }
        let salt: [u8; SALT_LEN] = bytes[..SALT_LEN].try_into().unwrap();
        let nonce: [u8; NONCE_LEN] = bytes[SALT_LEN..SALT_LEN + NONCE_LEN].try_into().unwrap();
        let ct = &bytes[SALT_LEN + NONCE_LEN..];

        let key = PassphraseKeyProvider::new(passphrase).derive_key(&salt);
        let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));
        let plain = cipher
            .decrypt(GenericArray::from_slice(&nonce), ct)
            .map_err(|_| WebError::Identity("解密失败（口令错误或数据被篡改）".into()))?;
        let payload: IdentityExportPayload = serde_json::from_slice(&plain)
            .map_err(|e| WebError::Identity(format!("身份导出数据损坏: {e}")))?;
        // 公钥 / 指纹从私钥现算，绝不信任导出里的任何派生值。
        Ok(Self::from_seed(
            hex_decode_array(&payload.secret_key_hex)?,
            hex_decode_array(&payload.device_id_hex)?,
            &payload.display_name,
        ))
    }
}

/// 导出加密前的明文负载（JSON）。
#[derive(Serialize, Deserialize)]
struct IdentityExportPayload {
    device_id_hex: String,
    secret_key_hex: String,
    display_name: String,
}

thread_local! {
    /// 当前内存身份（浏览器单线程，thread_local 即可；native 测试同样适用）。
    static CURRENT_IDENTITY: RefCell<Option<LocalIdentity>> = const { RefCell::new(None) };
}

fn set_current(id: LocalIdentity) {
    CURRENT_IDENTITY.with(|c| *c.borrow_mut() = Some(id));
}

/// 取当前身份；未生成 / 导入前返回 `Err`。
pub fn current() -> Result<LocalIdentity, WebError> {
    CURRENT_IDENTITY
        .with(|c| c.borrow().clone())
        .ok_or_else(|| {
            WebError::Identity("身份未初始化：请先 generate_identity 或 identity_import".into())
        })
}

/// 生成全新身份并置为当前身份，返回公开信息 JSON（见 [`LocalIdentity::public_json`]）。
#[wasm_bindgen]
pub fn generate_identity(display_name: Option<String>) -> Result<String, JsError> {
    let id = LocalIdentity::generate(display_name.as_deref().unwrap_or("web-viewer"));
    let json = id.public_json();
    set_current(id);
    Ok(json)
}

/// 由固定种子确定性构造身份（测试向量 / 开发用），返回公开信息 JSON。
#[wasm_bindgen]
pub fn identity_from_seed(
    secret_key_hex: &str,
    device_id_hex: &str,
    display_name: Option<String>,
) -> Result<String, JsError> {
    let id = LocalIdentity::from_seed(
        hex_decode_array(secret_key_hex)?,
        hex_decode_array(device_id_hex)?,
        display_name.as_deref().unwrap_or("web-viewer"),
    );
    let json = id.public_json();
    set_current(id);
    Ok(json)
}

/// 当前身份的公开信息 JSON；未初始化时报错。
#[wasm_bindgen]
pub fn identity_public() -> Result<String, JsError> {
    Ok(current()?.public_json())
}

/// 把当前身份用口令加密导出为字节（交给 TS 存 IndexedDB）。
#[wasm_bindgen]
pub fn identity_export(passphrase: &str) -> Result<Vec<u8>, JsError> {
    Ok(current()?.export(passphrase)?)
}

/// 从口令加密的导出字节恢复身份并置为当前身份，返回公开信息 JSON。
#[wasm_bindgen]
pub fn identity_import(bytes: &[u8], passphrase: &str) -> Result<String, JsError> {
    let id = LocalIdentity::import(bytes, passphrase)?;
    let json = id.public_json();
    set_current(id);
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_identity_is_deterministic() {
        let a = LocalIdentity::from_seed([7u8; 32], [3u8; 16], "dev");
        let b = LocalIdentity::from_seed([7u8; 32], [3u8; 16], "dev");
        assert_eq!(a.public, b.public, "同种子应得到同一身份");
        let c = LocalIdentity::from_seed([8u8; 32], [3u8; 16], "dev");
        assert_ne!(a.public.public_key, c.public.public_key);
    }

    #[test]
    fn export_import_roundtrip_and_wrong_passphrase() {
        let id = LocalIdentity::from_seed([9u8; 32], [4u8; 16], "web");
        let blob = id.export("pw-123").expect("导出应成功");
        let back = LocalIdentity::import(&blob, "pw-123").expect("导入应成功");
        assert_eq!(back.public, id.public);
        assert_eq!(back.secret, id.secret);
        assert!(
            LocalIdentity::import(&blob, "wrong").is_err(),
            "错误口令必须解密失败"
        );
        // 私钥明文字节绝不出现在导出 blob 里。
        assert!(
            !blob.windows(32).any(|w| w == id.secret.0.as_slice()),
            "私钥明文绝不应出现在导出字节中"
        );
    }

    #[test]
    fn export_with_fixed_salt_nonce_is_deterministic() {
        let id = LocalIdentity::from_seed([9u8; 32], [4u8; 16], "web");
        let a = id.export_with("pw", [1u8; 16], [2u8; 24]).unwrap();
        let b = id.export_with("pw", [1u8; 16], [2u8; 24]).unwrap();
        assert_eq!(a, b, "固定盐/nonce 的导出应可复现");
    }
}
