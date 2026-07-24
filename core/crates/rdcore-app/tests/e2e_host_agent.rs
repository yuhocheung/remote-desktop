//! 端到端集成：真实 Host Agent 进程（rdcore-desktop）↔ Rust Viewer，经真实 signaling-svc。
//!
//! 这是「同机/局域网」级的 M3 预演：不起 iPhone/公网/TURN，但用**真实进程**（而非进程内回环）
//! 跑通「signaling 中介 → Host Agent 配对注册 → Viewer 带 token 建连 → 收 H.264 压缩帧」。
//! 提前暴露 A5（Agent 进程）、B2（token 库文件）、B3（URL 格式）三方在真实进程下的集成问题。
//!
//! 运行前提：`cargo build -p rdcore-desktop` 已产出可执行文件（测试内自动定位）。

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rdcore_app::Connection;
use rdcore_crypto::Ed25519CryptoProvider;
use rdcore_identity::{create_local_identity, IdentityStore, InMemoryIdentityStore};
use rdcore_proto::SessionId;
use rdcore_rtc::RtcConfig;
use tokio::process::{Child, Command};

/// 进程级串行锁：`SIGNALING_TOKEN_DB` 是进程全局环境变量，多个测试并行会互相覆盖
/// （信令服务器在每次握手时才读取它），导致 token 库错配、Viewer 被 401。
/// 所有起信令 + Agent 的测试都必须持本锁跑完全程。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 定位已构建的 rdcore-desktop 可执行文件（debug profile）。
fn agent_bin() -> Option<std::path::PathBuf> {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // core/crates/rdcore-app -> 仓库根 target/debug
    p.pop(); // rdcore-app
    p.pop(); // crates
    p.pop(); // core
    let exe = p.join("target").join("debug").join(if cfg!(windows) {
        "rdcore-desktop.exe"
    } else {
        "rdcore-desktop"
    });
    exe.exists().then_some(exe)
}

/// 起一个真实 signaling-svc（per-session token 模式 + token 库文件）。
async fn start_signaling(
    token_db: &std::path::Path,
) -> (std::net::SocketAddr, signaling_svc::TokenStore) {
    std::env::set_var("SIGNALING_TOKEN_DB", token_db);
    let store = signaling_svc::TokenStore::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = signaling_svc::SignalingConfig::per_session(store.clone());
    tokio::spawn(async move {
        let _ = signaling_svc::serve_listener_with_config(listener, cfg).await;
    });
    (addr, store)
}

/// 起真实 Host Agent 子进程（headless 抓屏，无需显示器），返回句柄与配对信息。
async fn spawn_agent(
    signal_url: &str,
    token_db: &std::path::Path,
    identity_dir: &std::path::Path,
) -> Result<Child, String> {
    let bin = agent_bin()
        .ok_or_else(|| "rdcore-desktop 未构建；先 `cargo build -p rdcore-desktop`".to_string())?;
    Command::new(bin)
        .arg("run")
        .arg("--headless") // 合成抓屏，无需显示器/注入权限
        .arg("--no-banner") // 测试环境不拉 OS 横幅进程
        .arg("--loopback") // 同机回环候选
        .arg("--signal")
        .arg(signal_url)
        .arg("--identity-dir")
        .arg(identity_dir)
        .arg("--identity-pass")
        .arg("test-pass")
        .env("SIGNALING_TOKEN_DB", token_db)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("启动 Host Agent 失败: {e}"))
}

/// 从 Agent 的 stdout 读出配对码（`session : <32hex>` 与 `token   : <64hex>`）。
///
/// 返回 `(session, token, ready)`：`ready` 在 Agent 打印「等待 Viewer 连接」
/// （= 已入信令房间开始等 Offer）时收到信号。Viewer 必须等 `ready` 再建连，
/// 否则 Viewer 先进房广播的 PeerHello 会丢给空房间，Host 后入房只见 Offer
/// 不见 Hello → TOFU 验签报 UnknownPeer（测试时序竞争，生产无此问题）。
///
/// 读出配对码后必须**继续后台排空 stdout**：Agent 随后还要打印终端二维码等输出，
/// 若管道无人读取，Windows 管道写满会让 Agent 阻塞，句柄关闭则直接
/// `failed printing to stdout` panic —— Agent 死在连信令之前（测试假失败）。
async fn read_pairing(
    stdout: tokio::process::ChildStdout,
) -> (SessionId, String, tokio::sync::oneshot::Receiver<()>) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut session: Option<SessionId> = None;
    let mut token: Option<String> = None;
    // 读若干行直到凑齐 session + token（超时由外层 timeout 兜底）。
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.trim().strip_prefix("║ session :") {
            let hex = rest.trim();
            let mut sid = [0u8; 16];
            for i in 0..16 {
                sid[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0);
            }
            session = Some(SessionId(sid));
        }
        if let Some(rest) = line.trim().strip_prefix("║ token   :") {
            token = Some(rest.trim().to_string());
        }
        if session.is_some() && token.is_some() {
            break;
        }
    }
    // 后台排空剩余输出（QR 图、状态行），直到 Agent 退出关闭管道；
    // 见到「等待 Viewer 连接」即通知 Agent 已入房。
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("等待 Viewer 连接") {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(());
                }
            }
        }
    });
    (
        session.expect("未从 Agent stdout 读到 session"),
        token.expect("未从 Agent stdout 读到 token"),
        ready_rx,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_agent_process_pairs_and_streams_to_viewer() {
    let _env_guard = ENV_LOCK.lock().unwrap();
    if agent_bin().is_none() {
        eprintln!("跳过：rdcore-desktop 未构建（cargo build -p rdcore-desktop 后重跑）");
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "rdcore_a5_e2e_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let token_db = tmp.join("tokens.jsonl");
    let identity_dir = tmp.join("identity");

    // 1) 起真实 signaling（per-session + token 库文件）。
    let (sig_addr, _store) = start_signaling(&token_db).await;
    let signal_url = format!("ws://{sig_addr}");

    // 2) 起真实 Host Agent 进程（headless 抓屏 + 注册配对）。
    let mut agent = spawn_agent(&signal_url, &token_db, &identity_dir)
        .await
        .expect("启动 Host Agent");
    let stdout = agent.stdout.take().expect("agent stdout");
    let (session, token, ready) = tokio::time::timeout(Duration::from_secs(15), read_pairing(stdout))
        .await
        .expect("读配对码超时");
    // 等 Agent 入信令房间后再建连（否则 Viewer 的 PeerHello 丢给空房 → Host 验签 UnknownPeer）。
    tokio::time::timeout(Duration::from_secs(15), ready)
        .await
        .expect("等 Agent 入房超时")
        .expect("Agent stdout 提前关闭");

    // 3) Viewer 用配对码构造信令 URL（wss://host/<hex>?token=<64hex>，与 B3 修复后格式一致），
    //    带一次性 token 连信令。
    let viewer_url = format!(
        "{}/{}?token={}",
        signal_url,
        session
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        token
    );

    // 4) Viewer 身份：需先知道 Host 身份才能验签。真实流程里 Viewer 经带外配对拿到 Host 身份；
    //    此处从 Agent 的 identity.json 读 Host 公钥身份（等价于"扫码导入"）。
    let id_file = std::fs::read_to_string(identity_dir.join("identity.json"))
        .expect("Agent 应已写 identity.json");
    let id_json: serde_json::Value = serde_json::from_str(&id_file).unwrap();
    let host_peer: rdcore_identity::PeerIdentity =
        serde_json::from_value(id_json["local"].clone()).expect("解析 Host 身份");

    let provider = Ed25519CryptoProvider;
    let (viewer_peer, viewer_sk) = create_local_identity(&provider, "viewer-e2e");
    let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
        Arc::new(StdMutex::new(InMemoryIdentityStore::new(viewer_peer)));
    viewer_store.lock().unwrap().remember(host_peer);

    // 同机回环：纯 host 候选 + 关 mDNS，无需 TURN。
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    // 5) Viewer 建连 + 收一帧（Host Agent 在 headless 下投合成 H.264 帧）。
    let viewer = Connection::new_viewer(
        &viewer_url,
        session,
        viewer_sk,
        viewer_store,
        rtc_cfg,
        Duration::from_secs(30),
    )
    .await
    .expect("Viewer 构造失败");

    let stop = Arc::new(AtomicBool::new(false));
    viewer
        .establish(stop.clone(), None)
        .await
        .expect("Viewer establish 失败");

    // 6) 收帧验证：Host Agent 应投出画面（H.264 或回退编码），像素经 E2E 解密。
    let frame = tokio::time::timeout(Duration::from_secs(20), viewer.recv_media())
        .await
        .expect("收帧超时")
        .expect("recv_media 出错")
        .expect("应收到一帧");
    assert!(frame.width > 0 && frame.height > 0, "帧应非空");
    assert!(!frame.data.is_empty(), "帧数据应非空");
    println!(
        "✓ 收到 Host Agent 帧：{}x{} codec={:?} bytes={}",
        frame.width,
        frame.height,
        frame.codec,
        frame.data.len()
    );

    stop.store(true, Ordering::SeqCst);
    let _ = agent.kill().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 回归：Viewer 断开后，用**同一配对码**重扫必须能再次建连（真实 Agent 进程 + 真实信令）。
///
/// 链路：Agent（常驻重连循环）← signaling-svc（配对不焚毁、文件即事实 reconcile）。
/// 两个 Viewer 使用不同身份、相同配对码先后建连，第二个必须成功收帧。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_agent_process_accepts_rescan_with_same_pairing() {
    let _env_guard = ENV_LOCK.lock().unwrap();
    if agent_bin().is_none() {
        eprintln!("跳过：rdcore-desktop 未构建（cargo build -p rdcore-desktop 后重跑）");
        return;
    }

    let tmp = std::env::temp_dir().join(format!(
        "rdcore_a5_rescan_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let token_db = tmp.join("tokens.jsonl");
    let identity_dir = tmp.join("identity");

    let (sig_addr, _store) = start_signaling(&token_db).await;
    let signal_url = format!("ws://{sig_addr}");

    let mut agent = spawn_agent(&signal_url, &token_db, &identity_dir)
        .await
        .expect("启动 Host Agent");
    let stdout = agent.stdout.take().expect("agent stdout");
    let (session, token, ready) = tokio::time::timeout(Duration::from_secs(15), read_pairing(stdout))
        .await
        .expect("读配对码超时");
    // 等 Agent 入信令房间后再建连（否则 Viewer 的 PeerHello 丢给空房 → Host 验签 UnknownPeer）。
    tokio::time::timeout(Duration::from_secs(15), ready)
        .await
        .expect("等 Agent 入房超时")
        .expect("Agent stdout 提前关闭");

    let id_file = std::fs::read_to_string(identity_dir.join("identity.json"))
        .expect("Agent 应已写 identity.json");
    let id_json: serde_json::Value = serde_json::from_str(&id_file).unwrap();
    let host_peer: rdcore_identity::PeerIdentity =
        serde_json::from_value(id_json["local"].clone()).expect("解析 Host 身份");

    let session_hex: String = session.0.iter().map(|b| format!("{b:02x}")).collect();
    let viewer_url = format!("{signal_url}/{session_hex}?token={token}");
    let rtc_cfg = RtcConfig {
        ice_servers: vec![],
        channel_buffer: 64,
        include_loopback: true,
        force_relay: false,
    };

    // 同一个配对码连两次（两个不同 Viewer 身份）：各自 establish + 收一帧，然后断开。
    for round in 1..=2 {
        let provider = Ed25519CryptoProvider;
        let (viewer_peer, viewer_sk) =
            create_local_identity(&provider, &format!("viewer-rescan-{round}"));
        let viewer_store: Arc<StdMutex<dyn IdentityStore + Send + Sync>> =
            Arc::new(StdMutex::new(InMemoryIdentityStore::new(viewer_peer)));
        viewer_store.lock().unwrap().remember(host_peer.clone());

        let viewer = Connection::new_viewer(
            &viewer_url,
            session,
            viewer_sk,
            viewer_store,
            rtc_cfg.clone(),
            Duration::from_secs(30),
        )
        .await
        .expect("Viewer 构造失败");

        let stop = Arc::new(AtomicBool::new(false));
        tokio::time::timeout(
            Duration::from_secs(45),
            viewer.establish(stop.clone(), None),
        )
        .await
        .unwrap_or_else(|_| panic!("第 {round} 次扫码 establish 超时"))
        .unwrap_or_else(|e| panic!("第 {round} 次扫码 establish 失败：{e:#}"));

        let frame = tokio::time::timeout(Duration::from_secs(20), viewer.recv_media())
            .await
            .unwrap_or_else(|_| panic!("第 {round} 次扫码收帧超时"))
            .expect("recv_media 出错")
            .expect("应收到一帧");
        assert!(frame.width > 0 && !frame.data.is_empty(), "第 {round} 次扫码帧应非空");
        println!("✓ 第 {round} 次扫码建连并收帧成功");
        // 断开本轮 Viewer（drop 即断），Agent 应回到「等待下一个 Viewer」。
        stop.store(true, Ordering::SeqCst);
        drop(viewer);
        // 给 Agent 一点回到等待态的时间（重扫路径本身会抢占，无需等死透）。
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let _ = agent.kill().await;
    let _ = std::fs::remove_dir_all(&tmp);
}
