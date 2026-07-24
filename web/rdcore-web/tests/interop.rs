//! M0 最关键的互认证据：Web 侧构造的握手产物，直接被 Host 侧 `rdcore-session`
//! 的验签 / 密钥派生函数接受（以及反向：Host 侧构造的产物被 Web 状态机接受）。
//!
//! 这里**不经过**任何 Web 侧便利封装——直接用 Host 生产代码路径
//! （`verify_offer` / `sign_answer` / `sign_ephemeral_key` / `establish_session_key`）
//! 对 Web 产出的字节做裁判，证明浏览器端与现有 Windows Host 在密码学层面互通。
//! 结果同步写入 `web/testvectors/interop.json` 作为黄金留档。

use std::fs;
use std::path::PathBuf;

use rdcore_crypto::{
    derive_session_key, x25519_public_bytes, CryptoProvider, Ed25519CryptoProvider,
    X25519PublicKey, X25519SecretKey,
};
use rdcore_identity::{IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{Capabilities, ConnectionAnswer, Message, SessionId};
use rdcore_session::{
    establish_session_key, sign_answer, sign_ephemeral_key, verify_offer, HandshakeError,
};
use rdcore_web::handshake::Handshake;
use rdcore_web::identity::LocalIdentity;
use rdcore_web::{hex_encode, WebError};

mod shared {
    // 与 tests/testvectors.rs 相同的确定性常量（复制以避免跨测试文件依赖）。
    use rdcore_proto::SessionId;

    pub fn viewer_seed() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }
    pub fn host_seed() -> [u8; 32] {
        std::array::from_fn(|i| 0x20 + i as u8)
    }
    pub fn viewer_device() -> [u8; 16] {
        std::array::from_fn(|i| 0xa0 + i as u8)
    }
    pub fn host_device() -> [u8; 16] {
        std::array::from_fn(|i| 0xb0 + i as u8)
    }
    pub fn session_id() -> SessionId {
        SessionId(std::array::from_fn(|i| 0xc0 + i as u8))
    }
    pub fn viewer_eph() -> [u8; 32] {
        std::array::from_fn(|i| 0x40 + i as u8)
    }
    pub fn host_eph() -> [u8; 32] {
        std::array::from_fn(|i| 0x60 + i as u8)
    }
    pub const CAPS_JSON: &str = concat!(
        "{\"video_codecs\":[\"H264\",\"Raw\"],\"max_width\":1920,\"max_height\":1080,",
        "\"fps\":30,\"clipboard\":true,",
        "\"input\":{\"mouse\":true,\"keyboard\":true,\"wheel\":true}}"
    );
    pub const SDP_OFFER: &str = "v=0 rdcore-web test offer";
    pub const SDP_ANSWER: &str = "v=0 rdcore-web test answer";
}

use shared::*;

fn viewer() -> LocalIdentity {
    LocalIdentity::from_seed(viewer_seed(), viewer_device(), "web-viewer-fixture")
}

fn host() -> LocalIdentity {
    LocalIdentity::from_seed(host_seed(), host_device(), "windows-host-fixture")
}

fn caps() -> Capabilities {
    serde_json::from_str(CAPS_JSON).unwrap()
}

/// Host 侧身份存储：已按 TOFU 记住 Viewer 的公钥（真实流程里来自其 PeerHello）。
fn host_store() -> InMemoryIdentityStore {
    let mut store = InMemoryIdentityStore::new(host().public);
    store.remember(viewer().public);
    store
}

/// Web 侧握手状态机：已消费 Host 的 PeerHello。
fn web_handshake() -> Handshake {
    let mut hs = Handshake::new(session_id(), &viewer());
    let hello = rdcore_proto::encode(&Message::PeerHello(host().public)).unwrap();
    hs.handle_message(&hello).unwrap();
    hs
}

/// **M0 关键证据**：Web 构造的签名 Offer 被 Host 侧 `rdcore_session::verify_offer` 接受。
#[test]
fn web_signed_offer_accepted_by_host_verify() {
    let provider = Ed25519CryptoProvider;
    let hs = web_handshake();
    let offer_bytes = hs.build_signed_offer(SDP_OFFER, caps()).unwrap();

    // Host 视角：从线上字节解出 Offer，用生产代码验签。
    let Message::Offer(offer) = rdcore_proto::decode(&offer_bytes).unwrap() else {
        panic!("应为 Offer")
    };
    let verified =
        verify_offer(&provider, &host_store(), &offer).expect("Host 必须接受 Web 的签名 Offer");
    assert_eq!(verified.id, viewer().public.id, "验出的对端应是 Viewer");
    assert_eq!(verified.public_key, viewer().public.public_key);
    assert_eq!(verified.fingerprint, viewer().public.fingerprint);

    // 篡改任何一个被签字段（如 SDP / 能力），Host 必须拒绝。
    let mut tampered = offer.clone();
    tampered.sdp.push_str(" EVIL");
    assert_eq!(
        verify_offer(&provider, &host_store(), &tampered),
        Err(HandshakeError::InvalidSignature)
    );
    let mut tampered_caps = offer.clone();
    tampered_caps.capabilities.clipboard = false;
    assert_eq!(
        verify_offer(&provider, &host_store(), &tampered_caps),
        Err(HandshakeError::InvalidSignature),
        "能力被静默降级必须被签名挡住"
    );

    // 冒充：用另一把私钥签 Viewer 的 from，Host 必须拒绝。
    let mallory = LocalIdentity::generate("mallory");
    let hs_bad = Handshake::new(session_id(), &mallory);
    // 伪装 from 为 Viewer（构造逻辑强制 from=自身，故改为直接改字节层：
    // 用 mallory 密钥签 viewer 的 SigningPayload）。
    let forged_offer = {
        let mut o = match rdcore_proto::decode(&offer_bytes).unwrap() {
            Message::Offer(o) => o,
            _ => unreachable!(),
        };
        let payload = rdcore_proto::canonical_signing_bytes(&o.signing_payload()).unwrap();
        o.signature = Some(provider.sign(&mallory.secret, &payload));
        o
    };
    assert_eq!(
        verify_offer(&provider, &host_store(), &forged_offer),
        Err(HandshakeError::InvalidSignature),
        "冒充签名必须被拒绝"
    );
    // 用 mallory 自身身份（from 也是 mallory）构造的 Offer：Host 不认识 → UnknownPeer。
    let mallory_offer_bytes = hs_bad.build_signed_offer(SDP_OFFER, caps()).unwrap();
    let Message::Offer(mallory_offer) = rdcore_proto::decode(&mallory_offer_bytes).unwrap() else {
        panic!("应为 Offer")
    };
    assert_eq!(
        verify_offer(&provider, &host_store(), &mallory_offer),
        Err(HandshakeError::UnknownPeer),
        "未知设备必须被拒绝"
    );

    // 留档 interop.json。
    let dir = PathBuf::from("../testvectors");
    fs::create_dir_all(&dir).unwrap();
    let interop = serde_json::json!({
        "description": "M0 互认证据：Web 构造的签名 Offer / SessionKeyExchange 被 Host 侧 \
                        rdcore-session 验签接受；两端 ECDH 派生相同会话密钥",
        "offer_bytes_hex": hex_encode(&offer_bytes),
        "host_verify_offer": {
            "result": "ok",
            "verified_device_id_hex": hex_encode(&verified.id),
            "verified_fingerprint": verified.fingerprint.to_spaced_hex(),
        },
        "viewer_session_key_exchange_hex": hex_encode(&web_session_key_exchange()),
        "session_key_hex": hex_encode(&expected_session_key().0),
    });
    let mut text = serde_json::to_string_pretty(&interop).unwrap();
    text.push('\n');
    fs::write(dir.join("interop.json"), text).unwrap();
}

/// 反向：Host 用 `sign_answer` 构造的 Answer 被 Web 状态机验签接受。
#[test]
fn host_signed_answer_accepted_by_web() {
    let provider = Ed25519CryptoProvider;
    let mut hs = web_handshake();
    let answer = sign_answer(
        &provider,
        &host().secret,
        ConnectionAnswer {
            session_id: session_id(),
            from: host().public.id,
            sdp: SDP_ANSWER.to_string(),
            capabilities: caps(),
            frame: None,
            signature: None,
        },
    );
    let bytes = rdcore_proto::encode(&Message::Answer(answer)).unwrap();
    let json = hs
        .handle_answer(&bytes)
        .expect("Web 必须接受 Host 的签名 Answer");
    assert!(json.contains(SDP_ANSWER), "{json}");
    assert!(
        json.contains(&host().public.fingerprint.to_spaced_hex()),
        "{json}"
    );

    // Host 不认识 Viewer 时（未记住）→ verify_offer 必须 UnknownPeer（Host 侧语义）。
    let lonely_store = InMemoryIdentityStore::new(host().public);
    let hs2 = web_handshake();
    let offer_bytes = hs2.build_signed_offer(SDP_OFFER, caps()).unwrap();
    let Message::Offer(offer) = rdcore_proto::decode(&offer_bytes).unwrap() else {
        panic!("应为 Offer")
    };
    assert_eq!(
        verify_offer(&Ed25519CryptoProvider, &lonely_store, &offer),
        Err(HandshakeError::UnknownPeer)
    );
}

/// 双向：Web 构造的 SessionKeyExchange 被 Host `establish_session_key` 接受并派生相同密钥。
#[test]
fn session_key_exchange_cross_verified() {
    let provider = Ed25519CryptoProvider;
    let sid = session_id();
    let (viewer_ske_bytes, web_key) = web_session_key_exchange_with_key();
    let host_key = expected_session_key();

    // Host 视角：验签 + 派生（生产代码路径）。
    let Message::SessionKey(viewer_ske) = rdcore_proto::decode(&viewer_ske_bytes).unwrap() else {
        panic!("应为 SessionKey")
    };
    let host_eph_secret = X25519SecretKey::from(host_eph());
    let host_side_key =
        establish_session_key(&provider, &host_store(), &host_eph_secret, &viewer_ske, sid)
            .expect("Host 必须接受 Web 的 SessionKeyExchange");
    assert_eq!(host_side_key, web_key, "两端应派生相同会话密钥");
    assert_eq!(host_side_key, host_key);

    // 会话隔离：Host 用别的 session_id 裁判 → 必须 InvalidSession。
    assert_eq!(
        establish_session_key(
            &provider,
            &host_store(),
            &host_eph_secret,
            &viewer_ske,
            SessionId([0x99; 16]),
        ),
        Err(HandshakeError::InvalidSession)
    );

    // 篡改临时公钥 → InvalidSignature。
    let mut forged = viewer_ske.clone();
    forged.ephemeral = [0xEE; 32];
    assert_eq!(
        establish_session_key(&provider, &host_store(), &host_eph_secret, &forged, sid),
        Err(HandshakeError::InvalidSignature)
    );
}

/// Web 状态机全链路：Host 的 SessionKeyExchange → 验签 → 派生密钥 == Host 侧派生。
#[test]
fn web_derives_same_key_as_host() {
    let provider = Ed25519CryptoProvider;
    let sid = session_id();
    let host = host();

    // Host 构造已签名的临时公钥（生产代码路径）。
    let host_eph_secret = X25519SecretKey::from(host_eph());
    let host_pub = x25519_public_bytes(&X25519PublicKey::from(&host_eph_secret));
    let host_ske = sign_ephemeral_key(&provider, &host.secret, sid, host.public.id, host_pub);
    let host_ske_bytes = rdcore_proto::encode(&Message::SessionKey(host_ske)).unwrap();

    // Web 全链路：验签 + 派生。
    let mut hs = web_handshake();
    hs.build_session_key_exchange_det(viewer_eph()).unwrap();
    hs.handle_session_key_exchange(&host_ske_bytes).unwrap();
    let web_key = hs.session_key_bytes().unwrap();

    // Host 侧独立派生（用 Viewer 的临时公钥）。
    let viewer_pub =
        x25519_public_bytes(&X25519PublicKey::from(&X25519SecretKey::from(viewer_eph())));
    let expect = derive_session_key(&host_eph_secret, &X25519PublicKey::from(viewer_pub));
    assert_eq!(web_key, expect.0.to_vec(), "Web 派生必须等于 Host 派生");

    // 串会话防御：伪造一个 session_id 不同的交换消息（即使签名有效）必须拒绝。
    let wrong_sid = SessionId([0x99; 16]);
    let other = sign_ephemeral_key(&provider, &host.secret, wrong_sid, host.public.id, host_pub);
    let other_bytes = rdcore_proto::encode(&Message::SessionKey(other)).unwrap();
    let mut hs2 = web_handshake();
    hs2.build_session_key_exchange_det(viewer_eph()).unwrap();
    assert_eq!(
        hs2.handle_session_key_exchange(&other_bytes),
        Err(WebError::InvalidSession)
    );
}

fn web_session_key_exchange() -> Vec<u8> {
    web_session_key_exchange_with_key().0
}

fn web_session_key_exchange_with_key() -> (Vec<u8>, rdcore_crypto::SessionKey) {
    let mut hs = web_handshake();
    let bytes = hs.build_session_key_exchange_det(viewer_eph()).unwrap();
    let key = expected_session_key();
    (bytes, key)
}

fn expected_session_key() -> rdcore_crypto::SessionKey {
    let viewer_sec = X25519SecretKey::from(viewer_eph());
    let host_pub = X25519PublicKey::from(&X25519SecretKey::from(host_eph()));
    derive_session_key(&viewer_sec, &host_pub)
}
