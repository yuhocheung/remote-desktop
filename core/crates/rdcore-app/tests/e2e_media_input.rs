//! Track A 媒体面端到端集成测试：用真实信令服务器 + 真实 WebRTC（localhost 回环）把两条
//! `Connection` 真正连起来，验证 **媒体面全链路**：
//!
//! Host 抓取（NullCaptureSource 纯色帧）→ RawEncoder 编码 → `send_media`（像素 E2E 加密）
//!   → 真实 WebRTC 媒体通道 → Viewer `recv_rendered`（E2E 解密 → RawDecoder 解码 → render）
//!   → 断言 RGBA 与捕获完全一致；
//! 以及 **输入面全链路**：
//! Viewer `send_input`（经 E2E 加密控制通道）→ Host `recv_input` 收到同一 `InputEvent`。
//!
//! 这是 Track A 媒体/输入 API（`start_capture` / `recv_rendered` / `send_input` / `recv_input`）
//! 第一次在真实传输上被端到端验证。HostMediaPump 的单元行为另由 `host_media.rs` 单测覆盖。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use rdcore_app::{AppMessage, Connection, HostMediaPump};
use rdcore_capture::NullCaptureSource;
use rdcore_consent::{ConsentDecision, ConsentScope};
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore, PeerIdentity};
use rdcore_proto::{InputEvent, InputKind, MouseButton, SessionId};
use rdcore_rtc::RtcConfig;
use signaling_svc::{serve_listener, session_hex};
use tokio::sync::broadcast;

/// A0：把 `InMemoryIdentityStore` 包成 `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 注入 Connection。
fn make_store(
    self_id: PeerIdentity,
    peer: &PeerIdentity,
) -> Arc<StdMutex<dyn IdentityStore + Send + Sync>> {
    let s: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(self_id)));
    s.lock().unwrap().remember(peer.clone());
    s
}

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
async fn media_capture_to_render_and_input_roundtrip_over_real_webrtc() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([9u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    // 带外配对：双方各自生成身份并记住对端公钥（Ed25519 验签前提）。
    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-laptop");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-desktop");
    let viewer_store = make_store(viewer_peer.clone(), &host_peer);
    let host_store = make_store(host_peer.clone(), &viewer_peer);

    // 同机回环：纯 host 候选 + 关 mDNS，无需 STUN/TURN。
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    // 两个对等端并发跑完整握手（签名 / ICE / E2E 密钥 / 同意），完成后各自收进 Arc<Connection>。
    let (host, viewer) = tokio::join!(
        async {
            let c = Connection::new_host(
                &url,
                session,
                host_sk,
                host_store.clone(),
                rtc_cfg.clone(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            c.establish(stop.clone(), Some(decision))
                .await
                .expect("Host establish 失败");
            Arc::new(c)
        },
        async {
            let c = Connection::new_viewer(
                &url,
                session,
                viewer_sk,
                viewer_store.clone(),
                rtc_cfg.clone(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            c.establish(stop.clone(), None)
                .await
                .expect("Viewer establish 失败");
            Arc::new(c)
        },
    );

    assert!(host.is_active(), "Host 应已激活");
    assert!(viewer.is_active(), "Viewer 应反映为已激活");

    // ── 媒体面：Host 抓屏 → 编码 → E2E 加密 → 真实 WebRTC → Viewer 解密/解码/渲染 ──
    let width = 64u32;
    let height = 48u32;
    let frames = 10u32;
    let color = 0xABu8;
    let capture = NullCaptureSource::new(width, height, frames, color);
    let mut pump: HostMediaPump = Arc::clone(&host).start_capture(|| capture, 30);

    // 生产形态对齐：真实 Host 有常驻输入消费环（supervisor / 输入注入循环）持续消费
    // 控制通道。P 帧流下 Viewer 的关键帧请求（启动 IDR 被直播队列挤掉 / 参考链损坏）
    // 也走这条通道——无人消费 Host 永远不知道要补 IDR，Viewer 整段 GOP 不可解。
    // 测试用 mpsc 转发：后台任务独占 recv_input，末段输入断言改从 mpsc 读取，
    // 既不丢事件、又让关键帧请求在视频阶段即被消费（自愈链路端到端生效）。
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputEvent>(8);
    let host_for_input = Arc::clone(&host);
    let input_pump = tokio::spawn(async move {
        loop {
            match host_for_input.recv_input().await {
                Ok(Some(ev)) => {
                    if input_tx.send(ev).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    // RTP 是尽力传输（gap J 起视频走 RTP 轨道）：首帧可能因建连/调度竞态被直播队列
    // 挤掉（丢旧保新），「逐帧精确计数」不再成立。改为：整体死线内尽量多收，逐帧校验
    // 尺寸/颜色，健康回环下至少收到一半——验证的是「端到端能解密/解码/渲染」而非
    // 可靠传输（可靠传输语义由 DataChannel 回退路径的单测覆盖）。
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut got = 0u32;
    while got < frames && std::time::Instant::now() < deadline {
        let rendered =
            match tokio::time::timeout(Duration::from_secs(5), viewer.recv_rendered()).await {
                Ok(r) => r
                    .expect("recv_rendered 不应出错")
                    .expect("Viewer 应收到已渲染帧"),
                Err(_) => break, // 收帧超时：源已发完且有帧被挤掉，停止计数
            };
        assert_eq!(rendered.width, width, "渲染宽度应与捕获一致");
        assert_eq!(rendered.height, height, "渲染高度应与捕获一致");
        assert_eq!(
            rendered.rgba.len(),
            (width * height * 4) as usize,
            "RGBA 缓冲长度 = w*h*4"
        );
        // 默认编解码器现为 H.264（见 item #3 修复：生产不再用 Raw，否则 1280×720 帧超
        // WebRTC SCTP 单消息上限）。H.264 为有损且不携带 alpha 通道：解码后 alpha 固定为 255
        // （屏幕捕获本应不透明），R/G/B 三通道对纯色有极小偏差。故只校验 R/G/B 接近原始纯色
        // 0xAB（容差内），并断言 alpha 为不透明 255——足以证明媒体面端到端往返可用。
        let mut max_rgb_dev = 0i32;
        let mut alpha_opaque = true;
        for px in rendered.rgba.chunks_exact(4) {
            let (r, g, b, a) = (px[0] as i32, px[1] as i32, px[2] as i32, px[3]);
            max_rgb_dev = max_rgb_dev.max((r - color as i32).abs());
            max_rgb_dev = max_rgb_dev.max((g - color as i32).abs());
            max_rgb_dev = max_rgb_dev.max((b - color as i32).abs());
            if a != 255 {
                alpha_opaque = false;
            }
        }
        assert!(
            alpha_opaque,
            "解码后 alpha 应为不透明 255（H.264 不带 alpha 通道）"
        );
        assert!(
            max_rgb_dev <= 12,
            "R/G/B 应接近捕获纯色 0xAB（H.264 有损，容差 12），实际最大偏差={max_rgb_dev}"
        );
        got += 1;
    }
    assert!(
        got >= frames / 2,
        "RTP 尽力传输下健康回环至少应收到一半帧，实际 {got}/{frames}"
    );
    pump.stop().await;

    // ── 输入面：Viewer → Host 经 E2E 加密控制通道 ──
    let ev = InputEvent {
        seq: 1,
        kind: InputKind::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        },
    };
    viewer
        .send_input(&ev)
        .await
        .expect("Viewer 经 E2E 加密发输入事件");
    // 经后台输入消费环（生产形态）转发而来；超时兜底防回归时挂死。
    let got_ev = tokio::time::timeout(Duration::from_secs(10), input_rx.recv())
        .await
        .expect("收输入事件超时")
        .expect("输入消费环应存活");
    assert_eq!(got_ev, ev, "输入事件应经端到端加密往返且一致");
    input_pump.abort();
}

/// P0（契约 §9）回归：挂上 supervisor 风格的 `broadcast` 业务通道后，`recv_input` 必须改走
/// 该通道、不再碰 `recv_app`，从而与 supervisor 独占的 `recv_app` 心跳环互不争抢。
///
/// 这里不真正启动 `ConnectionSupervisor`（那会引入 Track B 的实时心跳），而是直接模拟其集成
/// 接缝：调用 `set_business_receiver` 注入一个 `broadcast` 订阅者，再从对应 `Sender` 发一条
/// `Input`，断言 `recv_input` 经业务通道收到、且不消耗 `recv_app` 队列。
#[tokio::test]
async fn recv_input_routes_via_injected_business_channel() {
    // 显式安装 rustls CryptoProvider（ring），规避 0.23 的 from_crate_features 歧义。
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = spawn_server().await;
    let session = SessionId([7u8; 16]);
    let url = format!("{base}/{}", session_hex(&session));

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-laptop");
    let (host_peer, host_sk) = create_local_identity(&provider, "host-desktop");
    let viewer_store = make_store(viewer_peer.clone(), &host_peer);
    let host_store = make_store(host_peer.clone(), &viewer_peer);

    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input]
            .into_iter()
            .collect(),
        duration: None,
    };

    let (host, _viewer) = tokio::join!(
        async {
            let c = Connection::new_host(
                &url,
                session,
                host_sk,
                host_store.clone(),
                rtc_cfg.clone(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            c.establish(stop.clone(), Some(decision))
                .await
                .expect("Host establish 失败");
            Arc::new(c)
        },
        async {
            let c = Connection::new_viewer(
                &url,
                session,
                viewer_sk,
                viewer_store.clone(),
                rtc_cfg.clone(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            c.establish(stop.clone(), None)
                .await
                .expect("Viewer establish 失败");
            Arc::new(c)
        },
    );

    // ── 模拟 supervisor 集成接缝：注入 broadcast 业务通道 ──
    let (biz_tx, biz_rx) = broadcast::channel::<AppMessage>(64);
    host.set_business_receiver(biz_rx).await;

    // 经业务通道发一条 Input（模拟 supervisor 从 recv_app 环转发出来）。
    let ev = InputEvent {
        seq: 42,
        kind: InputKind::Key {
            key_code: 13,
            pressed: true,
            modifiers: 0,
        },
    };
    assert!(
        biz_tx.send(AppMessage::Input(ev.clone())).is_ok(),
        "业务通道应成功转发 Input"
    );

    let got = host
        .recv_input()
        .await
        .expect("recv_input 不应出错")
        .expect("Host 应经业务通道收到输入事件");
    assert_eq!(got, ev, "recv_input 应路由到注入的 broadcast 业务通道");

    // 非 Input 业务消息应被忽略、不阻塞 recv_input（这里再发一条非 Input，仍应收不到也不断开）。
    let _ = biz_tx.send(AppMessage::Heartbeat(rdcore_proto::Heartbeat {
        seq: 1,
        timestamp_ms: 0,
    }));
    // 发第二条 Input，确认通道持续可用。
    let ev2 = InputEvent {
        seq: 43,
        kind: InputKind::MouseMove { x: 11, y: 22 },
    };
    let _ = biz_tx.send(AppMessage::Input(ev2.clone()));
    let got2 = host
        .recv_input()
        .await
        .expect("第二次 recv_input 不应出错")
        .expect("应收到第二条 Input");
    assert_eq!(got2, ev2, "业务通道第二条 Input 也应被 recv_input 收到");
}
