//! Host Agent 编排（A5.3）：把配对 / 信令 / WebRTC / 同意 / 捕获 / 注入 / 横幅串成一条可运行链路。
//!
//! 与架构文档 §1/§5 一致：信令仅传 SDP/ICE，媒体像素走 `MediaChannel`、输入走 `DataChannel`，
//! 云端控制面永远看不到内容；不可伪造横幅的数据源来自 P4/P5 的密码学结论（`SecurityIndicator`）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rdcore_app::connection_lifecycle::{
    identity_store_handle, load_or_create_persistent_identity, IdentityPersistenceConfig,
};
use rdcore_app::{Connection, HostMediaPump};
use rdcore_banner::{BannerClient, BannerPhase, BannerState};
use rdcore_capture::{HostInputInjector, NullCaptureSource, NullInputInjector};
use rdcore_consent::{ConsentDecision, ConsentScope};
use rdcore_proto::SessionId;
use rdcore_rtc::RtcConfig;
use tokio::task::JoinHandle;

use crate::banner::{banner_client, spawn_banner};
use crate::config::AgentConfig;
use crate::token_db::{clear_token_file, register_token_file, TOKEN_FILE_HEARTBEAT};

/// Host Agent 主流程。
///
/// 1. 生成配对邀请（session_id + token）→ 写入共享 token 库文件（A5↔B2 对接点）并心跳保鲜。
/// 2. 拉起独立横幅进程，周期推送不可伪造的 `BannerState`。
/// 3. 以 Host 身份连信令、**阻塞等待 Viewer 的 Offer**，建连后下发授权决定。
/// 4. 启动抓屏→媒体通道发送泵（E2E 加密）与输入注入循环。
/// 5. 收到 `shutdown` 信号后优雅拆连并关闭横幅。
pub async fn run_host_agent(cfg: AgentConfig, shutdown: Arc<AtomicBool>) -> Result<()> {
    // 1) 本机身份（B4 持久化）：首装创建并加密落盘，重启加载保持指纹一致（TOFU 不重新告警）。
    let identity = load_or_create_persistent_identity(&IdentityPersistenceConfig {
        dir: cfg.identity_dir.clone(),
        passphrase: cfg.identity_pass.clone(),
        display_name: cfg.display_name.clone(),
    })
    .context("加载/创建持久化身份失败（口令错误或目录不可写？）")?;
    // 先取出私钥（SecretKey 仅留本进程，绝不序列化），再把 store 装配成 trait object。
    let secret = identity.secret.clone();
    let store = identity_store_handle(identity);

    // 2) 生成配对邀请并登记到信令 token 库（让同机部署的 signaling-svc 识别本 session）。
    //    配对不焚毁：受控端在线期间同一二维码可重复扫码建连；退出时（步骤 14）删除
    //    token 库文件使配对失效，崩溃时由信令侧凭文件心跳过期自动回收。
    let pairing = Connection::create_pairing();
    if let Err(e) = register_token_file(&pairing.session_id, &pairing.token) {
        // 显式配置了 SIGNALING_TOKEN_DB（同机生产部署）时，写失败会让信令侧认不出
        // 本 session，必须报错；未配置（默认相对路径，如装在 Program Files 下不可写）
        // 说明信令是远程部署、该文件无人读取，降级为警告继续跑。
        if std::env::var_os("SIGNALING_TOKEN_DB").is_some() {
            return Err(e).context("写入信令 token 库文件失败（SIGNALING_TOKEN_DB 不可写？）");
        }
        eprintln!("[warn] 本地 token 库不可写（{e}）；远程信令部署无需此文件，继续运行。");
    }
    print_pairing_invite(&pairing.session_id, &pairing.token, &cfg.signaling_addr);

    // 2b) token 库文件心跳：周期重写（刷新 mtime）向信令侧表明受控端在线；
    //     进程退出/崩溃后心跳停更，信令侧凭 mtime 过期自动回收配对。
    let hb_sid = pairing.session_id;
    let hb_token = pairing.token.clone();
    let hb_shutdown = shutdown.clone();
    let heartbeat = tokio::spawn(async move {
        loop {
            tokio::time::sleep(TOKEN_FILE_HEARTBEAT).await;
            if hb_shutdown.load(Ordering::SeqCst) {
                break;
            }
            // 写失败不致命：下一次心跳再试；真正退出由步骤 14 删文件兜底。
            let _ = register_token_file(&hb_sid, &hb_token);
        }
    });

    // 3) 拼信令 URL（Host 不带 token，作为房门；Viewer 连时带配对 token）。
    let signaling_url = build_signaling_url(&cfg.signaling_addr, &pairing.session_id);

    // 4) RTC 配置：配置了任一 RDCORE_STUN / RDCORE_TURN_* 环境变量时以环境为准；
    //    否则用内置联调 STUN+TURN（config::default_rtc_config），开箱即可跨 NAT。
    let mut rtc_cfg = if std::env::var_os("RDCORE_STUN").is_some()
        || std::env::var_os("RDCORE_TURN_URL").is_some()
    {
        RtcConfig::from_env()
    } else {
        crate::config::default_rtc_config()
    };
    rtc_cfg.include_loopback = cfg.loopback;

    // 5) 拉起 OS 横幅进程（独立进程 + 置顶窗口 + 托盘二维码弹窗，配对码经 --qr 传入）。
    let banner_child = if cfg.no_banner {
        None
    } else {
        let qr_code = format!("{}:{}", hex::encode(pairing.session_id.0), pairing.token);
        spawn_banner(cfg.banner_bin.clone(), Some(qr_code), &|msg| {
            eprintln!("⚠ {msg}")
        })?
        .map(|c| Arc::new(tokio::sync::Mutex::new(c)))
    };

    // 5b) 监视横幅进程：托盘「退出」/ 横幅异常退出时它先走，Host 随之联动停止。
    //     横幅是远程连接唯一可信的可视指示——它没了，Host 不应继续隐形地接受远控；
    //     同时保证「退出后再打开」是一个干净的新实例，托盘图标正常出现。
    let banner_watch = banner_child.clone().map(|child| {
        let sd = shutdown.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut g = child.lock().await;
                    match g.try_wait() {
                        Ok(Some(_)) => {
                            if !sd.swap(true, Ordering::SeqCst) {
                                eprintln!("● 横幅进程已退出（如托盘「退出」），Host Agent 联动停止。");
                            }
                            return;
                        }
                        Ok(None) => {}
                        Err(_) => return,
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    });

    // 6) 构造 Host 连接（连信令 + 建 PeerConnection）。
    let conn = Arc::new(
        Connection::new_host(
            &signaling_url,
            pairing.session_id,
            secret,
            store,
            rtc_cfg,
            cfg.heartbeat_timeout,
        )
        .await
        .context("构造 Host 连接失败")?,
    );

    // 7) 横幅刷新任务：周期把 SecurityIndicator 映射为 BannerState 推送（连接前为 Connecting）。
    let banner_push = match banner_client() {
        Ok(client) => Some(start_banner_refresh(conn.clone(), client, shutdown.clone())),
        Err(e) => {
            eprintln!("⚠ 横幅推送不可用：{e:#}");
            None
        }
    };

    // 8) 安装进程级 Ctrl-C 监听（只注册一次；连接等待 / 运行期间均可优雅停止）。
    install_ctrl_c(shutdown.clone());

    // 9) 建立连接并接受 Viewer：支持断线后原地重连，开发联调时无需重启 Host / 更换二维码。
    //    Host 全程保持同一 session（二维码不变）；Viewer 断线后本循环回到「等待下一个 Viewer 的
    //    Offer」状态，由 `Connection::reconnect_with` 以同一授权决定重跑握手。
    let decision = ConsentDecision::Grant {
        scopes: cfg.scopes.clone(),
        duration: None,
    };
    let mut first = true;
    loop {
        // 首连走 establish；后续走 reconnect（同一 session 等待下一个 Viewer 的 Offer）。
        // ⚠ establish 的第一个参数是 ICE 中继循环的退出标志，绝不能传进程级 shutdown——
        // 否则握手一完成本机即退出、媒体泵 0 帧夭折（Viewer 永远 loading）。
        let connected = if first {
            first = false;
            println!("● 等待 Viewer 连接并请求同意…");
            match tokio::select! {
                r = conn.establish(Arc::new(AtomicBool::new(false)), Some(decision.clone())) => r,
                _ = shutdown_signaled(&shutdown) => {
                    eprintln!("● 收到停止信号，中止等待连接。");
                    break;
                }
            } {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("⚠ 连接建立失败（握手 / 验签 / E2E 密钥 / 同意）：{e:#}");
                    false
                }
            }
        } else {
            println!("● Viewer 已断开，等待重新连接…");
            // 显式传授权决定：首连若从未成功过，last_host_decision 为空，裸 reconnect 会
            // 退化成自动 Deny 拒绝后续 Viewer；select! 监听 shutdown，等待期间 Ctrl-C 即时停止。
            match tokio::select! {
                r = conn.reconnect_with(decision.clone()) => r,
                _ = shutdown_signaled(&shutdown) => {
                    eprintln!("● 收到停止信号，中止等待重连。");
                    break;
                }
            } {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("⚠ 重连失败：{e:#}");
                    false
                }
            }
        };

        if !connected {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            eprintln!("● 2 秒后重试…");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        println!(
            "✓ 连接已建立，已向 Viewer 下发授权：{:?}",
            scopes_to_strings(&cfg.scopes)
        );

        // 10) 启动抓屏→媒体通道发送泵（E2E 加密像素）。
        let mut pump = start_capture(conn.clone(), &cfg)?;

        // 11) 启动输入注入循环（Viewer→Host 的控制事件作用到本机）。
        let input_handle = start_input_loop(conn.clone(), &cfg)?;

        // 12) 等待三种事件之一：收到关闭信号 / Viewer 掉线 / 新 Viewer 重扫（抢占接入，
        //     无需等旧对端死透——ICE 掉线检测可能需数十秒，期间重扫不应卡在门外）。
        let outcome = tokio::select! {
            _ = shutdown_signaled(&shutdown) => None,
            o = conn.wait_peer_gone_or_rescan() => Some(o),
        };

        // 13) 收尾本轮：停泵、停输入循环；若收到 shutdown 则整体退出，否则等待下一个 Viewer。
        let _ = pump.stop().await;
        if let Some(h) = input_handle {
            h.abort();
        }

        if shutdown.load(Ordering::SeqCst) {
            println!("● 收到停止信号，正在优雅关闭…");
            break;
        }
        if outcome == Some(rdcore_app::HostWaitOutcome::Rescan) {
            println!("● 收到新的连接请求（Viewer 重扫），接入新 Viewer…");
        }
    }

    // 14) 收尾：停心跳、删除 token 库文件（受控端退出 → 配对码立即失效）、关横幅、终止横幅进程。
    heartbeat.abort();
    if let Err(e) = clear_token_file() {
        eprintln!("[warn] 清理信令 token 库文件失败：{e}（配对将随文件心跳过期自动失效）");
    }
    if let Ok(client) = banner_client() {
        let _ = client.close("host shutdown");
    }
    if let Some(handle) = banner_push {
        handle.abort();
    }
    if let Some(child) = &banner_child {
        // 先发了 Close，横幅多半已自行退出；kill 仅兜底，进程已死时返回错误可忽略。
        let _ = child.lock().await.kill();
    }
    if let Some(h) = banner_watch {
        h.abort();
    }
    println!("✓ 已停止。");
    Ok(())
}

/// 周期把 `SecurityIndicator` 映射为 `BannerState` 推送给横幅进程。
///
/// 连接尚未完成验签时 `security_indicator()` 返回 `None`，此时推送 `Connecting` 状态，
/// 横幅如实反映「正在建立连接」。映射由 `rdcore_banner` 的 `consent` feature 保证单一事实来源。
fn start_banner_refresh(
    conn: Arc<Connection>,
    client: BannerClient,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let state: BannerState = match conn.security_indicator() {
                Some(si) => si.into(),
                None => BannerState {
                    phase: BannerPhase::Connecting,
                    ..Default::default()
                },
            };
            let _ = client.update(&state);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

/// 启动抓屏→媒体通道发送泵。
///
/// `--headless` 时用 `NullCaptureSource`（合成纯色帧）；否则用真实 `ScrapCaptureSource`
/// （经 `rdcore-capture` 的 `real` feature，已随本 crate 默认启用）。注意 `ScrapCaptureSource`
/// 是 `!Send`（包裹 scrap 的 DXGI `Capturer`），必须以 factory（`|| ...`）形式传入，让它在
/// 捕获线程内就地构造——这正是 `Connection::start_capture` 接受 factory 的原因。
fn start_capture(conn: Arc<Connection>, cfg: &AgentConfig) -> Result<HostMediaPump> {
    let fps = cfg.fps.max(1);
    if cfg.headless {
        Ok(conn.start_capture(
            || NullCaptureSource::new(1280, 720, u32::MAX, 0x20),
            fps,
        ))
    } else {
        // 预检，给出清晰错误；真正的构造在 pump 线程内完成（避免 !Send 跨线程）。
        if let Err(e) = rdcore_capture::ScrapCaptureSource::new() {
            anyhow::bail!("抓屏源初始化失败（无显示器？）: {e}");
        }
        Ok(conn.start_capture(
            || rdcore_capture::ScrapCaptureSource::new().expect("抓屏源初始化失败"),
            fps,
        ))
    }
}

/// 启动输入注入循环。
///
/// `--headless` 时用记录型 `NullInputInjector`；否则用真实 `EnigoInputInjector` 把 Viewer 的
/// 控制事件真正作用到本机（经 `rdcore-capture` 的 `real` feature，已随本 crate 默认启用）。
fn start_input_loop(conn: Arc<Connection>, cfg: &AgentConfig) -> Result<Option<JoinHandle<()>>> {
    if cfg.headless {
        let inj = NullInputInjector::new();
        Ok(Some(spawn_input_loop(conn, inj)))
    } else {
        let inj = rdcore_capture::EnigoInputInjector::new()
            .map_err(|e| anyhow::anyhow!("输入注入器初始化失败: {e}"))?;
        Ok(Some(spawn_input_loop(conn, inj)))
    }
}

/// 后台任务：把 Viewer 经 E2E 加密控制通道发来的 `InputEvent` 作用到本机。
fn spawn_input_loop<I>(conn: Arc<Connection>, mut inj: I) -> JoinHandle<()>
where
    I: HostInputInjector + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match conn.recv_input().await {
                Ok(Some(ev)) => inj.inject(&ev),
                Ok(None) => break, // 连接关闭
                Err(_) => break,
            }
        }
    })
}

/// 进程级 Ctrl-C 监听：收到即置位 `shutdown`（只注册一次，连接等待 / 运行期间均可优雅停止）。
fn install_ctrl_c(shutdown: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("● 收到 Ctrl-C，正在停止…");
            shutdown.store(true, Ordering::SeqCst);
        }
    });
}

/// 阻塞直到 `shutdown` 置位（供 `select!` 在等待连接时响应 Ctrl-C）。
async fn shutdown_signaled(shutdown: &Arc<AtomicBool>) {
    while !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 拼接信令 URL：`ws://host/<session_hex>`（Host 不带 token）。
fn build_signaling_url(addr: &str, session: &SessionId) -> String {
    let base = addr.trim_end_matches('/');
    format!("{base}/{}", hex::encode(session.0))
}

/// 打印配对邀请（通过带外方式交给 Viewer：扫码 / 输码）。
///
/// 除分字段展示外，还打印合成配对码（`<32hex session>:<64hex token>`，Viewer App
/// `PairingInvite.parse` 的输入格式）及其终端二维码（unicode 半块，深/浅终端均可扫：
/// 模块用亮块渲染，在深色终端上呈白码黑底，扫码兼容性最好）。
///
/// 同时把配对码落盘到 `~/Library/Application Support/RdCore/pairing.txt`：
/// 双击 .app 启动时 stdout 被 LaunchServices 吞掉，Viewer 需要稳定的地方读配对码。
fn print_pairing_invite(session: &SessionId, token: &str, signal: &str) {
    let session_hex = hex::encode(session.0);
    let code = format!("{session_hex}:{token}");
    println!("╔════════════════════════════════════════════════════╗");
    println!("║  配对邀请（请经带外方式交给 Viewer）              ║");
    println!("╠════════════════════════════════════════════════════╣");
    println!("║ session : {session_hex}");
    println!("║ token   : {token}");
    println!("║ signal  : {signal}");
    println!("╚════════════════════════════════════════════════════╝");
    println!("配对码 : {code}");
    if let Err(e) = write_pairing_file(&code) {
        eprintln!("[warn] 写入配对码文件失败：{e}（双击启动时 Viewer 将无法读取）");
    }
    match qrcode::QrCode::new(code.as_bytes()) {
        Ok(qr) => {
            use qrcode::render::unicode;
            let art = qr
                .render::<unicode::Dense1x2>()
                .dark_color(unicode::Dense1x2::Light)
                .light_color(unicode::Dense1x2::Dark)
                .quiet_zone(true)
                .build();
            println!("扫码配对（用 Viewer App 的「扫码连接」）:\n{art}");
        }
        Err(e) => eprintln!("渲染配对二维码失败（可用上方配对码手动输入）: {e}"),
    }
}

/// 把配对码写入 `~/Library/Application Support/RdCore/pairing.txt`（覆盖写，原子替换）。
///
/// 双击 .app 启动时 stdout 不可见，Viewer 从这里取配对码；
/// 交互终端里此文件是冗余的，写入失败不影响主流程（仅告警）。
fn write_pairing_file(code: &str) -> std::io::Result<()> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME 未设置")
    })?;
    let dir = std::path::Path::new(&home)
        .join("Library")
        .join("Application Support")
        .join("RdCore");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("pairing.txt");
    // 原子替换：先写临时文件再 rename，避免 Viewer 读到半截。
    let tmp = dir.join("pairing.txt.tmp");
    std::fs::write(&tmp, format!("{code}\n"))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn scopes_to_strings(scopes: &HashSet<ConsentScope>) -> Vec<String> {
    scopes.iter().map(|s| format!("{s:?}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_signaling_url_appends_session_hex_without_token() {
        let sid = SessionId([0x01u8; 16]);
        let url = build_signaling_url("ws://127.0.0.1:8080/", &sid);
        assert_eq!(url, format!("ws://127.0.0.1:8080/{}", hex::encode(sid.0)));
        assert!(!url.contains("token"), "Host URL 不应携带 token");
    }

    #[test]
    fn build_signaling_url_handles_no_trailing_slash() {
        let sid = SessionId([0x02u8; 16]);
        let url = build_signaling_url("ws://example.com:9000", &sid);
        assert_eq!(url, format!("ws://example.com:9000/{}", hex::encode(sid.0)));
    }
}
