//! 端到端连接集成测试：用真实信令服务器 + 真实 WebRTC（localhost 回环）把两条
//! `Connection` 真正连起来，跑完「签名握手 → ICE → E2E 会话密钥 → 同意」全链路，
//! 再验证媒体像素与控制消息经端到端加密无损往返。
//!
//! 这是 P0–P7 全部库第一次被编排成一个**连贯、可运行、且加密**的远程桌面连接。

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use rdcore_app::{AppMessage, Connection};
use rdcore_consent::{ConnectionState, ConsentDecision, ConsentScope};
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{Heartbeat, MediaFrame, SessionId, VideoCodec};
use rdcore_rtc::RtcConfig;
use signaling_svc::{serve_listener, session_hex};
use tokio::sync::Mutex;

/// 在空闲端口启动信令服务器，返回其 `ws://` 基址。
async fn spawn_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_listener(listener).await;
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn full_connection_over_real_webrtc_with_e2e_encryption() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([7u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    // 带外配对：双方各自生成身份，并记住对端公钥（Ed25519 验签的前提）。
    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-laptop");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-desktop");

    // A0：store 改为 `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 注入（std::sync::Mutex）。
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    viewer_store.lock().unwrap().remember(host_peer.clone());
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));
    host_store.lock().unwrap().remember(viewer_peer.clone());

    // 同机回环：纯 host 候选 + 关 mDNS，无需 STUN/TURN。
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store.clone(),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store.clone(),
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let stop = Arc::new(AtomicBool::new(false));

    // Host 授权：View + Input（不含 Clipboard / FileTransfer）。
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    let v = viewer.clone();
    let h = host.clone();
    let s1 = stop.clone();
    let s2 = stop.clone();
    let v_task = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let h_task = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });

    // 整体 30s 超时防挂起（ICE / DTLS / 握手任一环节卡住都会暴露为超时）。
    let joined = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(v_task, h_task)
    })
    .await;
    assert!(
        joined.is_ok(),
        "establish 超时：ICE 未连通或握手卡住（检查 localhost 回环候选 / mDNS 配置）"
    );
    let (v_r, h_r) = joined.unwrap();
    v_r.unwrap().expect("Viewer establish 失败");
    h_r.unwrap().expect("Host establish 失败");

    let viewer = viewer.lock().await;
    let host = host.lock().await;

    // 1) 两端派生出相同的端到端会话密钥。
    let vk = viewer.session_key().expect("Viewer 应已建立会话密钥");
    let hk = host.session_key().expect("Host 应已建立会话密钥");
    assert_eq!(vk, hk, "两端应派生相同的端到端会话密钥");

    // 2) 同意门控生效：Host 激活且范围为授予的 View/Input。
    assert!(host.is_active(), "Host 应已激活");
    let scopes: HashSet<ConsentScope> = host.granted_scopes();
    assert!(scopes.contains(&ConsentScope::View), "应授予 View");
    assert!(scopes.contains(&ConsentScope::Input), "应授予 Input");
    assert!(
        !scopes.contains(&ConsentScope::Clipboard),
        "不应授予未授权的 Clipboard"
    );

    // 3) Viewer 正确反映为已激活。
    assert!(viewer.is_active(), "Viewer 应反映为已激活");

    // 3.5) P2P 连接状态应已进入 Connected（ICE + DTLS 成功、数据通道 open）。
    assert!(
        viewer.wait_connected(Duration::from_secs(5)).await,
        "Viewer 的 WebRTC 应在建立后进入 Connected 状态"
    );

    // 4) 不可伪造横幅数据：来自已认证对端、标明已加密。
    let ind = viewer.security_indicator().expect("Viewer 应有安全指示器");
    assert!(ind.encrypted, "指示器应标明已建立 E2E 加密");
    assert!(
        matches!(ind.state, ConnectionState::Active { .. }),
        "指示器状态应为 Active"
    );
    assert!(
        !ind.fingerprint_spaced.is_empty(),
        "指纹应来自已认证对端，非空"
    );

    // 5) 媒体像素经 E2E 加密往返且一致。
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0xABu8; 16 * 12 * 4],
    };
    host.send_media(&frame)
        .await
        .expect("Host 经 E2E 加密发媒体帧");
    let got = viewer
        .recv_media()
        .await
        .unwrap()
        .expect("Viewer 应收到媒体帧");
    assert_eq!(got, frame, "媒体像素应经端到端加密往返且一致");

    // 6) 控制消息（心跳）经 E2E 加密往返且一致。
    let hb = AppMessage::Heartbeat(Heartbeat {
        seq: 3,
        timestamp_ms: 42,
    });
    host.send_app(&hb)
        .await
        .expect("Host 经 E2E 加密发控制消息");
    let got = viewer
        .recv_app()
        .await
        .unwrap()
        .expect("Viewer 应收到控制消息");
    assert_eq!(got, hb, "控制消息应经端到端加密往返且一致");
}

/// A0 回归：断线后 `reconnect(&self)` 原地换出 `WebRtcPeer` 并重跑握手 / 密钥 / 同意，
/// 两端并发触发（模拟 B5 supervisor 在网络断开后同时重连），验证：
/// 1) 旧 WebRtcPeer 被换出、新 PeerConnection 建立成功；
/// 2) 重连派生**全新**端到端会话密钥（旧密钥不复用）；
/// 3) 重连后媒体仍经 E2E 加密无损往返。
#[tokio::test]
async fn reconnect_rebuilds_peer_and_reestablishes_e2e() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([3u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-laptop");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-desktop");
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    viewer_store.lock().unwrap().remember(host_peer.clone());
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));
    host_store.lock().unwrap().remember(viewer_peer.clone());

    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store.clone(),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store.clone(),
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    // 首次 establish（两端并发）。
    let v = viewer.clone();
    let h = host.clone();
    let s1 = Arc::new(AtomicBool::new(false));
    let s2 = s1.clone();
    let vt = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let ht = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });
    let joined =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(vt, ht) }).await;
    assert!(joined.is_ok(), "首次 establish 超时");
    let (vr, hr) = joined.unwrap();
    vr.unwrap().expect("Viewer 首次 establish 失败");
    hr.unwrap().expect("Host 首次 establish 失败");

    // 首次媒体往返（E2E 加密）。
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0xABu8; 16 * 12 * 4],
    };
    host.lock().await.send_media(&frame).await.unwrap();
    let got = viewer.lock().await.recv_media().await.unwrap().unwrap();
    assert_eq!(got, frame, "首次媒体应无损往返");
    let old_hk = host.lock().await.session_key().expect("Host 首次会话密钥");

    // 断线重建：两端并发 reconnect（模拟 B5 supervisor 触发两端同时重连）。
    let v = viewer.clone();
    let h = host.clone();
    let vt = tokio::spawn(async move { v.lock().await.reconnect().await });
    let ht = tokio::spawn(async move { h.lock().await.reconnect().await });
    let joined =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(vt, ht) }).await;
    assert!(joined.is_ok(), "reconnect 超时");
    let (vr, hr) = joined.unwrap();
    vr.unwrap().expect("Viewer reconnect 失败");
    hr.unwrap().expect("Host reconnect 失败");

    // 重连后：新会话密钥应不同于旧（证明 WebRtcPeer 被原地换出、ECDH 重新派生）。
    let new_hk = host
        .lock()
        .await
        .session_key()
        .expect("Host 重连后会话密钥");
    assert_ne!(old_hk, new_hk, "重连应派生全新会话密钥");

    // 重连后媒体仍应无损往返。
    let frame2 = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0xCDu8; 16 * 12 * 4],
    };
    host.lock().await.send_media(&frame2).await.unwrap();
    let got2 = viewer.lock().await.recv_media().await.unwrap().unwrap();
    assert_eq!(got2, frame2, "重连后媒体应仍无损往返");
}

/// 回归：Host 的「接受 → 失败 → 重试」常驻循环在**首次 establish 从未成功**时也走
/// `reconnect` 路径，此时 `last_host_decision` 为空，裸 `reconnect` 会把 Host 的授权
/// 决定退化成自动 `Deny`（`establish` 内的 `unwrap_or`）。`reconnect_with` 显式播种
/// 授权决定，保证首连失败后的每次重试仍按配置授权（而非拒绝 Viewer）。
#[tokio::test]
async fn reconnect_with_before_any_success_still_grants() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([0x33u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-phone");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-pc");
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    viewer_store.lock().unwrap().remember(host_peer.clone());
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));
    host_store.lock().unwrap().remember(viewer_peer.clone());

    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store,
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store,
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    // Host 首个握手动作就是 reconnect_with（模拟「首连失败 → 2s 退避 → 重连」的循环路径，
    // 此刻 last_host_decision 为 None）；Viewer 正常 establish。
    let v = viewer.clone();
    let h = host.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let vt = tokio::spawn(async move { v.lock().await.establish(stop, None).await });
    let ht = tokio::spawn(async move { h.lock().await.reconnect_with(decision).await });
    let joined =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(vt, ht) }).await;
    assert!(joined.is_ok(), "reconnect_with 首连超时");
    let (vr, hr) = joined.unwrap();
    vr.unwrap().expect("Viewer establish 失败");
    hr.unwrap().expect("Host reconnect_with 失败");

    // 关键断言：Viewer 收到的是 Grant（而非 Deny 退化），且权限范围与配置一致。
    let v = viewer.lock().await;
    assert!(v.is_active(), "reconnect_with 应下发 Grant 而非 Deny");
    let scopes = v.granted_scopes();
    assert!(scopes.contains(&ConsentScope::View));
    assert!(scopes.contains(&ConsentScope::Input));
}

/// 重扫抢占：Host 会话存续期间，新 Viewer 扫同一二维码（同 session）发起连接，
/// `wait_peer_gone_or_rescan` 应立即返回 `Rescan`（不等旧 Viewer 的 ICE 掉线检测），
/// 随后 `reconnect_with` 原地完成与新 Viewer 的握手，媒体恢复。
#[tokio::test]
async fn rescan_preempts_without_waiting_for_ice_timeout() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([0x44u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (v1_peer, v1_sk) = create_local_identity(&provider, "viewer-old");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-pc");
    let mk_store = |local: rdcore_identity::PeerIdentity,
                    peer: rdcore_identity::PeerIdentity| {
        let s: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
            Arc::new(StdMutex::new(InMemoryIdentityStore::new(local)));
        s.lock().unwrap().remember(peer);
        s
    };
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            mk_store(host_peer.clone(), v1_peer.clone()),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let viewer1 = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            v1_sk,
            mk_store(v1_peer.clone(), host_peer.clone()),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View].into_iter().collect(),
        duration: None,
    };

    // 首连：viewer1 正常 establish。
    let v = viewer1.clone();
    let h = host.clone();
    let s1 = Arc::new(AtomicBool::new(false));
    let s2 = s1.clone();
    let vt = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let d1 = decision.clone();
    let ht = tokio::spawn(async move { h.lock().await.establish(s2, Some(d1)).await });
    let joined =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(vt, ht) }).await;
    assert!(joined.is_ok(), "首连超时");
    let (vr, hr) = joined.unwrap();
    vr.unwrap().expect("viewer1 establish 失败");
    hr.unwrap().expect("host establish 失败");

    // Host 进入「等掉线 / 等重扫」；viewer1 保持连接但静默（不触发 ICE 超时）。
    let h = host.clone();
    let d2 = decision.clone();
    let host_wait = tokio::spawn(async move {
        let outcome = h.lock().await.wait_peer_gone_or_rescan().await;
        assert_eq!(
            outcome,
            rdcore_app::HostWaitOutcome::Rescan,
            "viewer2 的 PeerHello/Offer 应立即触发 Rescan"
        );
        h.lock().await.reconnect_with(d2).await
    });

    // viewer2（另一台设备）扫同一二维码建立连接。
    let (v2_peer, v2_sk) = create_local_identity(&provider, "viewer-new");
    let viewer2 = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            v2_sk,
            mk_store(v2_peer, host_peer),
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let v = viewer2.clone();
    let s3 = Arc::new(AtomicBool::new(false));
    let v2_task = tokio::spawn(async move { v.lock().await.establish(s3, None).await });

    let joined = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(host_wait, v2_task)
    })
    .await;
    assert!(joined.is_ok(), "重扫抢占握手超时");
    let (hr, vr) = joined.unwrap();
    hr.unwrap().expect("host reconnect_with 失败");
    vr.unwrap().expect("viewer2 establish 失败");

    // 新会话可用：viewer2 已获授权，Host→viewer2 媒体无损往返。
    assert!(viewer2.lock().await.is_active());
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0x7Eu8; 16 * 12 * 4],
    };
    host.lock().await.send_media(&frame).await.unwrap();
    let got = viewer2.lock().await.recv_media().await.unwrap().unwrap();
    assert_eq!(got, frame, "重扫后媒体应无损往返");
}

/// 配对身份交换（PeerHello）：双方**不预配对**（store 互不认识），
/// 握手时经 `Message::PeerHello` 自动交换公开身份并按 TOFU 记住，验签照常通过。
/// 这是真实配对流程（扫码/输码 → 一次性 token 会话）所依赖的路径——此前
/// 生产链路缺少身份交换，真机握手必报 `UnknownPeer`。
#[tokio::test]
async fn pairing_peer_hello_exchanges_identities_without_prepairing() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([9u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-phone");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-pc");

    // 注意：双方 store 只含本机身份，**互不 remember**——模拟真实首次配对。
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));

    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store.clone(),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store.clone(),
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    let v = viewer.clone();
    let h = host.clone();
    let s1 = stop.clone();
    let s2 = stop.clone();
    let v_task = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let h_task = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });

    let joined = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(v_task, h_task)
    })
    .await;
    assert!(joined.is_ok(), "establish 超时");
    let (v_r, h_r) = joined.unwrap();
    v_r.unwrap().expect("Viewer establish 失败");
    h_r.unwrap().expect("Host establish 失败");

    // TOFU：两端都已把对端身份记入 store（公钥可用于后续重连验签）。
    assert!(
        host_store.lock().unwrap().lookup(&viewer_peer.id).is_some(),
        "Host 应经 PeerHello 记住 Viewer 身份"
    );
    assert!(
        viewer_store.lock().unwrap().lookup(&host_peer.id).is_some(),
        "Viewer 应经 PeerHello 记住 Host 身份"
    );

    // 会话密钥一致（验签确实通过，而非跳过）。
    let vk = viewer.lock().await.session_key().expect("Viewer 会话密钥");
    let hk = host.lock().await.session_key().expect("Host 会话密钥");
    assert_eq!(vk, hk, "两端应派生相同的端到端会话密钥");
}

/// PeerHello 竞态回归：Host 先 establish（配对广播时房间无人），Viewer 延迟数秒才进房。
/// 生产场景即如此——Host 常驻等连接，其开场 PeerHello 必然发在 Viewer 进房之前；
/// Host 须在回 Answer 前重发身份，否则 Viewer verify_answer 报 UnknownPeer。
#[tokio::test]
async fn pairing_peer_hello_host_waiting_before_viewer_joins() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([10u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-phone");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-pc");

    // 双方 store 只含本机身份（不预配对），模拟真实首次配对。
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));

    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    // Host 先建连并进入等待（其开场 PeerHello 落在空房间里）。
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            &url,
            session,
            host_sk,
            host_store.clone(),
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };
    let h = host.clone();
    let s2 = stop.clone();
    let h_task = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });

    // Viewer 延迟 2s 才进房（Host 的开场广播早已发出且无人接收）。
    tokio::time::sleep(Duration::from_secs(2)).await;
    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            &url,
            session,
            viewer_sk,
            viewer_store.clone(),
            rtc_cfg,
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let v = viewer.clone();
    let s1 = stop.clone();
    let v_task = tokio::spawn(async move { v.lock().await.establish(s1, None).await });

    let joined = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(v_task, h_task)
    })
    .await;
    assert!(joined.is_ok(), "establish 超时");
    let (v_r, h_r) = joined.unwrap();
    v_r.unwrap().expect("Viewer establish 失败");
    h_r.unwrap().expect("Host establish 失败");

    assert!(
        host_store.lock().unwrap().lookup(&viewer_peer.id).is_some(),
        "Host 应经 PeerHello 记住 Viewer 身份"
    );
    assert!(
        viewer_store.lock().unwrap().lookup(&host_peer.id).is_some(),
        "Viewer 应经 Host 重发的 PeerHello 记住其身份"
    );
    let vk = viewer.lock().await.session_key().expect("Viewer 会话密钥");
    let hk = host.lock().await.session_key().expect("Host 会话密钥");
    assert_eq!(vk, hk, "两端应派生相同的端到端会话密钥");
}
