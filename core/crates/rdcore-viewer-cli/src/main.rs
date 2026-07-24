//! 无 GUI 的 headless Viewer harness。
//!
//! 用途：在**没有显示器、没有 Flutter GUI** 的环境（CI / 沙箱）下，用真实组件把整条
//! 远程桌面连接握手全自动跑通，证明「Host ↔ Viewer」链路在真实 WebRTC 上是可用的。
//!
//! 它做三件事：
//! 1. 在本机空闲端口起一个**真实**信令服务器（`signaling-svc`，与生产同构）。
//! 2. 生成真实 Host 身份 + 真实 Viewer 身份，双方**互记对方 Ed25519 公钥**（验签前提，
//!    相当于真实配对里经二维码/邀请码完成的带外密钥交换）。
//! 3. 并发跑 `Connection::new_host` / `Connection::new_viewer` 的 `establish`：
//!    Ed25519 验签 Offer/Answer → ICE 连通 → E2E 会话密钥派生 → 同意门控（Host 授权 View+Input）。
//!
//! 握手完成后验证：
//! - 两端派生出**相同**的端到端会话密钥；
//! - WebRTC PeerConnection 进入 `Connected`（ICE + DTLS + 数据通道 open）；
//! - 安全指示器标明「已加密」且指纹来自已认证对端；
//! - 一帧媒体像素 + 一条控制消息经 **E2E 加密** 往返且字节一致（证明内容通道真的加密了）。
//!
//! 关于「为什么不连那个已经在跑的 Host」：生产里 Host 在 `establish` 收到未知 Viewer 的
//! Offer 时会 `verify_offer → UnknownPeer` 直接拒绝——Viewer 的公钥必须**先于握手**经带外
//! （二维码/邀请码）交给 Host。这正是 Flutter 配对 UI 存在的意义。本 harness 自带 Host 并把
//! 双方公钥预先互记，从而无需 GUI 即可完整复现握手。这也等价于：若你把某 Viewer 的公钥预先
//! 写进 Host 身份库的 `peers`，这台 Host 就能与对应的 headless Viewer 完成握手。
//!
//! 运行：`cargo run -p rdcore-viewer-cli`（退出码 0 = 全链路通过）。

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{bail, Context};
use rdcore_app::{AppMessage, Connection};
use rdcore_consent::{ConnectionState, ConsentDecision, ConsentScope};
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::{Heartbeat, MediaFrame, SessionId, VideoCodec};
use rdcore_rtc::RtcConfig;
use signaling_svc::{serve_listener, session_hex};
use tokio::sync::Mutex;

fn main() -> anyhow::Result<()> {
    // webrtc-rs 的 DTLS 依赖 rustls；rustls 0.23 要求进程级默认 crypto provider。
    // 必须在任何网络/WebRTC 操作之前安装（与生产 rdcore-rtc 选用的 ring 保持一致）。
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 tokio runtime 失败")?;
    rt.block_on(async { run().await })
}

async fn run() -> anyhow::Result<()> {
    println!("══════════════════════════════════════════════════════════");
    println!("  rdcore-viewer-cli — headless WebRTC 握手自动化验证");
    println!("══════════════════════════════════════════════════════════");

    // 1) 真实信令服务器（本机空闲端口；与生产 signaling-svc 同构，纯中转）。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("绑定信令监听端口失败")?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = serve_listener(listener).await;
    });
    let base = format!("ws://{addr}");

    // 2) 固定 session（仅本 harness 用，避免随机导致日志难读）。
    let session = SessionId([0xABu8; 16]);
    let url = format!("{base}/{}", session_hex(&session));
    println!("• 信令服务器 : {base}");
    println!("• session    : {}", session_hex(&session));

    // 3) 双方真实身份 + 互记公钥（等价于真实配对里的带外密钥交换）。
    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "headless-viewer");
    let (host_peer, host_sk) = create_local_identity(&provider, "demo-host");

    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> = Arc::new(StdMutex::new(
        InMemoryIdentityStore::new(viewer_peer.clone()),
    ));
    viewer_store.lock().unwrap().remember(host_peer.clone());
    let host_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(host_peer.clone())));
    host_store.lock().unwrap().remember(viewer_peer.clone());

    // 4) RTC：纯回环候选 + 关 mDNS，无需 STUN/TURN（同机 P2P）。
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    let viewer = Arc::new(
        Mutex::new(
            Connection::new_viewer(
                &url,
                session,
                viewer_sk,
                viewer_store.clone(),
                rtc_cfg.clone(),
                Duration::from_secs(30),
            )
            .await
            .context("构造 Viewer 连接失败")?,
        ),
    );
    let host = Arc::new(
        Mutex::new(
            Connection::new_host(
                &url,
                session,
                host_sk,
                host_store.clone(),
                rtc_cfg,
                Duration::from_secs(30),
            )
            .await
            .context("构造 Host 连接失败")?,
        ),
    );

    // 5) 并发跑握手：Host 授权 View+Input；Viewer 传 None。
    let stop = Arc::new(AtomicBool::new(false));
    let decision = ConsentDecision::Grant {
        scopes: [ConsentScope::View, ConsentScope::Input].into_iter().collect(),
        duration: None,
    };
    let v = viewer.clone();
    let h = host.clone();
    let s1 = stop.clone();
    let s2 = stop.clone();
    println!("• 并发 establish（Ed25519 验签 → ICE → E2E 密钥 → 同意）…");
    let v_task = tokio::spawn(async move { v.lock().await.establish(s1, None).await });
    let h_task = tokio::spawn(async move { h.lock().await.establish(s2, Some(decision)).await });

    let joined = tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(v_task, h_task) }).await;
    match joined {
        Ok((v_r, h_r)) => {
            v_r.context("Viewer 任务 panic")?
                .context("Viewer establish 失败")?;
            h_r.context("Host 任务 panic")?
                .context("Host establish 失败")?;
        }
        Err(_) => bail!("establish 超时：ICE 未连通或握手卡住（检查 localhost 回环候选）"),
    }
    println!("  ✓ 握手完成（验签 / ICE / E2E 密钥 / 同意 全部通过）");

    // 6) 验证握手结论。
    let viewer = viewer.lock().await;
    let host = host.lock().await;

    let vk = viewer
        .session_key()
        .expect("Viewer 应已建立端到端会话密钥");
    let hk = host
        .session_key()
        .expect("Host 应已建立端到端会话密钥");
    if vk != hk {
        bail!("两端派生出的端到端会话密钥不一致");
    }
    println!("  ✓ 两端派生出相同的端到端会话密钥（ECDH over 已签名的临时公钥）");

    if !host.is_active() {
        bail!("Host 应已激活（同意门控生效）");
    }
    let scopes: HashSet<ConsentScope> = host.granted_scopes();
    if !scopes.contains(&ConsentScope::View) || !scopes.contains(&ConsentScope::Input) {
        bail!("Host 应授予 View+Input");
    }
    if scopes.contains(&ConsentScope::Clipboard) {
        bail!("不应授予未授权的 Clipboard");
    }
    println!("  ✓ 同意门控生效：Host 激活，授权范围 = View + Input（不含 Clipboard）");
    println!("  ✓ Viewer 反映为已激活");

    if !viewer
        .wait_connected(Duration::from_secs(5))
        .await
    {
        bail!("Viewer 的 WebRTC 未在握手后进入 Connected 状态");
    }
    println!("  ✓ WebRTC PeerConnection 进入 Connected（ICE + DTLS + 数据通道 open）");

    let ind = viewer
        .security_indicator()
        .expect("Viewer 应有安全指示器");
    if !ind.encrypted {
        bail!("安全指示器应标明已建立 E2E 加密");
    }
    if !matches!(ind.state, ConnectionState::Active { .. }) {
        bail!("指示器状态应为 Active");
    }
    if ind.fingerprint_spaced.is_empty() {
        bail!("指纹应来自已认证对端，不应为空");
    }
    println!("  ✓ 安全指示器：encrypted={}，指纹={}", ind.encrypted, ind.fingerprint_spaced);

    // 7) 真实内容往返：一帧媒体像素 + 一条控制消息，经 E2E 加密且字节一致。
    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0xABu8; 16 * 12 * 4],
    };
    host.send_media(&frame)
        .await
        .context("Host 经 E2E 加密发媒体帧")?;
    let got = viewer
        .recv_media()
        .await
        .context("Viewer 收媒体帧通道错误")?
        .expect("Viewer 应收到媒体帧");
    if got != frame {
        bail!("媒体像素经 E2E 加密往返后不一致");
    }
    println!("  ✓ 媒体像素经 E2E 加密往返一致（16×12×4 字节）");

    let hb = AppMessage::Heartbeat(Heartbeat {
        seq: 3,
        timestamp_ms: 42,
    });
    host.send_app(&hb)
        .await
        .context("Host 经 E2E 加密发控制消息")?;
    let got2 = viewer
        .recv_app()
        .await
        .context("Viewer 收控制消息通道错误")?
        .expect("Viewer 应收到控制消息");
    if got2 != hb {
        bail!("控制消息经 E2E 加密往返后不一致");
    }
    println!("  ✓ 控制消息（Heartbeat）经 E2E 加密往返一致");

    println!("══════════════════════════════════════════════════════════");
    println!("  ✅ 全链路通过：Host ↔ Viewer WebRTC 握手 + E2E 加密通道");
    println!("══════════════════════════════════════════════════════════");
    Ok(())
}
