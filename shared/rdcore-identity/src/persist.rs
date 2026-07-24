//! B1（韧性面）：设备身份的磁盘持久化实现，替换 `InMemoryIdentityStore`。
//!
//! 安全目标：TOFU 信任模型落地——重启后本机身份与已记住对端不丢，无需重复扫码。
//!
//! 私钥保护（采纳 WorkBuddy 建议，先做"salted 加密落盘 + 密钥来源可轮换"）：
//! - `SecretKey` **绝不明文落盘**：用口令经 KDF（SHA-256 迭代）派生 32 字节密钥，
//!   再用 XChaCha20Poly1305（与 `rdcore-crypto` 的会话 AEAD 同族）加密后存盘。
//! - 密钥来源抽象为 [`KeyProvider`] trait，可轮换：默认 [`PassphraseKeyProvider`]（口令），
//!   后续可插 OS 钥匙串（DPAPI/Keychain/SecretService）实现同一 trait，无需改本模块。
//!
//! 文件布局（`dir` 下）：
//! - `identity.json`：本机 `PeerIdentity`（公钥/指纹/展示名，本就公开）+ 已记住对端表。
//! - `secret.enc`：加密后的私钥（`[salt][nonce][密文]`），无明文密钥材料。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{generic_array::GenericArray, Aead, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use zeroize::Zeroize;

use rdcore_crypto::{CryptoProvider, SecretKey};

use crate::{create_local_identity, DeviceId, IdentityStore, PeerIdentity};

/// KDF 迭代次数（SHA-256 反复迭代，摊薄口令猜测）。越大越抗暴力破解，越慢。
const KDF_ITERATIONS: u32 = 100_000;
/// 盐长度（随机，每个安装一份）。
const SALT_LEN: usize = 16;
/// XChaCha20Poly1305 nonce 长度。
const NONCE_LEN: usize = 24;
/// 私钥明文字节数。
const SECRET_LEN: usize = 32;

/// 持久化错误。
#[derive(Debug)]
pub enum PersistError {
    /// 底层 I/O（读/写/建目录）。
    Io(std::io::Error),
    /// 落盘数据损坏 / 反序列化失败。
    Corrupt(String),
    /// 私钥解密失败（口令错误或文件被篡改）。
    DecryptFailed,
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "io: {e}"),
            PersistError::Corrupt(s) => write!(f, "corrupt: {s}"),
            PersistError::DecryptFailed => {
                write!(f, "decrypt failed (wrong passphrase or tampered)")
            }
        }
    }
}

impl std::error::Error for PersistError {}

impl From<std::io::Error> for PersistError {
    fn from(e: std::io::Error) -> Self {
        PersistError::Io(e)
    }
}

/// 私钥保护密钥的来源（可轮换）。
///
/// 默认实现是口令；后续可插 OS 钥匙串（DPAPI/Keychain/SecretService）而无需改
/// `PersistentIdentityStore`——只要也实现本 trait 提供同一把 32 字节保护密钥即可。
pub trait KeyProvider {
    /// 用给定的盐派生出 32 字节保护密钥（用于加密/解密私钥）。
    fn derive_key(&self, salt: &[u8]) -> [u8; 32];
}

/// 口令驱动的 [`KeyProvider`]：口令 + 盐经 SHA-256 迭代 [`KDF_ITERATIONS`] 次派生密钥。
///
/// 口令本身不存盘（只存加密后的私钥）；验证口令的方式是"解密能否成功"。
pub struct PassphraseKeyProvider {
    passphrase: String,
}

impl PassphraseKeyProvider {
    pub fn new(passphrase: impl Into<String>) -> Self {
        Self {
            passphrase: passphrase.into(),
        }
    }
}

impl KeyProvider for PassphraseKeyProvider {
    fn derive_key(&self, salt: &[u8]) -> [u8; 32] {
        // 简单可移植的 KDF：SHA-256(password || salt) 反复迭代。生产可换 Argon2（同为 KeyProvider）。
        let mut state = sha2::Sha256::digest([self.passphrase.as_bytes(), salt].concat());
        for _ in 0..KDF_ITERATIONS {
            state = sha2::Sha256::digest(state);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&state);
        out
    }
}

impl Drop for PassphraseKeyProvider {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

/// 落盘的身份文件（公钥部分，本就公开，JSON 明文存）。
///
/// `peers` 用 `Vec` 而非 `HashMap`：`DeviceId = [u8;16]` 不能直接作 JSON map 的 key
/// （JSON object 的 key 必须是字符串），用数组最稳。内存中仍按 `DeviceId` 索引。
#[derive(Serialize, Deserialize)]
struct IdentityFile {
    local: PeerIdentity,
    peers: Vec<PeerIdentity>,
}

/// 磁盘持久化的 [`IdentityStore`]：替换 `InMemoryIdentityStore`。
///
/// 身份与对端表在内存中操作，变更即原子写盘；私钥由 `KeyProvider` 派生密钥加密后单独存放。
pub struct PersistentIdentityStore {
    dir: PathBuf,
    self_identity: PeerIdentity,
    peers: HashMap<DeviceId, PeerIdentity>,
}

impl PersistentIdentityStore {
    fn identity_path(dir: &Path) -> PathBuf {
        dir.join("identity.json")
    }
    fn secret_path(dir: &Path) -> PathBuf {
        dir.join("secret.enc")
    }

    /// 加载已有身份；目录或文件不存在时，用 `provider` 生成新身份并落盘。
    ///
    /// 返回 `(store, SecretKey)`：`SecretKey` 只交给调用方（进程内使用），本 store 不长期持有明文。
    pub fn load_or_create(
        dir: impl AsRef<Path>,
        provider: &impl CryptoProvider,
        display_name: &str,
        keys: &impl KeyProvider,
    ) -> Result<(Self, SecretKey), PersistError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let id_path = Self::identity_path(&dir);
        let sec_path = Self::secret_path(&dir);

        if id_path.exists() && sec_path.exists() {
            let store = Self::load(&dir)?;
            let secret = load_secret(&sec_path, keys)?;
            Ok((store, secret))
        } else {
            let (identity, secret) = create_local_identity(provider, display_name);
            let store = Self {
                dir,
                self_identity: identity,
                peers: HashMap::new(),
            };
            store.write_identity()?;
            save_secret(&Self::secret_path(&store.dir), &secret, keys)?;
            Ok((store, secret))
        }
    }

    /// 仅从磁盘加载身份与对端表（不取私钥；私钥用 [`load_secret`] 单独取）。
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, PersistError> {
        let dir = dir.as_ref().to_path_buf();
        let raw = fs::read(Self::identity_path(&dir))?;
        let file: IdentityFile =
            serde_json::from_slice(&raw).map_err(|e| PersistError::Corrupt(e.to_string()))?;
        Ok(Self {
            dir,
            self_identity: file.local,
            peers: file.peers.into_iter().map(|p| (p.id, p)).collect(),
        })
    }

    /// 取回加密保存的私钥（用 `keys` 解密）。
    pub fn load_secret(&self, keys: &impl KeyProvider) -> Result<SecretKey, PersistError> {
        load_secret(&Self::secret_path(&self.dir), keys)
    }

    /// 原子写身份文件（先写临时文件再 rename，避免中途断电留下半截文件）。
    fn write_identity(&self) -> Result<(), PersistError> {
        let file = IdentityFile {
            local: self.self_identity.clone(),
            peers: self.peers.values().cloned().collect(),
        };
        let json =
            serde_json::to_vec_pretty(&file).map_err(|e| PersistError::Corrupt(e.to_string()))?;
        let path = Self::identity_path(&self.dir);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
    /// 记住对端并落盘；写盘失败时返回 `Err`（**不静默**）。
    ///
    /// 这是 `IdentityStore::remember` 的 `Result` 版。TOFU 的关键承诺是"配对不丢"——
    /// 写盘失败（磁盘满/权限）必须让调用方知晓，否则用户以为配对成功、重启后却丢设备。
    pub fn try_remember(&mut self, peer: PeerIdentity) -> Result<(), PersistError> {
        self.peers.insert(peer.id, peer);
        self.write_identity()
    }
}

impl IdentityStore for PersistentIdentityStore {
    fn local_identity(&self) -> &PeerIdentity {
        &self.self_identity
    }

    fn remember(&mut self, peer: PeerIdentity) {
        // trait 契约（`remember` 不返回 Result）无法向上传播错误；
        // 但绝不静默——失败至少落到 stderr，且推荐调用方改用 `try_remember` 拿到 Err。
        if let Err(e) = self.try_remember(peer) {
            eprintln!("[rdcore-identity] 警告：记住对端落盘失败（重启后可能丢失该配对）: {e}");
        }
    }

    fn lookup(&self, id: &DeviceId) -> Option<&PeerIdentity> {
        if &self.self_identity.id == id {
            return Some(&self.self_identity);
        }
        self.peers.get(id)
    }
}

/// 加密私钥并落盘：`[salt(16)][nonce(24)][密文(32+16 tag)]`。
fn save_secret(
    path: &Path,
    secret: &SecretKey,
    keys: &impl KeyProvider,
) -> Result<(), PersistError> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|e| PersistError::Corrupt(e.to_string()))?;
    let key = keys.derive_key(&salt);

    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(|e| PersistError::Corrupt(e.to_string()))?;

    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));
    let mut plain = secret.0;
    let ct = cipher
        .encrypt(GenericArray::from_slice(&nonce), plain.as_ref())
        .map_err(|_| PersistError::DecryptFailed)?;
    plain.zeroize();

    let mut out = Vec::with_capacity(SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    fs::write(path, out)?;
    Ok(())
}

/// 读盘并解密私钥；口令错误或文件被篡改返回 `DecryptFailed`。
fn load_secret(path: &Path, keys: &impl KeyProvider) -> Result<SecretKey, PersistError> {
    let raw = fs::read(path)?;
    if raw.len() < SALT_LEN + NONCE_LEN + SECRET_LEN {
        return Err(PersistError::Corrupt("secret file too short".into()));
    }
    let salt = &raw[..SALT_LEN];
    let nonce = &raw[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ct = &raw[SALT_LEN + NONCE_LEN..];

    let key = keys.derive_key(salt);
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key));
    let plain = cipher
        .decrypt(GenericArray::from_slice(nonce), ct)
        .map_err(|_| PersistError::DecryptFailed)?;
    if plain.len() != SECRET_LEN {
        return Err(PersistError::Corrupt("secret length mismatch".into()));
    }
    let mut sk = [0u8; SECRET_LEN];
    sk.copy_from_slice(&plain);
    Ok(SecretKey(sk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::Ed25519CryptoProvider;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "rdcore-identity-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn create_then_reload_same_identity() {
        let dir = tmpdir("reload");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("pw-123");
        let (store, secret) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "my-laptop", &keys).unwrap();
        let id = store.local_identity().id;
        let fp = store.local_identity().fingerprint.clone();
        drop(store);

        // 模拟重启：重新 load，身份与私钥都应一致。
        let store2 = PersistentIdentityStore::load(&dir).unwrap();
        assert_eq!(store2.local_identity().id, id, "重启后 DeviceId 应不变");
        assert_eq!(store2.local_identity().fingerprint, fp, "指纹应不变");
        let secret2 = store2.load_secret(&keys).unwrap();
        assert_eq!(secret2, secret, "重启后应能取回同一私钥");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_not_in_plaintext_on_disk() {
        let dir = tmpdir("noplain");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("pw-abc");
        let (_store, secret) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "dev", &keys).unwrap();

        // 遍历目录所有文件，私钥明文字节不得出现。
        for entry in fs::read_dir(&dir).unwrap() {
            let bytes = fs::read(entry.unwrap().path()).unwrap();
            assert!(
                !bytes.windows(SECRET_LEN).any(|w| w == secret.0.as_slice()),
                "私钥明文绝不应落盘"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_passphrase_cannot_decrypt() {
        let dir = tmpdir("wrongpw");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("right");
        let (store, _s) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "dev", &keys).unwrap();
        let bad = PassphraseKeyProvider::new("wrong");
        assert!(
            matches!(store.load_secret(&bad), Err(PersistError::DecryptFailed)),
            "错误口令必须解密失败"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_peer_persists_across_reload() {
        let dir = tmpdir("peer");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("pw");
        let (mut store, _s) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "self", &keys).unwrap();
        let (peer, _ps) = create_local_identity(&provider, "friend");
        let pid = peer.id;
        store.remember(peer);
        drop(store);

        let store2 = PersistentIdentityStore::load(&dir).unwrap();
        let found = store2.lookup(&pid).expect("重启后对端应仍在");
        assert_eq!(found.display_name, "friend");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn try_remember_persists_and_is_lookupable() {
        let dir = tmpdir("tryrem");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("pw");
        let (mut store, _s) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "self", &keys).unwrap();
        let (peer, _ps) = create_local_identity(&provider, "friend");
        let pid = peer.id;
        // Result 版：成功应 Ok，且重启后可查。
        store.try_remember(peer).expect("落盘应成功");
        let store2 = PersistentIdentityStore::load(&dir).unwrap();
        assert!(store2.lookup(&pid).is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_store_trait_object_usable() {
        // 验证 PersistentIdentityStore 能像 InMemoryIdentityStore 一样被当作 IdentityStore 用。
        let dir = tmpdir("traitobj");
        let provider = Ed25519CryptoProvider;
        let keys = PassphraseKeyProvider::new("pw");
        let (store, _s) =
            PersistentIdentityStore::load_or_create(&dir, &provider, "self", &keys).unwrap();
        let r: &dyn IdentityStore = &store;
        assert_eq!(r.local_identity().display_name, "self");
        assert!(r.lookup(&[9u8; 16]).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
