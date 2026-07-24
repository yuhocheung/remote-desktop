//! auth —— 认证服务（设备身份认证 + 访问令牌签发/校验）。
//!
//! 缺口 L 的云端控制面之一。本 crate 为**纯库**：真实领域逻辑 + 内存凭证存储 +
//! `rdcore-crypto` 的 Ed25519 非对称签名签发自包含令牌（类 JWT，但签名用项目自己的
//! 密码学后端，不引入 `jsonwebtoken`）。网关（gateway）负责把本服务暴露为 HTTP 端点。
//!
//! 设计要点：
//! - 令牌 = `b64url(header).b64url(claims).b64url(ed25519(header.claims))`，服务端持有
//!   私钥签名、公钥验签；不依赖外部 KMS。
//! - `claims.scopes` 由网关在登录时向 permission 服务查询后注入，auth 自身只负责"身份真伪"。
//! - 密码以 `sha256` 摘要存储（开发级；生产应换 argon2/bcrypt + 盐）。私钥经 `rdcore-crypto`
//!   `SecretKey` 持有，Drop 时清零。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rdcore_crypto::{CryptoProvider, Ed25519CryptoProvider, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 令牌默认有效期（与 signaling-svc 的 per-session token TTL 对齐：15 分钟）。
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// 可被授予的权限范围，与 Flutter 端 `ConsentScope`（View/Input/Clipboard/FileTransfer）
/// 及 Rust `permission` 服务对齐（PascalCase 序列化名一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Scope {
    View,
    Input,
    Clipboard,
    FileTransfer,
}

/// 令牌载荷（claims）。`sub` 为设备/用户标识；`scopes` 为该身份被授予的权限；
/// `iat`/`exp` 为 Unix 秒；`jti` 为唯一令牌 id（吊销/审计用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub scopes: Vec<Scope>,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenHeader {
    alg: String,
    typ: String,
}

/// 签发后的令牌：同时保留原始字符串与解析出的 claims，便于网关透传/鉴权。
#[derive(Debug, Clone)]
pub struct SignedToken {
    raw: String,
    claims: Claims,
}

impl SignedToken {
    pub fn raw(&self) -> &str {
        &self.raw
    }
    pub fn claims(&self) -> &Claims {
        &self.claims
    }
}

/// 认证失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// 令牌格式非法（不是三段 base64url）。
    Malformed,
    /// 签名校验失败（伪造/篡改）。
    InvalidSignature,
    /// 令牌已过期。
    Expired,
    /// 凭据不匹配（设备不存在或密码错误）。
    BadCredentials,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AuthError::Malformed => "令牌格式非法",
            AuthError::InvalidSignature => "签名校验失败",
            AuthError::Expired => "令牌已过期",
            AuthError::BadCredentials => "凭据不匹配",
        };
        f.write_str(s)
    }
}

impl std::error::Error for AuthError {}

/// 认证服务：持有签名密钥对 + 内存凭证表。线程安全（`Arc<AuthService>` 即可共享）。
pub struct AuthService {
    crypto: Ed25519CryptoProvider,
    public: PublicKey,
    secret: SecretKey,
    /// 设备标识 -> 密码 `sha256` 摘要。
    credentials: Mutex<HashMap<String, [u8; 32]>>,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 脱敏：绝不打印私钥/密码摘要。
        f.debug_struct("AuthService")
            .field("public", &self.public)
            .field("credential_count", &self.credentials.lock().unwrap().len())
            .finish()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hash_password(pw: &str) -> [u8; 32] {
    let d = Sha256::digest(pw.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    out
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    URL_SAFE_NO_PAD.decode(s)
}

#[allow(clippy::new_without_default)]
impl AuthService {
    /// 生成全新签名密钥对（内存态；生产应持久化公钥/私钥到 KMS）。
    pub fn new() -> Self {
        let crypto = Ed25519CryptoProvider;
        let (public, secret) = crypto.generate_keypair();
        Self {
            crypto,
            public,
            secret,
            credentials: Mutex::new(HashMap::new()),
        }
    }

    /// 用既有密钥对构造（例如从配置加载）。
    pub fn with_keys(public: PublicKey, secret: SecretKey) -> Self {
        Self {
            crypto: Ed25519CryptoProvider,
            public,
            secret,
            credentials: Mutex::new(HashMap::new()),
        }
    }

    /// 注册/更新设备凭据（幂等 upsert）。生产应加"设备已存在则拒绝"或管理员审批。
    pub fn register_device(&self, id: &str, password: &str) {
        self.credentials
            .lock()
            .unwrap()
            .insert(id.to_string(), hash_password(password));
    }

    /// 校验设备凭据，成功返回设备标识（subject）。
    pub fn authenticate(&self, id: &str, password: &str) -> Result<String, AuthError> {
        let store = self.credentials.lock().unwrap();
        let expected = store.get(id).ok_or(AuthError::BadCredentials)?;
        if *expected != hash_password(password) {
            return Err(AuthError::BadCredentials);
        }
        Ok(id.to_string())
    }

    /// 为某身份签发带权限范围的令牌（通常由网关在登录时查询 permission 后调用）。
    pub fn issue_token(&self, sub: &str, scopes: &[Scope]) -> SignedToken {
        let claims = Claims {
            sub: sub.to_string(),
            scopes: scopes.to_vec(),
            iat: now_secs(),
            exp: now_secs() + TOKEN_TTL.as_secs(),
            jti: format!("{:x}", now_secs() ^ u64::from_be_bytes(rand_bytes())),
        };
        let header = TokenHeader {
            alg: "Ed25519".to_string(),
            typ: "JWT".to_string(),
        };
        let h = b64(&serde_json::to_vec(&header).expect("header 序列化失败"));
        let c = b64(&serde_json::to_vec(&claims).expect("claims 序列化失败"));
        let signing_input = format!("{h}.{c}");
        let sig = self.crypto.sign(&self.secret, signing_input.as_bytes());
        let raw = format!("{signing_input}.{}", b64(&sig.0));
        SignedToken { raw, claims }
    }

    /// 校验令牌：格式/签名/有效期，成功返回 claims。
    pub fn verify_token(&self, token: &str) -> Result<Claims, AuthError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(AuthError::Malformed);
        }
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = b64_decode(parts[2]).map_err(|_| AuthError::Malformed)?;
        if sig_bytes.len() != 64 {
            return Err(AuthError::Malformed);
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&sig_bytes);
        let sig = Signature(arr);
        if !self
            .crypto
            .verify(&self.public, signing_input.as_bytes(), &sig)
        {
            return Err(AuthError::InvalidSignature);
        }
        let claims: Claims =
            serde_json::from_slice(&b64_decode(parts[1]).map_err(|_| AuthError::Malformed)?)
                .map_err(|_| AuthError::Malformed)?;
        if now_secs() >= claims.exp {
            return Err(AuthError::Expired);
        }
        Ok(claims)
    }
}

/// 轻量随机字节（用于 jti 唯一性，无需强随机；与系统时间异或即可）。
fn rand_bytes() -> [u8; 8] {
    // 用高分辨率计时器扰动，避免引入 `getrandom` 依赖；仅用于 id 去重，非安全用途。
    let n = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let bytes = n.to_be_bytes(); // [u8; 16]
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[8..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_authenticate_ok_and_wrong_password_rejected() {
        let svc = AuthService::new();
        svc.register_device("dev-1", "s3cret");
        assert_eq!(svc.authenticate("dev-1", "s3cret").unwrap(), "dev-1");
        assert_eq!(
            svc.authenticate("dev-1", "wrong").unwrap_err(),
            AuthError::BadCredentials
        );
        assert_eq!(
            svc.authenticate("dev-2", "s3cret").unwrap_err(),
            AuthError::BadCredentials
        );
    }

    #[test]
    fn issued_token_verifies_and_expires() {
        let svc = AuthService::new();
        let token = svc.issue_token("dev-1", &[Scope::View, Scope::Input]);
        let claims = svc.verify_token(token.raw()).expect("合法令牌应验签通过");
        assert_eq!(claims.sub, "dev-1");
        assert_eq!(claims.scopes, vec![Scope::View, Scope::Input]);
        assert!(claims.exp > claims.iat);

        // 篡改令牌（改一段）-> 验签失败。
        let mut parts: Vec<&str> = token.raw().split('.').collect();
        parts[1] = "AAAA";
        let tampered = parts.join(".");
        assert_eq!(
            svc.verify_token(&tampered).unwrap_err(),
            AuthError::InvalidSignature
        );

        // 完全非法的令牌 -> Malformed。
        assert_eq!(
            svc.verify_token("not.a.jwt").unwrap_err(),
            AuthError::Malformed
        );
    }

    #[test]
    fn token_scope_roundtrip_serializes_pascal_case() {
        let svc = AuthService::new();
        let token = svc.issue_token("d", &[Scope::Clipboard, Scope::FileTransfer]);
        // claims 经 serde 序列化后范围名应为 PascalCase。
        let json = serde_json::to_string(token.claims()).unwrap();
        assert!(json.contains("\"Clipboard\""), "claims json: {json}");
        assert!(json.contains("\"FileTransfer\""), "claims json: {json}");
    }
}
