//! Track B（韧性面）e2e：真实 WebRTC 连接上的连接生命周期监督（ConnectionSupervisor）。
//!
//! 验证三件事：
//! 1. 双方 supervisor 互发心跳后，相位保持 `Up`（心跳新鲜）。
//! 2. 一方停发心跳，对方在超时后进入 `Dead`（沉默判死，防僵尸会话）。
//! 3. 业务消息（非心跳 AppMessage）经 `recv_business` 收到，心跳被过滤、不与业务争抢。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use rdcore_app::connection_lifecycle::{ConnectionSupervisor, LinkPhase, SupervisorConfig};
use rdcore_app::{AppMessage, Connection};
use rdcore_consent::ConsentDecision;
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{InputEvent, InputKind, MouseButton, SessionId};
use rdcore_rtc::RtcConfig;
use signaling_svc::{serve_listener, session_hex};
use tokio::sync::Mutex;

async fn spawn_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve_listener(listener).await;
    });
    format!("ws://{addr}")
}

fn loopback_cfg() -> RtcConfig {
    RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    }
}

/// 建一对已 establish 的 Host/Viewer 连接（带外配对 + Host 授予全部）。
async fn establish_pair(url: &str, session: SessionId) -> (Arc<Connection>, Arc<Connection>) {
    let provider = Ed25519CryptoProvider;
    let (vp, vsk) = create_local_identity(&provider, "viewer");
    let (hp, hsk) = create_local_identity(&provider, "host");
    // A0：store 注入为 Arc<Mutex<dyn IdentityStore + Send + Sync>>（std::sync::Mutex）。
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(vp.clone())));
    viewer_store.lock().unwrap().remember(hp.clone());
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(hp.clone())));
    host_store.lock().unwrap().remember(vp.clone());

    let viewer = Arc::new(Mutex::new(
        Connection::new_viewer(
            url,
            session,
            vsk,
            viewer_store.clone(),
            loopback_cfg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));
    let host = Arc::new(Mutex::new(
        Connection::new_host(
            url,
            session,
            hsk,
            host_store.clone(),
            loopback_cfg(),
            Duration::from_secs(30),
        )
        .await
        .unwrap(),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [
            rdcore_consent::ConsentScope::View,
            rdcore_consent::ConsentScope::Input,
        ]
        .into_iter()
        .collect(),
        duration: None,
    };
    let v = viewer.clone();
    let h = host.clone();
    let s1 = stop.clone();
    let s2 = stop.clone();
    let vt = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let ht = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });
    let joined =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(vt, ht) }).await;
    assert!(joined.is_ok(), "establish 超时");
    let (vr, hr) = joined.unwrap();
    vr.unwrap().unwrap();
    hr.unwrap().unwrap();

    // establish 已改 `&self`；这里仍转为共享 Arc<Connection> 供 supervisor 使用。
    let viewer = Arc::try_unwrap(viewer)
        .ok()
        .expect("viewer 引用唯一")
        .into_inner();
    let host = Arc::try_unwrap(host)
        .ok()
        .expect("host 引用唯一")
        .into_inner();
    (Arc::new(viewer), Arc::new(host))
}

#[tokio::test]
async fn supervisor_keeps_up_with_heartbeats_and_filters_business() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([9u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));
    let (viewer, host) = establish_pair(&url, session).await;

    let cfg = SupervisorConfig {
        heartbeat_interval: Duration::from_millis(100),
        heartbeat_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let v_sup = ConnectionSupervisor::start(viewer.clone(), cfg.clone()).await;
    let h_sup = ConnectionSupervisor::start(host.clone(), cfg).await;

    // 1) 互发心跳 → 双方应保持 Up。
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(v_sup.phase(), LinkPhase::Up, "Viewer 心跳新鲜应 Up");
    assert_eq!(h_sup.phase(), LinkPhase::Up, "Host 心跳新鲜应 Up");

    // 2) Host 发一条业务消息（Input），Viewer 的 recv_business 应收到；心跳已被过滤。
    let input = AppMessage::Input(InputEvent {
        seq: 1,
        kind: InputKind::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        },
    });
    host.send_app(&input).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(3), v_sup.recv_business())
        .await
        .expect("recv_business 超时")
        .expect("应收到业务消息");
    assert_eq!(got, input, "业务消息应被转发、未被心跳环吃掉");

    // 3) Viewer 也发一条，Host 收到。
    viewer.send_app(&input).await.unwrap();
    let got2 = tokio::time::timeout(Duration::from_secs(3), h_sup.recv_business())
        .await
        .expect("recv_business 超时")
        .expect("Host 应收到业务消息");
    assert_eq!(got2, input);

    v_sup.stop();
    h_sup.stop();
}

#[tokio::test]
async fn supervisor_goes_dead_when_peer_silent() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([8u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));
    let (viewer, host) = establish_pair(&url, session).await;

    // Viewer 起 supervisor 发心跳；Host 只起 supervisor 但不发心跳（通过长 interval 模拟沉默）。
    let v_sup = ConnectionSupervisor::start(
        viewer.clone(),
        SupervisorConfig {
            heartbeat_interval: Duration::from_millis(100),
            heartbeat_timeout: Duration::from_millis(800),
            ..Default::default()
        },
    )
    .await;
    // Host 心跳间隔设得极长（近乎不发），Viewer 收不到 Host 心跳应判 Dead。
    let h_sup = ConnectionSupervisor::start(
        host.clone(),
        SupervisorConfig {
            heartbeat_interval: Duration::from_secs(3600),
            heartbeat_timeout: Duration::from_millis(800),
            ..Default::default()
        },
    )
    .await;

    // Viewer 在超时窗口内先 Up，随后因 Host 沉默超过 heartbeat_timeout 进入 Dead。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let initial = v_sup.phase();
    assert!(
        matches!(initial, LinkPhase::Up | LinkPhase::Degraded),
        "初始应在窗口内"
    );
    tokio::time::timeout(Duration::from_secs(3), v_sup.wait_dead())
        .await
        .expect("Host 沉默应在超时后判 Dead");
    assert_eq!(
        v_sup.phase(),
        LinkPhase::Dead,
        "Host 沉默超时后 Viewer 应判 Dead"
    );

    v_sup.stop();
    h_sup.stop();
}
