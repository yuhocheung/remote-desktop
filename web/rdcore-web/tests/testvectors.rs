//! 黄金测试向量生成器（M0 核心交付物）。
//!
//! 用固定种子 / 固定密钥 / 固定 nonce 生成一组**确定性** fixture，写到
//! `web/testvectors/*.json`。这些 JSON 同时是：
//! - Rust 侧回归基线（本测试写完立即读回校验）；
//! - Node 侧对拍依据（`web/testvectors/parity.mjs` 用 WASM 绑定重算并逐字节比对）。
//!
//! 覆盖：postcard 编码期望字节、Ed25519 签名验签、X25519 ECDH 两端一致、
//! XChaCha20Poly1305 已知密文、SCTP 分片重组、身份导出（KDF + XChaCha）。
//! 与 Host 侧 `rdcore-session` 的互认证据见 `tests/interop.rs`（写 interop.json）。

use std::fs;
use std::path::PathBuf;

use chacha20poly1305::aead::{generic_array::GenericArray, Aead, KeyInit};
use chacha20poly1305::XChaCha20Poly1305;
use rdcore_crypto::{
    derive_session_key, x25519_public_bytes, Ciphertext, CryptoProvider, Ed25519CryptoProvider,
    PublicKey, SessionKey, X25519PublicKey, X25519SecretKey,
};
use rdcore_identity::{KeyProvider, PassphraseKeyProvider};
use rdcore_proto::{canonical_signing_bytes, Capabilities, Message, SessionId};
use rdcore_web::handshake::Handshake;
use rdcore_web::identity::LocalIdentity;
use rdcore_web::pipeline::{
    app_clipboard_request, app_heartbeat, app_input_mouse_move, frame_wrap, sctp_chunks, Pipeline,
    MAX_DATA_FRAME_LEN,
};
use rdcore_web::{hex_decode_array, hex_encode};
use sha2::Digest;

// ───────────────────────── 确定性常量（一切 fixture 的种子） ─────────────────────────

/// Viewer Ed25519 种子（0x00..0x1f）。
pub fn viewer_seed() -> [u8; 32] {
    std::array::from_fn(|i| i as u8)
}
/// Host Ed25519 种子（0x20..0x3f）。
pub fn host_seed() -> [u8; 32] {
    std::array::from_fn(|i| 0x20 + i as u8)
}
/// Viewer DeviceId（0xa0..0xaf）。
pub fn viewer_device() -> [u8; 16] {
    std::array::from_fn(|i| 0xa0 + i as u8)
}
/// Host DeviceId（0xb0..0xbf）。
pub fn host_device() -> [u8; 16] {
    std::array::from_fn(|i| 0xb0 + i as u8)
}
/// 会话 ID（0xc0..0xcf）。
pub fn session_id() -> SessionId {
    SessionId(std::array::from_fn(|i| 0xc0 + i as u8))
}
/// Viewer X25519 临时私钥（0x40..0x5f）。
pub fn viewer_eph() -> [u8; 32] {
    std::array::from_fn(|i| 0x40 + i as u8)
}
/// Host X25519 临时私钥（0x60..0x7f）。
pub fn host_eph() -> [u8; 32] {
    std::array::from_fn(|i| 0x60 + i as u8)
}

/// 能力协商 JSON（parity.mjs 逐字符喂给 `build_signed_offer`，字节必须一致）。
pub const CAPS_JSON: &str = concat!(
    "{\"video_codecs\":[\"H264\",\"Raw\"],\"max_width\":1920,\"max_height\":1080,",
    "\"fps\":30,\"clipboard\":true,",
    "\"input\":{\"mouse\":true,\"keyboard\":true,\"wheel\":true}}"
);
pub const SDP_OFFER: &str = "v=0 rdcore-web test offer";
pub const SDP_ANSWER: &str = "v=0 rdcore-web test answer";
pub const ICE_CANDIDATE_JSON: &str = concat!(
    "{\"candidate\":\"candidate:1 1 UDP 2122194738 192.168.1.10 54321 typ host\",",
    "\"sdpMid\":\"0\",\"sdpMLineIndex\":0,\"usernameFragment\":\"abcd\"}"
);
pub const KDF_PASSPHRASE: &str = "web-test-passphrase";

fn out_dir() -> PathBuf {
    // 测试的工作目录是 crate 根（web/rdcore-web），fixture 写在 web/testvectors。
    let dir = PathBuf::from("../testvectors");
    fs::create_dir_all(&dir).expect("创建 web/testvectors 目录");
    dir
}

fn write_json(dir: &std::path::Path, name: &str, v: &serde_json::Value) {
    let path = dir.join(name);
    let mut text = serde_json::to_string_pretty(v).expect("JSON 序列化");
    text.push('\n');
    fs::write(&path, text).unwrap_or_else(|e| panic!("写 {path:?} 失败: {e}"));
}

/// 会话密钥（双方 ECDH + SHA-256 派生的确定值）。
pub fn session_key() -> SessionKey {
    let viewer_sec = X25519SecretKey::from(viewer_eph());
    let host_pub = X25519PublicKey::from(&X25519SecretKey::from(host_eph()));
    derive_session_key(&viewer_sec, &host_pub)
}

/// 用固定 nonce 做 XChaCha20Poly1305 加密（与 `rdcore_crypto::aead_seal` 同算法，仅 nonce 固定）。
pub fn aead_seal_det(key: &SessionKey, nonce: [u8; 24], plaintext: &[u8]) -> Ciphertext {
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(&key.0));
    let data = cipher
        .encrypt(GenericArray::from_slice(&nonce), plaintext)
        .expect("AEAD 加密不应失败");
    Ciphertext { nonce, data }
}

#[test]
fn generate_and_verify_fixtures() {
    let provider = Ed25519CryptoProvider;
    let viewer = LocalIdentity::from_seed(viewer_seed(), viewer_device(), "web-viewer-fixture");
    let host = LocalIdentity::from_seed(host_seed(), host_device(), "windows-host-fixture");
    let sid = session_id();

    // ── 1. identity.json ────────────────────────────────────────────────
    let kdf_salt: [u8; 16] = std::array::from_fn(|i| 0xd0 + i as u8);
    let kdf_key = PassphraseKeyProvider::new(KDF_PASSPHRASE).derive_key(&kdf_salt);
    let export_salt: [u8; 16] = std::array::from_fn(|i| 0xe0 + i as u8);
    let export_nonce: [u8; 24] = std::array::from_fn(|i| 0x80 + i as u8);
    let export_blob = viewer
        .export_with(KDF_PASSPHRASE, export_salt, export_nonce)
        .expect("确定性导出");
    // 读回校验：导入固定 blob 必须还原同一身份；错误口令必须失败。
    let restored = LocalIdentity::import(&export_blob, KDF_PASSPHRASE).expect("导入");
    assert_eq!(restored.public, viewer.public);
    assert!(LocalIdentity::import(&export_blob, "wrong-pass").is_err());

    let identity_json = serde_json::json!({
        "description": "固定种子的确定性身份 + KDF + 口令导出向量（fingerprint = SHA-256(公钥) 空格分隔大写 hex）",
        "viewer": {
            "seed_hex": hex_encode(&viewer_seed()),
            "device_id_hex": hex_encode(&viewer_device()),
            "display_name": viewer.public.display_name,
            "public_key_hex": hex_encode(&viewer.public.public_key.0),
            "fingerprint": viewer.public.fingerprint.to_spaced_hex(),
        },
        "host": {
            "seed_hex": hex_encode(&host_seed()),
            "device_id_hex": hex_encode(&host_device()),
            "display_name": host.public.display_name,
            "public_key_hex": hex_encode(&host.public.public_key.0),
            "fingerprint": host.public.fingerprint.to_spaced_hex(),
        },
        "kdf": {
            "algorithm": "SHA-256(passphrase || salt) 迭代 100000 次（rdcore-identity PassphraseKeyProvider）",
            "passphrase": KDF_PASSPHRASE,
            "salt_hex": hex_encode(&kdf_salt),
            "iterations": 100_000,
            "key_hex": hex_encode(&kdf_key),
        },
        "export": {
            "format": "[salt(16)][nonce(24)][XChaCha20Poly1305(JSON{device_id_hex,secret_key_hex,display_name})]",
            "passphrase": KDF_PASSPHRASE,
            "salt_hex": hex_encode(&export_salt),
            "nonce_hex": hex_encode(&export_nonce),
            "blob_hex": hex_encode(&export_blob),
            "expect_public_key_hex": hex_encode(&viewer.public.public_key.0),
        },
    });

    // ── 2. messages.json（postcard 编码期望字节） ────────────────────────
    let caps: Capabilities = serde_json::from_str(CAPS_JSON).expect("capabilities JSON 合法");
    let mut hs = Handshake::new(sid, &viewer);
    let peer_hello_viewer = hs.peer_hello().expect("peer_hello");
    let peer_hello_host = rdcore_proto::encode(&Message::PeerHello(host.public.clone())).unwrap();
    hs.handle_message(&peer_hello_host).expect("记住 Host");

    let offer_bytes = hs.build_signed_offer(SDP_OFFER, caps).expect("签名 Offer");
    let offer = match rdcore_proto::decode(&offer_bytes).unwrap() {
        Message::Offer(o) => o,
        _ => panic!("应为 Offer"),
    };
    let offer_canonical = canonical_signing_bytes(&offer.signing_payload()).unwrap();
    let offer_sig = offer.signature.clone().expect("Offer 带签名");
    // 本端公钥可验（与 Host 用 rdcore-session 验签的互认见 interop.rs）。
    assert!(provider.verify(&viewer.public.public_key, &offer_canonical, &offer_sig));

    // Host 侧 Answer：用 rdcore-session 的 sign_answer 构造（Host 真实代码路径）。
    let answer = rdcore_session::sign_answer(
        &provider,
        &host.secret,
        rdcore_proto::ConnectionAnswer {
            session_id: sid,
            from: host.public.id,
            sdp: SDP_ANSWER.to_string(),
            capabilities: serde_json::from_str(CAPS_JSON).unwrap(),
            frame: None,
            signature: None,
        },
    );
    let answer_bytes = rdcore_proto::encode(&Message::Answer(answer)).unwrap();
    let answer_json = hs.handle_answer(&answer_bytes).expect("验签 Answer");
    assert!(answer_json.contains(SDP_ANSWER), "{answer_json}");

    let ice_bytes = hs
        .build_ice(ICE_CANDIDATE_JSON, Some("0".to_string()), Some(0))
        .expect("ICE");

    let viewer_ske = hs
        .build_session_key_exchange_det(viewer_eph())
        .expect("Viewer SessionKeyExchange");
    let host_ske = rdcore_proto::encode(&Message::SessionKey(rdcore_session::sign_ephemeral_key(
        &provider,
        &host.secret,
        sid,
        host.public.id,
        x25519_public_bytes(&X25519PublicKey::from(&X25519SecretKey::from(host_eph()))),
    )))
    .unwrap();
    hs.handle_session_key_exchange(&host_ske)
        .expect("派生会话密钥");
    assert_eq!(hs.session_key_bytes().unwrap(), session_key().0.to_vec());

    let app_input = app_input_mouse_move(1, 640, 360).unwrap();
    let app_heartbeat = app_heartbeat(1, 1_700_000_000_123).unwrap();
    let app_clipboard = app_clipboard_request(2).unwrap();

    let messages_json = serde_json::json!({
        "description": "各 Message 变体的 postcard 编码期望字节（hex）+ AppMessage 负载",
        "session_id_hex": hex_encode(&sid.0),
        "capabilities_json": CAPS_JSON,
        "sdp_offer": SDP_OFFER,
        "sdp_answer": SDP_ANSWER,
        "ice_candidate_json": ICE_CANDIDATE_JSON,
        "peer_hello_viewer_hex": hex_encode(&peer_hello_viewer),
        "peer_hello_host_hex": hex_encode(&peer_hello_host),
        "offer_signed_hex": hex_encode(&offer_bytes),
        "offer_canonical_signing_bytes_hex": hex_encode(&offer_canonical),
        "offer_signature_hex": hex_encode(&offer_sig.0),
        "answer_signed_hex": hex_encode(&answer_bytes),
        "ice_hex": hex_encode(&ice_bytes),
        "session_key_exchange_viewer_hex": hex_encode(&viewer_ske),
        "session_key_exchange_host_hex": hex_encode(&host_ske),
        "app_input_mouse_move_hex": hex_encode(&app_input),
        "app_heartbeat_hex": hex_encode(&app_heartbeat),
        "app_clipboard_request_hex": hex_encode(&app_clipboard),
    });

    // ── 3. crypto.json（签名 / ECDH / AEAD 已知密文） ────────────────────
    let aead_key = session_key();
    let aead_nonce: [u8; 24] = std::array::from_fn(|i| 0x55 + i as u8);
    let aead_plain = app_input.clone();
    let aead_ct = aead_seal_det(&aead_key, aead_nonce, &aead_plain);
    // 解密必须还原（已知密文向量）。
    let opened = rdcore_crypto::aead_open(&aead_key, &aead_ct).expect("已知密文可解密");
    assert_eq!(opened, aead_plain);
    // Message::Encrypted 承载同一密文（控制通道线格式）。
    let encrypted_msg = rdcore_proto::encode(&Message::Encrypted(aead_ct.clone())).unwrap();

    // 加密媒体帧：pixels = postcard(Ciphertext)（镜像 rdcore-app::send_media）。
    let pixels: Vec<u8> = (0..2048u32).map(|i| (i % 256) as u8).collect();
    let media_nonce: [u8; 24] = std::array::from_fn(|i| 0x66 + i as u8);
    let media_ct = aead_seal_det(&aead_key, media_nonce, &pixels);
    let media_frame = rdcore_proto::MediaFrame {
        codec: rdcore_proto::VideoCodec::H264,
        width: 64,
        height: 48,
        data: postcard::to_stdvec(&media_ct).unwrap(),
    };
    let media_payload = postcard::to_stdvec(&media_frame).unwrap();
    // 经 Pipeline 解密必须还原像素。
    let mut pipe = Pipeline::new(rdcore_web::pipeline::MAX_MEDIA_FRAME_LEN);
    pipe.set_session_key(aead_key.0);
    let media_json = pipe
        .decrypt_media_frame(&media_payload)
        .expect("媒体帧解密");
    assert!(
        media_json.contains(&hex_encode(&pixels[..16])),
        "{media_json}"
    );

    let crypto_json = serde_json::json!({
        "description": "Ed25519 签名 / X25519 ECDH / XChaCha20Poly1305 已知密文 / 加密媒体帧向量",
        "ed25519": {
            "algorithm": "Ed25519 over canonical_signing_bytes(SigningPayload)",
            "viewer_secret_hex": hex_encode(&viewer_seed()),
            "viewer_public_key_hex": hex_encode(&viewer.public.public_key.0),
            "message_hex": hex_encode(&offer_canonical),
            "signature_hex": hex_encode(&offer_sig.0),
        },
        "x25519": {
            "kdf": "SHA-256(X25519 共享密钥)",
            "viewer_secret_hex": hex_encode(&viewer_eph()),
            "viewer_public_hex": hex_encode(&x25519_public_bytes(&X25519PublicKey::from(
                &X25519SecretKey::from(viewer_eph())
            ))),
            "host_secret_hex": hex_encode(&host_eph()),
            "host_public_hex": hex_encode(&x25519_public_bytes(&X25519PublicKey::from(
                &X25519SecretKey::from(host_eph())
            ))),
            "session_key_hex": hex_encode(&aead_key.0),
        },
        "aead": {
            "algorithm": "XChaCha20Poly1305（Ciphertext = {nonce[24], data}）",
            "key_hex": hex_encode(&aead_key.0),
            "nonce_hex": hex_encode(&aead_nonce),
            "plaintext_hex": hex_encode(&aead_plain),
            "ciphertext_hex": hex_encode(&aead_ct.data),
            "encrypted_message_hex": hex_encode(&encrypted_msg),
        },
        "media_frame": {
            "codec": "H264",
            "width": 64,
            "height": 48,
            "pixels_hex": hex_encode(&pixels),
            "sealed_ciphertext_nonce_hex": hex_encode(&media_nonce),
            "payload_hex": hex_encode(&media_payload),
        },
    });

    // ── 4. fragment.json（SCTP 分片：整包 / 2 片 / 多片） ────────────────
    let small = frame_wrap(&peer_hello_viewer, MAX_DATA_FRAME_LEN).unwrap();
    let two = frame_wrap(
        &(0..20_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        rdcore_web::pipeline::MAX_MEDIA_FRAME_LEN,
    )
    .unwrap();
    let three = frame_wrap(
        &(0..40_000u32).map(|i| (i % 241) as u8).collect::<Vec<u8>>(),
        rdcore_web::pipeline::MAX_MEDIA_FRAME_LEN,
    )
    .unwrap();
    let frag_case = |name: &str, framed: &Vec<u8>| {
        let chunks = sctp_chunks(framed);
        // 重组必须还原（经 Pipeline 全链路：标签重组 + 去长度前缀）。
        let mut p = Pipeline::new(rdcore_web::pipeline::MAX_MEDIA_FRAME_LEN);
        let mut out = None;
        for c in &chunks {
            out = p.push_sctp_message(c).unwrap().or(out);
        }
        assert_eq!(
            out.as_deref(),
            Some(&framed[4..]),
            "{name}: 重组 + 去前缀应还原负载"
        );
        serde_json::json!({
            "name": name,
            "framed_hex": hex_encode(framed),
            "payload_hex": hex_encode(&framed[4..]),
            "chunks": chunks.iter().map(|c| hex_encode(c)).collect::<Vec<_>>(),
        })
    };
    let fragment_json = serde_json::json!({
        "description": "SCTP 分片向量：1 字节标签（0=整包 1=首片 2=中片 3=末片），16 KiB 切片",
        "dc_chunk_size": rdcore_web::pipeline::DC_CHUNK_SIZE,
        "cases": [
            frag_case("whole_small", &small),
            frag_case("two_chunks", &two),
            frag_case("three_chunks", &three),
        ],
    });

    // ── 写盘并读回校验 ────────────────────────────────────────────────
    let dir = out_dir();
    write_json(&dir, "identity.json", &identity_json);
    write_json(&dir, "messages.json", &messages_json);
    write_json(&dir, "crypto.json", &crypto_json);
    write_json(&dir, "fragment.json", &fragment_json);

    for (name, expect) in [
        ("identity.json", &identity_json),
        ("messages.json", &messages_json),
        ("crypto.json", &crypto_json),
        ("fragment.json", &fragment_json),
    ] {
        let raw = fs::read_to_string(dir.join(name)).expect("读回 fixture");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("fixture 是合法 JSON");
        assert_eq!(&parsed, expect, "{name} 读回应与写入一致");
    }
}

/// 固定种子的公钥必须由私钥派生（独立重算，防 fixture 自身写错）。
#[test]
fn fixture_public_keys_derive_from_seeds() {
    let provider = Ed25519CryptoProvider;
    for seed in [viewer_seed(), host_seed()] {
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pk = PublicKey(signing.verifying_key().to_bytes());
        // fingerprint 必须等于 SHA-256(pubkey)。
        let fp = provider.fingerprint(&pk);
        assert_eq!(fp.0.as_slice(), sha2::Sha256::digest(pk.0).as_slice());
    }
}

/// hex 工具与 fixture 常量自洽。
#[test]
fn fixture_constants_well_formed() {
    let _: [u8; 32] = hex_decode_array(&hex_encode(&viewer_seed())).unwrap();
    let caps: Capabilities = serde_json::from_str(CAPS_JSON).unwrap();
    assert_eq!(caps.video_codecs.len(), 2);
    assert!(caps.input.mouse && caps.input.keyboard && caps.input.wheel);
}
