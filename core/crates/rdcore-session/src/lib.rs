//! rdcore-session — P4 控制面：把身份/认证接入连接建立（Offer/Answer 签名与验签）。
//!
//! 架构文档 §1/§5：云端只做信令（SDP/ICE），**不碰媒体或控制数据**；对端身份由
//! 控制面在握手期用 Ed25519 验签核验（配合带外指纹核对，防 MITM / 设备冒充）。
//!
//! 本 crate 只负责"握手签名/验签"这一小块控制逻辑，不持有网络：
//! - [`sign_offer`] / [`sign_answer`]：用本地 [`SecretKey`] 对 [`SigningPayload`]
//!   的规范字节签名，写入 `Message` 的 `signature` 字段。
//! - [`verify_offer`] / [`verify_answer`]：从 [`IdentityStore`] 取出对端公钥验签；
//!   通过则返回 [`VerifiedPeer`](含指纹，供 UI 展示 / TOFU 记录)。

use rdcore_crypto::{
    derive_session_key, CryptoProvider, Fingerprint, PublicKey, SecretKey, SessionKey, Signature,
    X25519PublicKey, X25519SecretKey,
};
use rdcore_identity::{DeviceId, IdentityStore, PeerIdentity};
use rdcore_proto::{
    canonical_ephemeral_bytes, canonical_signing_bytes, ConnectionAnswer, ConnectionOffer,
    ProtocolError, SessionId, SessionKeyExchange, SigningPayload,
};

/// 握手校验失败的原因。
#[derive(Debug, Clone, PartialEq)]
pub enum HandshakeError {
    /// 消息没有携带签名（对端未认证）。
    MissingSignature,
    /// 签名与负载不匹配（被篡改，或密钥不匹配 → 防冒充/MITM）。
    InvalidSignature,
    /// 发送方 `from` 不在本端已知对端表里（首次连接未做带外配对）。
    UnknownPeer,
    /// `SessionKeyExchange` 携带的 `session_id` 与当前会话不符（防中继跨会话替换密钥）。
    InvalidSession,
    /// 构造签名负载时序列化失败（理论不会发生）。
    Encode(ProtocolError),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::MissingSignature => write!(f, "握手缺少签名（对端未认证）"),
            HandshakeError::InvalidSignature => write!(f, "签名校验失败（负载被篡改或密钥不匹配）"),
            HandshakeError::UnknownPeer => write!(f, "未知对端（未在带外配对 / 未记住）"),
            HandshakeError::InvalidSession => write!(f, "会话 ID 不匹配（疑似跨会话替换密钥）"),
            HandshakeError::Encode(e) => write!(f, "签名负载序列化失败: {e}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// 验签通过后得到的、已确认的对端身份快照。
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedPeer {
    /// 设备 ID（信令层面 `from`）。
    pub id: DeviceId,
    /// 展示名（来自已认证的 PeerIdentity，供不可伪造横幅使用）。
    pub display_name: String,
    /// 公钥（已用于验签）。
    pub public_key: PublicKey,
    /// 公钥指纹（应带外展示给用户核对，防 MITM）。
    pub fingerprint: Fingerprint,
}

/// 用本地私钥给 Offer 签名，返回带 `signature` 的副本。
///
/// 签名覆盖 `SigningPayload { session_id, from, sdp }` 的规范字节
/// （见 [`rdcore_proto::canonical_signing_bytes`]），**绝不覆盖 `signature` 自身**。
pub fn sign_offer(
    provider: &impl CryptoProvider,
    secret: &SecretKey,
    mut offer: ConnectionOffer,
) -> ConnectionOffer {
    let bytes =
        canonical_signing_bytes(&offer.signing_payload()).expect("SigningPayload 必须可序列化");
    offer.signature = Some(provider.sign(secret, &bytes));
    offer
}

/// 用本地私钥给 Answer 签名，返回带 `signature` 的副本（语义同 [`sign_offer`]）。
pub fn sign_answer(
    provider: &impl CryptoProvider,
    secret: &SecretKey,
    mut answer: ConnectionAnswer,
) -> ConnectionAnswer {
    let bytes =
        canonical_signing_bytes(&answer.signing_payload()).expect("SigningPayload 必须可序列化");
    answer.signature = Some(provider.sign(secret, &bytes));
    answer
}

/// 验签收到的 Offer：从 `store` 取 `from` 对应公钥，校验其对 `SigningPayload` 规范字节
/// 的签名。通过返回 [`VerifiedPeer`]，失败返回 [`HandshakeError`]（调用方据此拒绝连接）。
pub fn verify_offer(
    provider: &impl CryptoProvider,
    store: &dyn IdentityStore,
    offer: &ConnectionOffer,
) -> Result<VerifiedPeer, HandshakeError> {
    verify(
        provider,
        store,
        &offer.from,
        &offer.signing_payload(),
        offer.signature.as_ref(),
    )
}

/// 验签收到的 Answer（语义同 [`verify_offer`]）。
pub fn verify_answer(
    provider: &impl CryptoProvider,
    store: &dyn IdentityStore,
    answer: &ConnectionAnswer,
) -> Result<VerifiedPeer, HandshakeError> {
    verify(
        provider,
        store,
        &answer.from,
        &answer.signing_payload(),
        answer.signature.as_ref(),
    )
}

/// 对本次会话的 X25519 临时公钥签名，产出可经信令传输的 [`rdcore_proto::SessionKeyExchange`]。
///
/// 把临时密钥绑定到 P4 已认证的 Ed25519 身份：签名覆盖 `session_id || from || ephemeral`。
pub fn sign_ephemeral_key(
    provider: &impl CryptoProvider,
    secret: &SecretKey,
    session_id: SessionId,
    from: DeviceId,
    ephemeral: [u8; 32],
) -> SessionKeyExchange {
    let bytes =
        canonical_ephemeral_bytes(session_id, from, ephemeral).expect("临时公钥负载必须可序列化");
    SessionKeyExchange {
        session_id,
        from,
        ephemeral,
        signature: Some(provider.sign(secret, &bytes)),
    }
}

/// 验签对端的 [`rdcore_proto::SessionKeyExchange`] 并派生端到端会话密钥。
///
/// 先校验消息里的 `session_id` 与 `expected_session_id` 一致（**会话隔离**：防中继
/// 把另一会话的 `SessionKeyExchange` 注入本会话，导致两端派生出串台/重用的密钥），
/// 再复用 P4 的验签逻辑（查 store 取 `from` 公钥、核对 Ed25519 签名），通过后用本端
/// X25519 私钥与对端临时公钥做 ECDH，得到两端一致的 `SessionKey`。
pub fn establish_session_key(
    provider: &impl CryptoProvider,
    store: &dyn IdentityStore,
    our_secret: &X25519SecretKey,
    their: &SessionKeyExchange,
    expected_session_id: SessionId,
) -> Result<SessionKey, HandshakeError> {
    if their.session_id != expected_session_id {
        return Err(HandshakeError::InvalidSession);
    }
    let sig = their
        .signature
        .as_ref()
        .ok_or(HandshakeError::MissingSignature)?;
    let peer = store
        .lookup(&their.from)
        .ok_or(HandshakeError::UnknownPeer)?;
    let bytes = canonical_ephemeral_bytes(their.session_id, their.from, their.ephemeral)
        .map_err(HandshakeError::Encode)?;
    if !provider.verify(&peer.public_key, &bytes, sig) {
        return Err(HandshakeError::InvalidSignature);
    }
    let their_pk = X25519PublicKey::from(their.ephemeral);
    Ok(derive_session_key(our_secret, &their_pk))
}

/// 验签公共逻辑：查公钥 → 序列化负载 → Ed25519 验签。
fn verify(
    provider: &impl CryptoProvider,
    store: &dyn IdentityStore,
    from: &DeviceId,
    payload: &SigningPayload,
    signature: Option<&Signature>,
) -> Result<VerifiedPeer, HandshakeError> {
    let sig = signature.ok_or(HandshakeError::MissingSignature)?;
    let peer: &PeerIdentity = store.lookup(from).ok_or(HandshakeError::UnknownPeer)?;
    let bytes = canonical_signing_bytes(payload).map_err(HandshakeError::Encode)?;
    if provider.verify(&peer.public_key, &bytes, sig) {
        // 指纹必须由已验签的公钥**现算**，绝不信任 store 里可能过期/被投毒的副本
        // （见 rdcore-crypto 的约定：fingerprint 永远派生，不独立存储）。
        Ok(VerifiedPeer {
            id: peer.id,
            display_name: peer.display_name.clone(),
            public_key: peer.public_key.clone(),
            fingerprint: provider.fingerprint(&peer.public_key),
        })
    } else {
        Err(HandshakeError::InvalidSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::{ephemeral_x25519_keypair, x25519_public_bytes, Ed25519CryptoProvider};
    use rdcore_identity::{create_local_identity, InMemoryIdentityStore};
    use rdcore_proto::{Capabilities, ConnectionOffer, SessionId, SessionKeyExchange, VideoCodec};

    fn offer(from: DeviceId, sdp: &str) -> ConnectionOffer {
        ConnectionOffer {
            session_id: SessionId([1u8; 16]),
            from,
            sdp: sdp.to_string(),
            capabilities: Capabilities {
                video_codecs: vec![VideoCodec::Raw],
                max_width: 1920,
                max_height: 1080,
                fps: 30,
                clipboard: true,
                input: rdcore_proto::InputCaps {
                    mouse: true,
                    keyboard: true,
                    wheel: true,
                },
            },
            frame: None,
            signature: None,
        }
    }

    #[test]
    fn sign_verify_roundtrip_succeeds() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");

        // Host 预先在带外（扫码）记住了 Viewer 的公钥。
        let mut host_store = InMemoryIdentityStore::new(host);
        host_store.remember(viewer.clone());

        let signed = sign_offer(&provider, &viewer_sk, offer(viewer.id, "v=0..."));
        let verified = verify_offer(&provider, &host_store, &signed).expect("验签应通过");
        assert_eq!(verified.id, viewer.id);
        assert_eq!(verified.fingerprint, viewer.fingerprint);
        assert_eq!(verified.public_key, viewer.public_key);
    }

    #[test]
    fn rejects_missing_signature() {
        let provider = Ed25519CryptoProvider;
        let (host, _) = create_local_identity(&provider, "host");
        let store = InMemoryIdentityStore::new(host);
        let unsigned = offer([9u8; 16], "v=0...");
        assert_eq!(
            verify_offer(&provider, &store, &unsigned),
            Err(HandshakeError::MissingSignature)
        );
    }

    #[test]
    fn rejects_unknown_peer() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        // Host 不认识 Viewer（没做带外配对）。
        let store = InMemoryIdentityStore::new(host);
        let signed = sign_offer(&provider, &viewer_sk, offer(viewer.id, "v=0..."));
        assert_eq!(
            verify_offer(&provider, &store, &signed),
            Err(HandshakeError::UnknownPeer)
        );
    }

    #[test]
    fn rejects_tampered_sdp() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _) = create_local_identity(&provider, "host");
        let mut store = InMemoryIdentityStore::new(host);
        store.remember(viewer.clone());

        let mut signed = sign_offer(&provider, &viewer_sk, offer(viewer.id, "v=0..."));
        signed.sdp = "v=0... EVIL".to_string(); // 改了被签内容
        assert_eq!(
            verify_offer(&provider, &store, &signed),
            Err(HandshakeError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_tampered_capabilities() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        let mut store = InMemoryIdentityStore::new(host);
        store.remember(viewer.clone());

        let mut signed = sign_offer(&provider, &viewer_sk, offer(viewer.id, "v=0..."));
        // 篡改能力协商：偷偷关掉剪贴板 / 鼠标输入 → 签名应失败
        // （这正是 P4 把 capabilities 纳入签名要防的"中间人静默降级"）。
        signed.capabilities.clipboard = false;
        signed.capabilities.input.mouse = false;
        assert_eq!(
            verify_offer(&provider, &store, &signed),
            Err(HandshakeError::InvalidSignature)
        );
    }

    #[test]
    fn rejects_wrong_key_impersonation() {
        let provider = Ed25519CryptoProvider;
        let (viewer, _viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        let (mallory, mallory_sk) = create_local_identity(&provider, "mallory");
        let mut store = InMemoryIdentityStore::new(host);
        // Host 记住的是 Viewer 的公钥；Mallory 试图冒用 Viewer 的 `from`。
        store.remember(viewer.clone());

        let forged = sign_offer(&provider, &mallory_sk, offer(viewer.id, "v=0..."));
        assert_eq!(
            verify_offer(&provider, &store, &forged),
            Err(HandshakeError::InvalidSignature)
        );
        // Mallory 用自己的 from 但 Host 不认识 → 同样是 UnknownPeer。
        let mallory_offer = sign_offer(&provider, &mallory_sk, offer(mallory.id, "v=0..."));
        assert_eq!(
            verify_offer(&provider, &store, &mallory_offer),
            Err(HandshakeError::UnknownPeer)
        );
    }

    #[test]
    fn session_key_handshake_both_ends_equal() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, host_sk) = create_local_identity(&provider, "host");
        let mut host_store = InMemoryIdentityStore::new(host.clone());
        host_store.remember(viewer.clone());
        let mut viewer_store = InMemoryIdentityStore::new(viewer.clone());
        viewer_store.remember(host.clone());

        let sid = SessionId([1u8; 16]);
        let (v_pub, v_sec) = ephemeral_x25519_keypair();
        let v_ex = sign_ephemeral_key(
            &provider,
            &viewer_sk,
            sid,
            viewer.id,
            x25519_public_bytes(&v_pub),
        );
        let (h_pub, h_sec) = ephemeral_x25519_keypair();
        let h_ex = sign_ephemeral_key(
            &provider,
            &host_sk,
            sid,
            host.id,
            x25519_public_bytes(&h_pub),
        );

        // 两端各自用"自己的 X25519 私钥 + 对方临时公钥"派生，必须相同。
        let key_host_side =
            establish_session_key(&provider, &host_store, &h_sec, &v_ex, sid).unwrap();
        let key_viewer_side =
            establish_session_key(&provider, &viewer_store, &v_sec, &h_ex, sid).unwrap();
        assert_eq!(key_host_side, key_viewer_side, "两端应派生相同会话密钥");
    }

    #[test]
    fn session_key_rejects_tampered_ephemeral() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        let mut host_store = InMemoryIdentityStore::new(host.clone());
        host_store.remember(viewer.clone());

        let (v_pub, v_sec) = ephemeral_x25519_keypair();
        let mut v_ex = sign_ephemeral_key(
            &provider,
            &viewer_sk,
            SessionId([1u8; 16]),
            viewer.id,
            x25519_public_bytes(&v_pub),
        );
        // 篡改临时公钥 → 验签失败（防 MITM 替换密钥）
        v_ex.ephemeral = [9u8; 32];
        assert_eq!(
            establish_session_key(&provider, &host_store, &v_sec, &v_ex, SessionId([1u8; 16])),
            Err(HandshakeError::InvalidSignature)
        );
    }

    #[test]
    fn session_key_rejects_unknown_peer() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        // Host 不认识 Viewer（没做带外配对）
        let host_store = InMemoryIdentityStore::new(host.clone());

        let (v_pub, v_sec) = ephemeral_x25519_keypair();
        let v_ex = sign_ephemeral_key(
            &provider,
            &viewer_sk,
            SessionId([1u8; 16]),
            viewer.id,
            x25519_public_bytes(&v_pub),
        );
        assert_eq!(
            establish_session_key(&provider, &host_store, &v_sec, &v_ex, SessionId([1u8; 16])),
            Err(HandshakeError::UnknownPeer)
        );
    }

    #[test]
    fn session_key_rejects_missing_signature() {
        let provider = Ed25519CryptoProvider;
        let (viewer, _viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        let host_store = InMemoryIdentityStore::new(host.clone());

        let (_v_pub, v_sec) = ephemeral_x25519_keypair();
        let v_ex = SessionKeyExchange {
            session_id: SessionId([1u8; 16]),
            from: viewer.id,
            ephemeral: [2u8; 32],
            signature: None,
        };
        assert_eq!(
            establish_session_key(&provider, &host_store, &v_sec, &v_ex, SessionId([1u8; 16])),
            Err(HandshakeError::MissingSignature)
        );
    }

    #[test]
    fn session_key_rejects_wrong_session_id() {
        let provider = Ed25519CryptoProvider;
        let (viewer, viewer_sk) = create_local_identity(&provider, "viewer");
        let (host, _host_sk) = create_local_identity(&provider, "host");
        let mut host_store = InMemoryIdentityStore::new(host.clone());
        host_store.remember(viewer.clone());

        let (v_pub, v_sec) = ephemeral_x25519_keypair();
        // 合法签名，但会话 ID 属于"另一会话"
        let v_ex = sign_ephemeral_key(
            &provider,
            &viewer_sk,
            SessionId([1u8; 16]),
            viewer.id,
            x25519_public_bytes(&v_pub),
        );
        // 本端期望的会话 ID 不同 → 必须拒绝（防中继跨会话替换密钥）
        assert_eq!(
            establish_session_key(&provider, &host_store, &v_sec, &v_ex, SessionId([2u8; 16])),
            Err(HandshakeError::InvalidSession)
        );
        // 会话 ID 一致 → 通过
        assert!(
            establish_session_key(&provider, &host_store, &v_sec, &v_ex, SessionId([1u8; 16]))
                .is_ok()
        );
    }
}
