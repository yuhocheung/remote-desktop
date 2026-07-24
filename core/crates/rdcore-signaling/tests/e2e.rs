//! P3 端到端：真实 WebSocket 信令服务器 + 客户端，验证三条独立通道。
//!
//! 架构文档 §1/§5 把一条连接拆成三条互不干扰的通道，本测试逐一跑通：
//! - **信令通道**（真实 WebSocket，signaling-svc 中继）：只承载 SDP/ICE（Offer/Answer/Ice）。
//! - **媒体通道**（`rdcore_media::MediaChannel`）：屏幕视频流（P3 用进程内实现占位，未来换 RTP/WebRTC）。
//! - **数据通道**（`rdcore_media::DataChannel`）：输入 / 剪贴板 / 心跳（P3 用进程内实现占位，未来换 WebRTC DataChannel）。
//!
//! 关键点：输入/剪贴板/心跳**不再**走信令 WebSocket，符合"云端只看 SDP/ICE"的约束。

use std::time::Duration;

use rdcore_consent::{
    ClosedReason, ConnectionState, ConsentDecision, ConsentGate, ConsentMode, ConsentScope,
};
use rdcore_crypto::{
    aead_open, aead_seal, ephemeral_x25519_keypair, x25519_public_bytes, Ed25519CryptoProvider,
    SessionKey,
};
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_loopback::{
    BufferFrameSink, Frame, FrameDecoder, FrameEncoder, FrameSink, FrameSource, InputInjector,
    RawDecoder, RawEncoder, RecordingInputInjector, SyntheticFrameSource,
};
use rdcore_media::{
    data_channel_pair, media_channel_pair, tcp_channel_pair, DataChannel, MediaChannel,
};
use rdcore_proto::{
    Capabilities, Ciphertext, ClipboardAction, ClipboardEvent, ConnectionAnswer, ConnectionOffer,
    Heartbeat, IceCandidate, InputEvent, InputKind, Message, MouseButton, SessionId, VideoCodec,
};
use rdcore_session::{
    establish_session_key, sign_answer, sign_ephemeral_key, sign_offer, verify_answer,
    verify_offer, HandshakeError,
};
use rdcore_signaling::SignalingClient;

/// 在空闲端口启动信令服务器，返回其地址。
async fn spawn_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = signaling_svc::serve_listener(listener).await;
    });
    addr
}

fn caps() -> Capabilities {
    Capabilities {
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
    }
}

fn frame_meta() -> rdcore_proto::FrameMetadata {
    rdcore_proto::FrameMetadata {
        width: 64,
        height: 48,
        fps: 30,
        codec: VideoCodec::Raw,
    }
}

#[tokio::test]
async fn e2e_signaling_handshake_and_pipeline() {
    let addr = spawn_server().await;
    // 本次会话的 session_id（也通过 WebSocket URL 路径告知服务器，用于房间路由）。
    let session = SessionId([7u8; 16]);
    // session_id 通过 WebSocket URL 路径携带（ws://host/<session_hex>）。
    let url = format!("ws://{addr}/{}", signaling_svc::session_hex(&session));

    // 两个端点各自连上真实信令服务器（viewer = 控制端，host = 被控端）。
    let (viewer, host) = (
        SignalingClient::connect(&url).await.unwrap(),
        SignalingClient::connect(&url).await.unwrap(),
    );

    // ---------- 0) 控制面（P4）：双方创建 Ed25519 身份，带外预共享 ----------
    // 模拟"首次配对扫码"：各自把对方公钥记进自己的 IdentityStore，握手期据此验签。
    let crypto = Ed25519CryptoProvider;
    let (viewer_id, viewer_sk) = create_local_identity(&crypto, "viewer-laptop");
    let (host_id, host_sk) = create_local_identity(&crypto, "host-desktop");
    let mut viewer_store = InMemoryIdentityStore::new(viewer_id.clone());
    let mut host_store = InMemoryIdentityStore::new(host_id.clone());
    viewer_store.remember(host_id.clone());
    host_store.remember(viewer_id.clone());

    // ---------- 1) 信令通道：仅 SDP/ICE + Ed25519 签名（真实 WebSocket 中继）----------
    // Viewer 对 Offer 签名（覆盖 session_id || from || sdp 的规范字节）。
    let offer = sign_offer(
        &crypto,
        &viewer_sk,
        ConnectionOffer {
            session_id: session,
            from: viewer_id.id,
            sdp: "v=0...".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    viewer.send(&Message::Offer(offer)).await.unwrap();
    let got_offer = match host.recv().await.unwrap().unwrap() {
        Message::Offer(o) => o,
        other => panic!("期望收到 Offer，实际 {other:?}"),
    };
    // Host 验签：从 store 取 Viewer 公钥校验 → 得到已确认对端（含指纹，真实系统带外展示给用户）。
    let verified_viewer = verify_offer(&crypto, &host_store, &got_offer).expect("Host 应验签通过");
    assert_eq!(verified_viewer.fingerprint, viewer_id.fingerprint);

    // Host 对 Answer 签名，Viewer 验签。
    let answer = sign_answer(
        &crypto,
        &host_sk,
        ConnectionAnswer {
            session_id: session,
            from: host_id.id,
            sdp: "v=0...answer".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    host.send(&Message::Answer(answer)).await.unwrap();
    let got_answer = match viewer.recv().await.unwrap().unwrap() {
        Message::Answer(a) => a,
        other => panic!("期望收到 Answer，实际 {other:?}"),
    };
    let verified_host =
        verify_answer(&crypto, &viewer_store, &got_answer).expect("Viewer 应验签通过");
    assert_eq!(verified_host.fingerprint, host_id.fingerprint);

    // 负向：未签名的 Offer 应被验签拒绝（控制面安全闸门，防未认证/冒充连接）。
    let rogue = ConnectionOffer {
        session_id: session,
        from: viewer_id.id,
        sdp: "v=0... rogue".into(),
        capabilities: caps(),
        frame: Some(frame_meta()),
        signature: None,
    };
    viewer.send(&Message::Offer(rogue)).await.unwrap();
    let rogue_got = match host.recv().await.unwrap().unwrap() {
        Message::Offer(o) => o,
        other => panic!("期望收到 Offer，实际 {other:?}"),
    };
    assert_eq!(
        verify_offer(&crypto, &host_store, &rogue_got),
        Err(HandshakeError::MissingSignature),
        "未签名 Offer 必须被拒绝"
    );

    host.send(&Message::Ice(IceCandidate {
        session_id: session,
        from: host_id.id,
        candidate: "candidate:1 1 UDP 1234 192.0.2.1 5000 typ host".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    }))
    .await
    .unwrap();
    let got_ice = viewer.recv().await.unwrap().unwrap();
    assert!(matches![got_ice, Message::Ice(_)]);

    // ---------- 2) 媒体通道：Host 捕获→编码→MediaChannel→Viewer 解码→渲染 ----------
    // P3 起媒体走专用 MediaChannel（未来换真实 RTP/WebRTC 后端时上层不用改）。
    let (host_media, viewer_media) = media_channel_pair();

    let width = 64u32;
    let height = 48u32;
    let frames = 5u32;
    let expected: Vec<Frame> = {
        let mut s = SyntheticFrameSource::new(width, height, frames);
        std::iter::from_fn(|| s.next_frame()).collect()
    };
    let mut source = SyntheticFrameSource::new(width, height, frames);
    let encoder = RawEncoder;
    let decoder = RawDecoder;
    let mut sink = BufferFrameSink::default();
    let mut actual = Vec::with_capacity(frames as usize);
    while let Some(frame) = source.next_frame() {
        let media = encoder.encode(&frame).unwrap();
        host_media.send_frame(&media).await.unwrap();
        let media_in = viewer_media.recv_frame().await.unwrap().unwrap();
        let decoded = decoder.decode(&media_in).unwrap();
        actual.push(decoded.clone());
        sink.present(&decoded);
    }
    assert_eq!(sink.presented, frames as u64, "应渲染 5 帧");
    assert_eq!(
        actual, expected,
        "每帧都应经媒体通道无损往返（P3 MediaChannel，未来换 RTP 上层不变）"
    );

    // ---------- 3) 输入管线：Viewer→DataChannel→Host ----------
    // P3：输入/剪贴板/心跳改走 DataChannel，不再经信令 WebSocket（架构 §1/§5）。
    let (viewer_dc, host_dc) = data_channel_pair();

    let scripted = vec![
        InputEvent {
            seq: 1,
            kind: InputKind::MouseMove { x: 10, y: 20 },
        },
        InputEvent {
            seq: 2,
            kind: InputKind::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        },
        InputEvent {
            seq: 3,
            kind: InputKind::Key {
                key_code: 65,
                pressed: true,
                modifiers: 0,
            },
        },
    ];
    let mut injector = RecordingInputInjector::default();
    for ev in &scripted {
        viewer_dc
            .send(&Message::InputEvent(ev.clone()))
            .await
            .unwrap();
        let m = host_dc.recv().await.unwrap().unwrap();
        match m {
            Message::InputEvent(received) => injector.inject(&received),
            other => panic!("期望收到 InputEvent，实际 {other:?}"),
        }
    }
    assert_eq!(
        injector.received, scripted,
        "输入事件应经数据通道原样往返（postcard 编解码无损）"
    );

    // ---------- 4) 剪贴板：Viewer 请求→Host 回数据（走 DataChannel）----------
    viewer_dc
        .send(&Message::Clipboard(ClipboardEvent {
            seq: 1,
            action: ClipboardAction::Request,
        }))
        .await
        .unwrap();
    let req = host_dc.recv().await.unwrap().unwrap();
    assert!(matches!(
        req,
        Message::Clipboard(ClipboardEvent {
            action: ClipboardAction::Request,
            ..
        })
    ));

    let clip = vec![0xABu8; 128];
    host_dc
        .send(&Message::Clipboard(ClipboardEvent {
            seq: 2,
            action: ClipboardAction::Data(clip.clone()),
        }))
        .await
        .unwrap();
    let resp = viewer_dc.recv().await.unwrap().unwrap();
    match resp {
        Message::Clipboard(ClipboardEvent {
            action: ClipboardAction::Data(got),
            ..
        }) => assert_eq!(got, clip, "剪贴板数据应经数据通道原样往返"),
        other => panic!("期望收到 Clipboard Data，实际 {other:?}"),
    }

    // ---------- 5) 心跳（走 DataChannel）----------
    host_dc
        .send(&Message::Heartbeat(Heartbeat {
            seq: 1,
            timestamp_ms: 1_700_000_000_000,
        }))
        .await
        .unwrap();
    let hb = viewer_dc.recv().await.unwrap().unwrap();
    assert!(matches!(hb, Message::Heartbeat(_)));
}

/// P5 端到端集成：同意门控 + 会话密钥握手（E2E）+ 加密通道。
///
/// 在 P4 已认证握手的基础上，进一步验证：Host 必须在"已认证的 Viewer 身份"上显式批准
/// （同意门控）；批准后两端做一次绑定身份的 X25519 密钥协商，得到相同的端到端会话密钥；
/// 之后 Viewer 用该密钥加密的控制负载经真实信令 WebSocket 发送，Host 能解密、错误密钥不能。
#[tokio::test]
async fn e2e_p5_consent_and_session_crypto() {
    let addr = spawn_server().await;
    let session = SessionId([7u8; 16]);
    let url = format!("ws://{addr}/{}", signaling_svc::session_hex(&session));
    let (viewer, host) = (
        SignalingClient::connect(&url).await.unwrap(),
        SignalingClient::connect(&url).await.unwrap(),
    );

    // 0) 身份 + 带外预共享（同 P4 握手）
    let crypto = Ed25519CryptoProvider;
    let (viewer_id, viewer_sk) = create_local_identity(&crypto, "viewer-laptop");
    let (host_id, host_sk) = create_local_identity(&crypto, "host-desktop");
    let mut viewer_store = InMemoryIdentityStore::new(viewer_id.clone());
    let mut host_store = InMemoryIdentityStore::new(host_id.clone());
    viewer_store.remember(host_id.clone());
    host_store.remember(viewer_id.clone());

    // 1) P4 握手：Offer/Answer 签名验签
    let offer = sign_offer(
        &crypto,
        &viewer_sk,
        ConnectionOffer {
            session_id: session,
            from: viewer_id.id,
            sdp: "v=0...".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    viewer.send(&Message::Offer(offer)).await.unwrap();
    let got_offer = match host.recv().await.unwrap().unwrap() {
        Message::Offer(o) => o,
        other => panic!("期望收到 Offer，实际 {other:?}"),
    };
    let verified_viewer = verify_offer(&crypto, &host_store, &got_offer).expect("Host 应验签通过");

    let answer = sign_answer(
        &crypto,
        &host_sk,
        ConnectionAnswer {
            session_id: session,
            from: host_id.id,
            sdp: "v=0...answer".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    host.send(&Message::Answer(answer)).await.unwrap();
    let got_answer = match viewer.recv().await.unwrap().unwrap() {
        Message::Answer(a) => a,
        other => panic!("期望收到 Answer，实际 {other:?}"),
    };
    let _verified_host =
        verify_answer(&crypto, &viewer_store, &got_answer).expect("Viewer 应验签通过");

    // 2) P5 同意门控：Host 用已认证 Viewer 身份建门控并批准
    let mut gate = ConsentGate::new(
        verified_viewer.clone(),
        ConsentMode::Interactive,
        Duration::from_secs(30),
    );
    gate.request_consent(None);
    assert!(!gate.is_active(), "Host 批准前不应 Active");
    gate.decide(ConsentDecision::Grant {
        scopes: [
            ConsentScope::View,
            ConsentScope::Input,
            ConsentScope::Clipboard,
        ]
        .into_iter()
        .collect(),
        duration: None,
    });
    assert!(gate.is_active(), "Host 批准后应 Active");
    assert!(gate.scopes_allow(ConsentScope::Input));
    // 安全指示（不可伪造横幅数据）应反映已认证对端 + 握手前未加密
    let indicator_before = gate.security_indicator(false);
    assert_eq!(indicator_before.display_name, "viewer-laptop");
    assert!(!indicator_before.encrypted);

    // 3) P5 会话密钥握手（E2E）：两端各发签名临时公钥，经真实信令 WebSocket
    let (v_pub, v_sec) = ephemeral_x25519_keypair();
    let v_ex = sign_ephemeral_key(
        &crypto,
        &viewer_sk,
        session,
        viewer_id.id,
        x25519_public_bytes(&v_pub),
    );
    viewer
        .send(&Message::SessionKey(v_ex.clone()))
        .await
        .unwrap();
    let got_v_ex = match host.recv().await.unwrap().unwrap() {
        Message::SessionKey(e) => e,
        other => panic!("期望收到 SessionKey，实际 {other:?}"),
    };
    let (h_pub, h_sec) = ephemeral_x25519_keypair();
    let host_key = establish_session_key(&crypto, &host_store, &h_sec, &got_v_ex, session)
        .expect("Host 应派生会话密钥");

    let h_ex = sign_ephemeral_key(
        &crypto,
        &host_sk,
        session,
        host_id.id,
        x25519_public_bytes(&h_pub),
    );
    host.send(&Message::SessionKey(h_ex.clone())).await.unwrap();
    let got_h_ex = match viewer.recv().await.unwrap().unwrap() {
        Message::SessionKey(e) => e,
        other => panic!("期望收到 SessionKey，实际 {other:?}"),
    };
    let viewer_key = establish_session_key(&crypto, &viewer_store, &v_sec, &got_h_ex, session)
        .expect("Viewer 应派生会话密钥");
    assert_eq!(host_key, viewer_key, "两端应得到相同会话密钥（端到端）");

    // 4) E2E 加密通道：Viewer 用会话密钥加密一段控制负载，经真实信令 WebSocket 发送，
    //    Host 用同一会话密钥解密（中继只看到密文，看不到明文）。
    let payload = b"P5 end-to-end encrypted control payload";
    let ct = aead_seal(&viewer_key, payload);
    viewer.send(&Message::Encrypted(ct.clone())).await.unwrap();
    let received: Ciphertext = match host.recv().await.unwrap().unwrap() {
        Message::Encrypted(c) => c,
        other => panic!("期望收到 Encrypted，实际 {other:?}"),
    };
    let decrypted = aead_open(&host_key, &received).expect("Host 用会话密钥解密应成功");
    assert_eq!(decrypted, payload);
    // 错误密钥无法解密
    let wrong_key = SessionKey([0u8; 32]);
    assert!(aead_open(&wrong_key, &ct).is_none(), "错误密钥必须解密失败");

    // 5) 安全指示更新为已加密
    let indicator_after = gate.security_indicator(true);
    assert!(indicator_after.encrypted);

    // 6) Host 随时终止
    gate.revoke();
    assert!(matches!(
        gate.state(),
        ConnectionState::Closed(ClosedReason::Revoked)
    ));
}

/// P7 集成：媒体 / 数据通道走**真实 localhost TCP**（不再进程内占位），信令仍走真实 WebSocket。
///
/// 证明 P3 的 `MediaChannel` / `DataChannel` 抽象能无缝换上 `TcpTransport` 后端——
/// 整条管线（SDP/ICE 经 WebSocket 中继、视频帧经 TCP、输入/剪贴板/心跳经 TCP）真正跨网络字节管道往返，
/// 且上层握手 / 验签 / 编解码逻辑一行未改。
#[tokio::test]
async fn e2e_p7_real_transports() {
    let addr = spawn_server().await;
    let session = SessionId([7u8; 16]);
    let url = format!("ws://{addr}/{}", signaling_svc::session_hex(&session));
    let (viewer, host) = (
        SignalingClient::connect(&url).await.unwrap(),
        SignalingClient::connect(&url).await.unwrap(),
    );

    // 0) 身份 + 带外预共享（同 P4 握手）
    let crypto = Ed25519CryptoProvider;
    let (viewer_id, viewer_sk) = create_local_identity(&crypto, "viewer-laptop");
    let (host_id, host_sk) = create_local_identity(&crypto, "host-desktop");
    let mut viewer_store = InMemoryIdentityStore::new(viewer_id.clone());
    let mut host_store = InMemoryIdentityStore::new(host_id.clone());
    viewer_store.remember(host_id.clone());
    host_store.remember(viewer_id.clone());

    // 1) 信令通道：仅 SDP/ICE + Ed25519 签名（真实 WebSocket 中继）
    let offer = sign_offer(
        &crypto,
        &viewer_sk,
        ConnectionOffer {
            session_id: session,
            from: viewer_id.id,
            sdp: "v=0...".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    viewer.send(&Message::Offer(offer)).await.unwrap();
    let got_offer = match host.recv().await.unwrap().unwrap() {
        Message::Offer(o) => o,
        other => panic!("期望收到 Offer，实际 {other:?}"),
    };
    let verified_viewer = verify_offer(&crypto, &host_store, &got_offer).expect("Host 应验签通过");
    assert_eq!(verified_viewer.fingerprint, viewer_id.fingerprint);

    let answer = sign_answer(
        &crypto,
        &host_sk,
        ConnectionAnswer {
            session_id: session,
            from: host_id.id,
            sdp: "v=0...answer".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        },
    );
    host.send(&Message::Answer(answer)).await.unwrap();
    let got_answer = match viewer.recv().await.unwrap().unwrap() {
        Message::Answer(a) => a,
        other => panic!("期望收到 Answer，实际 {other:?}"),
    };
    let _verified_host =
        verify_answer(&crypto, &viewer_store, &got_answer).expect("Viewer 应验签通过");

    // 2) ★ P7：媒体 / 数据通道走真实 localhost TCP（不再进程内占位）
    let ((host_media, host_dc), (viewer_media, viewer_dc)) = tcp_channel_pair().await.unwrap();

    // 媒体通道：Host 捕获→编码→真实 TCP→Viewer 解码→渲染
    let width = 64u32;
    let height = 48u32;
    let frames = 5u32;
    let expected: Vec<Frame> = {
        let mut s = SyntheticFrameSource::new(width, height, frames);
        std::iter::from_fn(|| s.next_frame()).collect()
    };
    let mut source = SyntheticFrameSource::new(width, height, frames);
    let encoder = RawEncoder;
    let decoder = RawDecoder;
    let mut sink = BufferFrameSink::default();
    let mut actual = Vec::with_capacity(frames as usize);
    while let Some(frame) = source.next_frame() {
        let media = encoder.encode(&frame).unwrap();
        host_media.send_frame(&media).await.unwrap();
        let media_in = viewer_media.recv_frame().await.unwrap().unwrap();
        let decoded = decoder.decode(&media_in).unwrap();
        actual.push(decoded.clone());
        sink.present(&decoded);
    }
    assert_eq!(sink.presented, frames as u64, "应经真实 TCP 渲染 5 帧");
    assert_eq!(
        actual, expected,
        "每帧都应经真实 TCP 媒体通道无损往返（上层握手/编解码未改）"
    );

    // 3) 数据通道（真实 TCP）：Viewer→Host 输入事件
    let scripted = vec![
        InputEvent {
            seq: 1,
            kind: InputKind::MouseMove { x: 10, y: 20 },
        },
        InputEvent {
            seq: 2,
            kind: InputKind::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        },
        InputEvent {
            seq: 3,
            kind: InputKind::Key {
                key_code: 65,
                pressed: true,
                modifiers: 0,
            },
        },
    ];
    let mut injector = RecordingInputInjector::default();
    for ev in &scripted {
        viewer_dc
            .send(&Message::InputEvent(ev.clone()))
            .await
            .unwrap();
        let m = host_dc.recv().await.unwrap().unwrap();
        match m {
            Message::InputEvent(received) => injector.inject(&received),
            other => panic!("期望收到 InputEvent，实际 {other:?}"),
        }
    }
    assert_eq!(
        injector.received, scripted,
        "输入事件应经真实 TCP 数据通道原样往返"
    );

    // 4) 心跳（真实 TCP 数据通道）
    host_dc
        .send(&Message::Heartbeat(Heartbeat {
            seq: 1,
            timestamp_ms: 1_700_000_000_000,
        }))
        .await
        .unwrap();
    let hb = viewer_dc.recv().await.unwrap().unwrap();
    assert!(matches!(hb, Message::Heartbeat(_)));
}
