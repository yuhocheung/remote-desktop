//! Viewer 侧握手状态机（WASM facade：`WebHandshake`）。
//!
//! 镜像 `rdcore-app::Connection::establish` 的 Viewer 分支与 `rdcore-session` 的
//! 签名 / 验签 / 密钥派生逻辑：
//!
//! 1. `peer_hello`：广播本端公开身份（`Message::PeerHello`，只含公开信息）。
//! 2. `build_signed_offer`：构造 `ConnectionOffer` 并对 `SigningPayload`
//!    （session_id ‖ from ‖ sdp ‖ capabilities ‖ frame）的规范字节做 Ed25519 签名，
//!    与 `rdcore_session::sign_offer` 逐字节一致。
//! 3. `handle_answer`：验签 Host 的 Answer（公钥取自其 PeerHello，TOFU 记住），
//!    通过后返回对端 SDP / capabilities。
//! 4. `build_session_key_exchange` / `handle_session_key_exchange`：交换已签名的
//!    X25519 临时公钥（签名覆盖 session_id ‖ from ‖ ephemeral），ECDH + SHA-256
//!    派生 32 字节会话密钥。
//!
//! 与 `rdcore-session` 的互认由 `tests/` 下的黄金测试向量直接调用其
//! `verify_offer` / `establish_session_key` 交叉证明（M0 关键交付物）。

use std::collections::HashMap;

use rdcore_crypto::{
    derive_session_key, ephemeral_x25519_keypair, x25519_public_bytes, CryptoProvider,
    Ed25519CryptoProvider, SessionKey, X25519PublicKey, X25519SecretKey,
};
use rdcore_identity::{DeviceId, PeerIdentity};
use rdcore_proto::{
    canonical_ephemeral_bytes, canonical_signing_bytes, decode_limited, encode, Capabilities,
    ConnectionAnswer, ConnectionOffer, IceCandidate, Message, SessionId, SessionKeyExchange,
    MAX_SIGNALING_MESSAGE_LEN,
};
use wasm_bindgen::prelude::*;

use crate::identity::{self, LocalIdentity};
use crate::{hex_decode_array, hex_encode, WebError};

/// 核心状态机（纯 Rust，便于原生单测；wasm facade 只是薄封装）。
pub struct Handshake {
    session_id: SessionId,
    own: PeerIdentity,
    secret: rdcore_crypto::SecretKey,
    /// TOFU 记住的对端（来自其 PeerHello；镜像 `Connection::remember_peer_tofu`：
    /// 首个版本生效，后续同名 PeerHello 不覆盖）。
    peers: HashMap<DeviceId, PeerIdentity>,
    eph_secret: Option<X25519SecretKey>,
    session_key: Option<SessionKey>,
}

impl Handshake {
    /// 以本会话 ID 与本机身份初始化。
    pub fn new(session_id: SessionId, identity: &LocalIdentity) -> Self {
        Self {
            session_id,
            own: identity.public.clone(),
            secret: identity.secret.clone(),
            peers: HashMap::new(),
            eph_secret: None,
            session_key: None,
        }
    }

    /// postcard 编码的 `Message::PeerHello`（配对身份广播，不含任何密钥材料）。
    pub fn peer_hello(&self) -> Result<Vec<u8>, WebError> {
        Ok(encode(&Message::PeerHello(self.own.clone()))?)
    }

    /// 构造已签名的 Offer（镜像 `rdcore_session::sign_offer`）。
    pub fn build_signed_offer(
        &self,
        sdp: &str,
        capabilities: Capabilities,
    ) -> Result<Vec<u8>, WebError> {
        let mut offer = ConnectionOffer {
            session_id: self.session_id,
            from: self.own.id,
            sdp: sdp.to_string(),
            capabilities,
            frame: None,
            signature: None,
        };
        let payload = canonical_signing_bytes(&offer.signing_payload())?;
        offer.signature = Some(Ed25519CryptoProvider.sign(&self.secret, &payload));
        Ok(encode(&Message::Offer(offer))?)
    }

    /// 验签 Host 的 Answer：公钥按 `from` 从 TOFU 对端表取（须先收到其 PeerHello）。
    /// 通过返回 JSON：`{sdp, capabilities, frame, from_device_id_hex, display_name,
    /// public_key_hex, fingerprint}`；失败返回 `Err`（缺签 / 未知对端 / 验签失败 / 串会话）。
    pub fn handle_answer(&mut self, bytes: &[u8]) -> Result<String, WebError> {
        let msg = decode_limited(bytes, MAX_SIGNALING_MESSAGE_LEN)?;
        let Message::Answer(answer) = msg else {
            return Err(WebError::Protocol("期望 Message::Answer".into()));
        };
        if answer.session_id != self.session_id {
            return Err(WebError::InvalidSession);
        }
        let peer = self.peers.get(&answer.from).ok_or(WebError::UnknownPeer)?;
        let sig = answer
            .signature
            .as_ref()
            .ok_or(WebError::MissingSignature)?;
        let payload = canonical_signing_bytes(&answer.signing_payload())?;
        if !Ed25519CryptoProvider.verify(&peer.public_key, &payload, sig) {
            return Err(WebError::InvalidSignature);
        }
        Ok(Self::answer_json(&answer, peer))
    }

    fn answer_json(answer: &ConnectionAnswer, peer: &PeerIdentity) -> String {
        serde_json::json!({
            "sdp": answer.sdp,
            "capabilities": answer.capabilities,
            "frame": answer.frame,
            "from_device_id_hex": hex_encode(&answer.from),
            "display_name": peer.display_name,
            "public_key_hex": hex_encode(&peer.public_key.0),
            "fingerprint": peer.fingerprint.to_spaced_hex(),
        })
        .to_string()
    }

    /// 构造已签名的 X25519 临时公钥交换消息（随机临时密钥）。
    pub fn build_session_key_exchange(&mut self) -> Result<Vec<u8>, WebError> {
        let (pub_k, sec_k) = ephemeral_x25519_keypair();
        self.eph_secret = Some(sec_k);
        self.session_key_exchange_with(x25519_public_bytes(&pub_k))
    }

    /// 确定性变体（黄金测试向量用）：指定 X25519 临时私钥。
    pub fn build_session_key_exchange_det(
        &mut self,
        eph_secret: [u8; 32],
    ) -> Result<Vec<u8>, WebError> {
        let sec_k = X25519SecretKey::from(eph_secret);
        let pub_bytes = X25519PublicKey::from(&sec_k).to_bytes();
        self.eph_secret = Some(sec_k);
        self.session_key_exchange_with(pub_bytes)
    }

    /// 签名 `session_id ‖ from ‖ ephemeral` 并编码为 `Message::SessionKey`
    /// （镜像 `rdcore_session::sign_ephemeral_key`）。
    fn session_key_exchange_with(&self, eph_pub: [u8; 32]) -> Result<Vec<u8>, WebError> {
        let payload = canonical_ephemeral_bytes(self.session_id, self.own.id, eph_pub)?;
        let sig = Ed25519CryptoProvider.sign(&self.secret, &payload);
        Ok(encode(&Message::SessionKey(SessionKeyExchange {
            session_id: self.session_id,
            from: self.own.id,
            ephemeral: eph_pub,
            signature: Some(sig),
        }))?)
    }

    /// 验签对端 `SessionKeyExchange` 并派生会话密钥
    /// （镜像 `rdcore_session::establish_session_key`：先核 session_id，再验签，后 ECDH）。
    pub fn handle_session_key_exchange(&mut self, bytes: &[u8]) -> Result<(), WebError> {
        let msg = decode_limited(bytes, MAX_SIGNALING_MESSAGE_LEN)?;
        let Message::SessionKey(ex) = msg else {
            return Err(WebError::Protocol("期望 Message::SessionKey".into()));
        };
        if ex.session_id != self.session_id {
            return Err(WebError::InvalidSession);
        }
        let peer = self.peers.get(&ex.from).ok_or(WebError::UnknownPeer)?;
        let sig = ex.signature.as_ref().ok_or(WebError::MissingSignature)?;
        let payload = canonical_ephemeral_bytes(ex.session_id, ex.from, ex.ephemeral)?;
        if !Ed25519CryptoProvider.verify(&peer.public_key, &payload, sig) {
            return Err(WebError::InvalidSignature);
        }
        let our = self
            .eph_secret
            .take()
            .ok_or_else(|| WebError::Crypto("临时密钥缺失（重复调用或顺序错误）".into()))?;
        let key = derive_session_key(&our, &X25519PublicKey::from(ex.ephemeral));
        self.session_key = Some(key);
        Ok(())
    }

    /// 构造 ICE 候选信令消息。
    ///
    /// 注意（与现有 Host 对齐的线格式事实）：`candidate` 字段在现有实现里承载的是
    /// **整段 JSON 序列化的 RTCIceCandidateInit**（含 sdpMid/sdpMLineIndex/usernameFragment），
    /// 对端 `serde_json` 还原后加入连接；`sdp_mid` / `sdp_mline_index` 字段平行保留。
    pub fn build_ice(
        &self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u32>,
    ) -> Result<Vec<u8>, WebError> {
        Ok(encode(&Message::Ice(IceCandidate {
            session_id: self.session_id,
            from: self.own.id,
            candidate: candidate.to_string(),
            sdp_mid,
            sdp_mline_index,
        }))?)
    }

    /// 通用信令消息解析；`PeerHello` 按 TOFU 记住（首个版本生效，不覆盖）。
    /// 返回 JSON：`{kind: "peer_hello" | "offer" | "answer" | "ice" | "session_key" | ...}`。
    pub fn handle_message(&mut self, bytes: &[u8]) -> Result<String, WebError> {
        let msg = decode_limited(bytes, MAX_SIGNALING_MESSAGE_LEN)?;
        let v = match &msg {
            Message::PeerHello(p) => {
                if !self.peers.contains_key(&p.id) && p.id != self.own.id {
                    self.peers.insert(p.id, p.clone());
                }
                serde_json::json!({
                    "kind": "peer_hello",
                    "device_id_hex": hex_encode(&p.id),
                    "display_name": p.display_name,
                    "public_key_hex": hex_encode(&p.public_key.0),
                    "fingerprint": p.fingerprint.to_spaced_hex(),
                })
            }
            Message::Offer(_) => serde_json::json!({"kind": "offer"}),
            Message::Answer(_) => serde_json::json!({"kind": "answer"}),
            Message::Ice(c) => serde_json::json!({
                "kind": "ice",
                "candidate": c.candidate,
                "sdp_mid": c.sdp_mid,
                "sdp_mline_index": c.sdp_mline_index,
            }),
            Message::SessionKey(_) => serde_json::json!({"kind": "session_key"}),
            Message::Heartbeat(h) => {
                serde_json::json!({"kind": "heartbeat", "seq": h.seq, "timestamp_ms": h.timestamp_ms})
            }
            Message::InputEvent(_) => serde_json::json!({"kind": "input_event"}),
            Message::Clipboard(_) => serde_json::json!({"kind": "clipboard"}),
            Message::Encrypted(_) => serde_json::json!({"kind": "encrypted"}),
            Message::FileTransfer(_) => serde_json::json!({"kind": "file_transfer"}),
        };
        Ok(v.to_string())
    }

    /// 会话密钥是否已建立。
    pub fn has_session_key(&self) -> bool {
        self.session_key.is_some()
    }

    /// 取出 32 字节会话密钥（供 `FramePipeline::set_session_key`；未建立时报错）。
    pub fn session_key_bytes(&self) -> Result<Vec<u8>, WebError> {
        Ok(self
            .session_key
            .as_ref()
            .ok_or(WebError::NoSessionKey)?
            .0
            .to_vec())
    }

    /// 当前已知的对端（TOFU）公开信息 JSON；未知时报错。
    pub fn peer_json(&self, device_id_hex: &str) -> Result<String, WebError> {
        let id: DeviceId = hex_decode_array(device_id_hex)?;
        let p = self.peers.get(&id).ok_or(WebError::UnknownPeer)?;
        Ok(serde_json::json!({
            "device_id_hex": hex_encode(&p.id),
            "display_name": p.display_name,
            "public_key_hex": hex_encode(&p.public_key.0),
            "fingerprint": p.fingerprint.to_spaced_hex(),
        })
        .to_string())
    }
}

/// wasm-bindgen facade：Viewer 侧握手状态机。构造前须先 `generate_identity` / `identity_import`。
#[wasm_bindgen]
pub struct WebHandshake {
    inner: Handshake,
}

#[wasm_bindgen]
impl WebHandshake {
    /// 以 32 字符小写 hex 的会话 ID（16 字节）构造。
    #[wasm_bindgen(constructor)]
    pub fn new(session_id_hex: &str) -> Result<WebHandshake, JsError> {
        let session_id = SessionId(hex_decode_array(session_id_hex)?);
        let id = identity::current()?;
        Ok(Self {
            inner: Handshake::new(session_id, &id),
        })
    }

    /// postcard 编码的 `Message::PeerHello`（经信令 WS 二进制帧发出）。
    pub fn peer_hello(&self) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.peer_hello()?)
    }

    /// 构造已签名的 Offer。`capabilities_json` 为 `Capabilities` 的 JSON
    /// （如 `{"video_codecs":["H264","Raw"],"max_width":1920,"max_height":1080,"fps":30,
    /// "clipboard":true,"input":{"mouse":true,"keyboard":true,"wheel":true}}`）。
    pub fn build_signed_offer(
        &mut self,
        sdp: &str,
        capabilities_json: &str,
    ) -> Result<Vec<u8>, JsError> {
        let capabilities: Capabilities = serde_json::from_str(capabilities_json)?;
        Ok(self.inner.build_signed_offer(sdp, capabilities)?)
    }

    /// 验签 Host 的 Answer，返回对端 SDP / 能力 / 已认证身份的 JSON。
    pub fn handle_answer(&mut self, bytes: &[u8]) -> Result<String, JsError> {
        Ok(self.inner.handle_answer(bytes)?)
    }

    /// 构造已签名的 X25519 临时公钥交换消息（经控制 DataChannel 明文发出）。
    pub fn build_session_key_exchange(&mut self) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.build_session_key_exchange()?)
    }

    /// 确定性变体（黄金测试向量 / 对拍用）：指定 X25519 临时私钥（hex）。
    pub fn build_session_key_exchange_det(
        &mut self,
        eph_secret_hex: &str,
    ) -> Result<Vec<u8>, JsError> {
        Ok(self
            .inner
            .build_session_key_exchange_det(hex_decode_array(eph_secret_hex)?)?)
    }

    /// 验签对端 `SessionKeyExchange` 并派生会话密钥（存内部状态）。
    pub fn handle_session_key_exchange(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        Ok(self.inner.handle_session_key_exchange(bytes)?)
    }

    /// 构造 ICE 候选信令消息。`candidate` 建议填整段 JSON 序列化的 ICE candidate init
    /// （与现有 Host 的 `add_remote_ice` 对齐；见其字段文档）。
    pub fn build_ice(
        &self,
        candidate: &str,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u32>,
    ) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.build_ice(candidate, sdp_mid, sdp_mline_index)?)
    }

    /// 通用信令消息解析（PeerHello 会按 TOFU 记住对端）。
    pub fn handle_message(&mut self, bytes: &[u8]) -> Result<String, JsError> {
        Ok(self.inner.handle_message(bytes)?)
    }

    /// 会话密钥是否已建立。
    pub fn has_session_key(&self) -> bool {
        self.inner.has_session_key()
    }

    /// 取出 32 字节会话密钥（供 `FramePipeline.set_session_key`）。
    pub fn session_key_bytes(&self) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.session_key_bytes()?)
    }

    /// 已知对端（TOFU）的公开信息 JSON。
    pub fn peer_json(&self, device_id_hex: &str) -> Result<String, JsError> {
        Ok(self.inner.peer_json(device_id_hex)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_proto::{InputCaps, VideoCodec};

    pub fn test_caps() -> Capabilities {
        Capabilities {
            video_codecs: vec![VideoCodec::H264, VideoCodec::Raw],
            max_width: 1920,
            max_height: 1080,
            fps: 30,
            clipboard: true,
            input: InputCaps {
                mouse: true,
                keyboard: true,
                wheel: true,
            },
        }
    }

    fn viewer() -> LocalIdentity {
        LocalIdentity::from_seed([1u8; 32], [0xA0; 16], "viewer")
    }

    fn host() -> LocalIdentity {
        LocalIdentity::from_seed([2u8; 32], [0xB0; 16], "host")
    }

    fn handshake_with_peer() -> Handshake {
        let id = viewer();
        let host = host();
        let mut h = Handshake::new(SessionId([0xC0; 16]), &id);
        // 模拟先收到 Host 的 PeerHello。
        let hello = encode(&Message::PeerHello(host.public.clone())).unwrap();
        h.handle_message(&hello).unwrap();
        h
    }

    #[test]
    fn peer_hello_roundtrips_own_identity() {
        let id = viewer();
        let h = Handshake::new(SessionId([0xC0; 16]), &id);
        let bytes = h.peer_hello().unwrap();
        match rdcore_proto::decode(&bytes).unwrap() {
            Message::PeerHello(p) => assert_eq!(p, id.public),
            _ => panic!("应为 PeerHello"),
        }
    }

    #[test]
    fn offer_is_signed_and_verifiable() {
        let h = handshake_with_peer();
        let bytes = h.build_signed_offer("v=0 web", test_caps()).unwrap();
        let Message::Offer(offer) = rdcore_proto::decode(&bytes).unwrap() else {
            panic!("应为 Offer")
        };
        let sig = offer.signature.clone().expect("应带签名");
        let payload = canonical_signing_bytes(&offer.signing_payload()).unwrap();
        assert!(
            Ed25519CryptoProvider.verify(&viewer().public.public_key, &payload, &sig),
            "签名应可被本端公钥验过"
        );
    }

    #[test]
    fn answer_requires_peer_and_valid_signature() {
        let id = viewer();
        let host = host();
        let sid = SessionId([0xC0; 16]);

        // 未收 PeerHello → UnknownPeer。
        let mut h = Handshake::new(sid, &id);
        let unsigned = Message::Answer(ConnectionAnswer {
            session_id: sid,
            from: host.public.id,
            sdp: "v=0 host".into(),
            capabilities: test_caps(),
            frame: None,
            signature: None,
        });
        assert_eq!(
            h.handle_answer(&encode(&unsigned).unwrap()),
            Err(WebError::UnknownPeer)
        );

        // 收了 PeerHello 但无签名 → MissingSignature。
        let mut h = handshake_with_peer();
        assert_eq!(
            h.handle_answer(&encode(&unsigned).unwrap()),
            Err(WebError::MissingSignature)
        );

        // 伪造签名（用 viewer 私钥签 host 的 answer）→ InvalidSignature。
        let mut forged = match &unsigned {
            Message::Answer(a) => a.clone(),
            _ => unreachable!(),
        };
        let payload = canonical_signing_bytes(&forged.signing_payload()).unwrap();
        forged.signature = Some(Ed25519CryptoProvider.sign(&viewer().secret, &payload));
        assert_eq!(
            h.handle_answer(&encode(&Message::Answer(forged)).unwrap()),
            Err(WebError::InvalidSignature)
        );
    }

    #[test]
    fn session_key_exchange_both_ends_equal() {
        let id = viewer();
        let host = host();
        let sid = SessionId([0xC0; 16]);
        let mut h = handshake_with_peer();

        // Viewer 构造确定性临时密钥交换消息。
        let ex_bytes = h.build_session_key_exchange_det([0x42; 32]).unwrap();
        let Message::SessionKey(ex) = rdcore_proto::decode(&ex_bytes).unwrap() else {
            panic!("应为 SessionKey")
        };
        assert_eq!(ex.session_id, sid);
        assert_eq!(ex.from, id.public.id);
        assert!(ex.signature.is_some());

        // Host 侧构造自己的交换消息（用同一逻辑镜像），Viewer 验签 + 派生。
        let host_sec = X25519SecretKey::from([0x77; 32]);
        let host_pub = X25519PublicKey::from(&host_sec).to_bytes();
        let payload = canonical_ephemeral_bytes(sid, host.public.id, host_pub).unwrap();
        let sig = Ed25519CryptoProvider.sign(&host.secret, &payload);
        let host_ex = encode(&Message::SessionKey(SessionKeyExchange {
            session_id: sid,
            from: host.public.id,
            ephemeral: host_pub,
            signature: Some(sig),
        }))
        .unwrap();
        h.handle_session_key_exchange(&host_ex).unwrap();
        assert!(h.has_session_key());

        // 与 Host 侧手工派生比对。
        let expect = derive_session_key(&host_sec, &X25519PublicKey::from(ex.ephemeral));
        assert_eq!(h.session_key_bytes().unwrap(), expect.0.to_vec());
    }

    #[test]
    fn session_key_exchange_rejects_wrong_session() {
        let host = host();
        let mut h = handshake_with_peer();
        let _ = h.build_session_key_exchange_det([0x42; 32]).unwrap();
        let payload =
            canonical_ephemeral_bytes(SessionId([0x99; 16]), host.public.id, [7u8; 32]).unwrap();
        let sig = Ed25519CryptoProvider.sign(&host.secret, &payload);
        let bad = encode(&Message::SessionKey(SessionKeyExchange {
            session_id: SessionId([0x99; 16]),
            from: host.public.id,
            ephemeral: [7u8; 32],
            signature: Some(sig),
        }))
        .unwrap();
        assert_eq!(
            h.handle_session_key_exchange(&bad),
            Err(WebError::InvalidSession)
        );
    }
}
