//! rdcore-identity — 每设备身份类型与 `IdentityStore` 接口。
//!
//! P0 只定义数据类型与 trait 边界（契约）。**P4 起提供具体辅助**：
//!
//! - [`create_local_identity`]：用 [`CryptoProvider`] 生成密钥对 + 随机 `DeviceId` +
//!   由公钥派生指纹，得到本设备身份。
//! - [`InMemoryIdentityStore`]：实现 [`IdentityStore`] 的内存实现（本地/测试用，
//!   真实环境换成 OS 钥匙串 / TPM）。
//!
//! 纯 Rust。

use rdcore_crypto::{CryptoProvider, Fingerprint, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod persist;
pub use persist::{KeyProvider, PassphraseKeyProvider, PersistError, PersistentIdentityStore};

/// 稳定的每设备标识符（16 个随机字节）。作为信令消息线上级别的 `from` 字段。
///
/// 注：目前是类型别名 `type`；评审建议（F5）后续可改为 newtype
/// `struct DeviceId(pub [u8; 16])` 以提升类型安全，避免与其它 `[u8;16]` 混用。
pub type DeviceId = [u8; 16];

/// 一个已认知对端的、经过核验的身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    /// 设备 ID。
    pub id: DeviceId,
    /// 展示名（给人看的昵称）。
    pub display_name: String,
    /// 公钥。
    pub public_key: PublicKey,
    /// 公钥指纹（应由 `CryptoProvider::fingerprint` 派生，绝不可独立信任/存储）。
    pub fingerprint: Fingerprint,
}

/// 本地身份 + 已记住对端身份的存储边界（trait）。
///
/// 真正的实现在后续里程碑落地（文件系统 / OS 钥匙串 / TPM）。
/// 协议层只依赖这个 trait，不直接依赖具体存储。
pub trait IdentityStore {
    /// 返回本设备自身的身份。
    fn local_identity(&self) -> &PeerIdentity;
    /// 持久化一个我们交互过的对端。
    fn remember(&mut self, peer: PeerIdentity);
    /// 按 [`DeviceId`] 查找一个之前见过的对端。
    fn lookup(&self, id: &DeviceId) -> Option<&PeerIdentity>;
}

/// 用 [`CryptoProvider`] 生成本设备的身份。
///
/// - 生成 Ed25519 密钥对（`SecretKey` 留在调用方，**绝不序列化**）。
/// - 生成 16 字节随机 `DeviceId` 作为信令层面的 `from`。
/// - 由公钥派生 SHA-256 [`Fingerprint`]，供带外（电话/扫码）展示给用户做人工核验。
///
/// 返回 `(PeerIdentity, SecretKey)`：前者可存进 [`IdentityStore`]，后者必须安全保存
/// （本进程内），用于后续对 Offer/Answer 签名。
pub fn create_local_identity(
    provider: &impl CryptoProvider,
    display_name: &str,
) -> (PeerIdentity, SecretKey) {
    let (public_key, secret) = provider.generate_keypair();
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).expect("系统随机数不可用，无法生成 DeviceId");
    let fingerprint = provider.fingerprint(&public_key);
    let identity = PeerIdentity {
        id,
        display_name: display_name.to_string(),
        public_key,
        fingerprint,
    };
    (identity, secret)
}

/// [`IdentityStore`] 的内存实现（本地 / 测试用）。
///
/// 持有本设备身份 `self_identity`（`local_identity()` 返回它）以及已记住对端的
/// `peers` 表。带外配对（如扫码）得到的对端用 [`IdentityStore::remember`] 存入；
/// 握手时 [`IdentityStore::lookup`] 取出公钥来验签。
///
/// 派生 `Clone` 以便 FFI 会话（`rdcore-ffi`）在创建时复制一份本机身份存储，
/// 而长期身份（`RdLocal`）继续保留原始副本供后续会话复用。
#[derive(Clone)]
pub struct InMemoryIdentityStore {
    self_identity: PeerIdentity,
    peers: HashMap<DeviceId, PeerIdentity>,
}

impl InMemoryIdentityStore {
    /// 以本设备身份初始化一个空的对端表。
    pub fn new(self_identity: PeerIdentity) -> Self {
        Self {
            self_identity,
            peers: HashMap::new(),
        }
    }
}

impl IdentityStore for InMemoryIdentityStore {
    fn local_identity(&self) -> &PeerIdentity {
        &self.self_identity
    }

    fn remember(&mut self, peer: PeerIdentity) {
        self.peers.insert(peer.id, peer);
    }

    fn lookup(&self, id: &DeviceId) -> Option<&PeerIdentity> {
        if &self.self_identity.id == id {
            return Some(&self.self_identity);
        }
        self.peers.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::Ed25519CryptoProvider;

    #[test]
    fn local_identity_stores_and_looks_up_peer() {
        let provider = Ed25519CryptoProvider;
        let (self_id, _self_sk) = create_local_identity(&provider, "my-laptop");
        let (peer_id, _peer_sk) = create_local_identity(&provider, "friend-phone");

        // 两个身份应不同。
        assert_ne!(self_id.id, peer_id.id);
        assert_ne!(self_id.fingerprint, peer_id.fingerprint);

        let mut store = InMemoryIdentityStore::new(self_id);
        assert_eq!(store.local_identity().display_name, "my-laptop");
        // 自己也能被 lookup 命中。
        assert!(store.lookup(&store.local_identity().id).is_some());

        // 记住对端后可查到，且公钥/指纹一致。
        store.remember(peer_id.clone());
        let found = store.lookup(&peer_id.id).expect("对端应被记住");
        assert_eq!(found.public_key, peer_id.public_key);
        assert_eq!(found.fingerprint, peer_id.fingerprint);

        // 未知对端查不到。
        assert!(store.lookup(&[7u8; 16]).is_none());
    }
}
