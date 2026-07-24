//! OS 级不可伪造连接横幅（rdcore-banner）
//!
//! # 为什么需要"OS 级 / 不可伪造"？
//!
//! 之前的 P5/P6 已经在 Rust 核心产出了密码学确认的 [`SecurityIndicator`]，
//! 并由 Flutter 画在主界面上。但主界面本身属于"被控应用"——一个恶意的远端对等方
//! 理论上可以诱骗主程序把横幅画成"未连接"，从而让用户误以为安全。
//!
//! 类比 Web 开发：**浏览器地址栏是"浏览器外壳（chrome）"，不是"网页（content）"**。
//! 无论网页里的 JS 怎么写，它都无法伪造地址栏里的 `https://` 与小锁。因为地址栏由
//! 一个**更高权限、独立信任域**的进程/组件绘制，网页进程碰不到它。
//!
//! 本 crate 就是这个"浏览器外壳"：
//!
//! 1. **独立进程**：横幅由 `rdcore-banner` 这个**单独的可执行文件**运行，与主程序
//!    处于不同的进程（最好由更高权限启动，例如 Windows 服务或独立用户），主程序无法直接
//!    `TerminateWindow` / 覆盖它。
//! 2. **置顶窗口（windows-native）**：窗口带 `WS_EX_TOPMOST`，永远盖在受控应用之上，
//!    且对输入透明（click-through），受控应用既不能盖住它、也点不到它。
//! 3. **只被动接收状态**：横幅进程**从不主动**从网络读任何东西，它只通过一个本地 IPC
//!    通道接收主程序推送的 [`BannerState`]。"现在连着谁、是否 E2E 加密"由主程序告知，
//!    但主程序**无法让横幅显示它没发送过的状态**——因为状态来自 P4/P5 的密码学结论。
//!
//! 安全边界 = "进程隔离 + 置顶窗口"，而不是 IPC 通道本身。所以默认实现用 UDP 回环即可
//! （沙箱可测、跨平台），生产环境应切换为带 ACL 的命名管道（见 `windows-native` 说明）。
//!
//! [`SecurityIndicator`]: rdcore_consent::SecurityIndicator

use serde::{Deserialize, Serialize};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 横幅监听的默认 UDP 端口（回环 127.0.0.1）。生产环境应改用命名管道 + ACL。
pub const DEFAULT_BANNER_PORT: u16 = 48173;

/// 进程级退出请求标志。
///
/// 原生渲染器的窗口线程在消息循环结束（托盘「退出」/ 窗口被销毁）后置位；IPC 接收端
/// 借此跳出阻塞的 `recv`，让进程真正退出。
///
/// 背景 bug：此前窗口线程退出后主线程仍阻塞在 UDP `recv` 上，横幅进程不死且一直占着
/// [`DEFAULT_BANNER_PORT`]；下次启动 `bind` 该端口直接失败（GUI 子系统下静默 panic），
/// 表现为「退出后再打开程序，右下角托盘图标再也没有了」。
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 请求整个横幅进程退出（由原生窗口线程在消息循环结束时调用）。
pub fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

/// 是否已请求进程退出（IPC 接收循环在阻塞超时后检查）。
pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// 启动失败诊断：追加写 `%TEMP%\rdcore-banner.log`（GUI 子系统无控制台可查）。
#[cfg(any(feature = "windows-native", feature = "macos-native"))]
fn fatal_log(msg: &str) {
    eprintln!("{msg}");
    if let Some(dir) = std::env::var_os("TEMP") {
        let path = std::path::PathBuf::from(dir).join("rdcore-banner.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// 连接所处阶段，镜像 Flutter 的 `ConnectionPhase`。横幅必须如实反映。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BannerPhase {
    /// 初始化 / 尚未发起。
    Setup,
    /// 正在建立连接（信令 / ICE）。
    Connecting,
    /// 等待 Host 用户同意。
    AwaitingConsent,
    /// 已激活（含授予范围）。
    Active,
    /// 被 Host 拒绝。
    Denied,
    /// 已关闭。
    Closed,
}

impl BannerPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            BannerPhase::Setup => "setup",
            BannerPhase::Connecting => "connecting",
            BannerPhase::AwaitingConsent => "awaitingConsent",
            BannerPhase::Active => "active",
            BannerPhase::Denied => "denied",
            BannerPhase::Closed => "closed",
        }
    }
}

/// 横幅要展示的实时状态。全部是"纯数据"，便于跨进程序列化。
///
/// 字段刻意与 `rdcore_consent::SecurityIndicator` / Flutter 的
/// `SecurityIndicatorSnapshot` 对齐（见 `consent` feature 的一键映射）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BannerState {
    /// 对端展示名（来自已认证的 VerifiedPeer，不可被对端伪造）。
    pub peer_name: String,
    /// 对端设备 ID（十六进制）。
    pub peer_device_id: String,
    /// 对端公钥指纹（紧凑十六进制，无空格）。
    pub peer_fingerprint: String,
    /// 对端公钥指纹（空格分隔，便于人眼逐字节核对）。
    pub peer_fingerprint_spaced: String,
    /// 是否已建立端到端加密（P5 会话密钥握手成功后主程序置 true）。
    pub encrypted: bool,
    /// 当前阶段。
    pub phase: BannerPhase,
    /// 已授予的权限范围（字符串，便于跨语言）。
    pub granted_scopes: Vec<String>,
    /// 关闭原因（phase == Closed 时）。
    pub closed_reason: Option<String>,
    /// 额外说明（如拒绝原因）。
    pub message: Option<String>,
}

impl Default for BannerState {
    fn default() -> Self {
        Self {
            peer_name: String::new(),
            peer_device_id: String::new(),
            peer_fingerprint: String::new(),
            peer_fingerprint_spaced: String::new(),
            encrypted: false,
            phase: BannerPhase::Setup,
            granted_scopes: Vec::new(),
            closed_reason: None,
            message: None,
        }
    }
}

/// 主程序发给横幅进程的指令（经 IPC 以 JSON 传输，每行一条）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BannerCommand {
    /// 用新状态刷新横幅。
    Update(BannerState),
    /// 显示横幅（若曾被 Hide）。
    Show,
    /// 隐藏横幅（但不退出进程）。
    Hide,
    /// 结束会话并关闭横幅进程，`reason` 记录原因。
    Close { reason: String },
}

/// 横幅渲染器抽象。默认实现是控制台（沙箱可测）；生产实现是 Windows 置顶窗口。
///
/// 用 `&self` + 内部可变性（如 `Arc<Mutex<…>>` / 窗口句柄）实现，
/// 这样 `run_banner_with` 只需持有 `Box<dyn BannerRenderer>` 即可反复调用。
pub trait BannerRenderer: Send + 'static {
    /// 用给定状态重绘横幅。
    fn render(&self, state: &BannerState);
    /// 显示（默认无操作）。
    fn show(&self) {}
    /// 隐藏（默认无操作）。
    fn hide(&self) {}
    /// 横幅进程退出前调用（默认无操作）。原生渲染器借此销毁窗口。
    fn on_close(&self) {}
}

/// IPC 接收端抽象。横幅进程阻塞等待下一条指令；通道关闭则返回 `None`。
pub trait BannerIpc: Send {
    fn recv(&mut self) -> Option<BannerCommand>;
}

/// 横幅主循环：从 IPC 读指令，驱动 renderer。收到 `Close` 或通道关闭即退出。
///
/// 这是整套机制的核心：**横幅进程只在这里被动消费状态**，从不主动联网。
///
/// 返回最终状态（供 macOS 原生实现在 run loop 退出前做收尾）。
pub fn run_banner_with(renderer: Box<dyn BannerRenderer>, mut ipc: Box<dyn BannerIpc>) -> BannerState {
    let mut state = BannerState::default();
    loop {
        match ipc.recv() {
            Some(BannerCommand::Update(s)) => {
                state = s;
                renderer.render(&state);
            }
            Some(BannerCommand::Show) => renderer.show(),
            Some(BannerCommand::Hide) => renderer.hide(),
            Some(BannerCommand::Close { reason }) => {
                state.phase = BannerPhase::Closed;
                state.closed_reason = Some(reason);
                renderer.render(&state);
                renderer.on_close();
                break;
            }
            None => break,
        }
    }
    state
}

/// 进程入口。生产按平台选真实置顶窗口；否则用控制台渲染（沙箱/跨平台）。
pub fn run_banner() {
    #[cfg(all(feature = "windows-native", target_os = "windows"))]
    {
        let ipc = match UdpBannerIpc::bind(DEFAULT_BANNER_PORT) {
            Ok((ipc, _port)) => ipc,
            Err(e) => {
                // 端口被占（典型：上一个横幅进程未退净）时不再 panic——GUI 子系统下
                // panic 无任何可见输出，用户只会看到「托盘图标没了」。记诊断日志后退出。
                fatal_log(&format!(
                    "run_banner: bind 127.0.0.1:{DEFAULT_BANNER_PORT} failed: {e}"
                ));
                return;
            }
        };
        run_banner_with(
            Box::new(crate::native::WindowsTopmostBannerRenderer::new(
                parse_qr_arg(),
            )),
            Box::new(ipc),
        );
    }
    #[cfg(all(feature = "macos-native", target_os = "macos"))]
    {
        // macOS 原生实现必须在主线程跑 NSApplication；IPC 接收由实现内部挪到后台线程。
        crate::macos_native::run_banner_macos(parse_qr_arg());
    }
    #[cfg(not(all(
        any(feature = "windows-native", feature = "macos-native"),
        any(target_os = "windows", target_os = "macos")
    )))]
    {
        let (ipc, _port) = UdpBannerIpc::bind(DEFAULT_BANNER_PORT).expect("bind banner udp socket");
        run_banner_with(Box::new(ConsoleBannerRenderer), Box::new(ipc));
    }
}

/// 从命令行解析 `--qr <配对码>`（Host Agent 拉起横幅时传入，用于二维码弹窗）。
/// 仅生产渲染器（windows-native / macos-native）使用。
#[cfg(any(feature = "windows-native", feature = "macos-native"))]
fn parse_qr_arg() -> Option<String> {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--qr" {
            return it.next();
        }
    }
    None
}

/// 控制台渲染器：把横幅画成一段带边框的文本。沙箱与无显示环境可用，便于测试与演示。
pub struct ConsoleBannerRenderer;

impl BannerRenderer for ConsoleBannerRenderer {
    fn render(&self, state: &BannerState) {
        println!("{}", render_banner_text(state));
    }
}

/// 基于 UDP 回环的 IPC 接收端。跨平台、无需特权，适合沙箱与默认构建。
///
/// 安全边界不取决于它；生产应换成命名管道 + ACL（见 crate 文档）。
pub struct UdpBannerIpc {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl UdpBannerIpc {
    /// 绑定端口（`0` 表示系统分配临时端口）。返回 (接收端, 实际端口)。
    pub fn bind(port: u16) -> io::Result<(Self, u16)> {
        let socket = UdpSocket::bind(("127.0.0.1", port))?;
        // 读超时仅用于周期性检查 QUIT_REQUESTED：窗口线程结束后能及时跳出 recv，
        // 让进程随之退出（否则进程残留占用端口，下次启动 bind 失败、托盘图标出不来）。
        // 超时不代表通道关闭，见 recv 实现。
        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
        let local = socket.local_addr()?;
        Ok((
            Self {
                socket,
                buf: vec![0u8; 65536],
            },
            local.port(),
        ))
    }
}

impl BannerIpc for UdpBannerIpc {
    fn recv(&mut self) -> Option<BannerCommand> {
        loop {
            match self.socket.recv(&mut self.buf) {
                Ok(n) => return serde_json::from_slice::<BannerCommand>(&self.buf[..n]).ok(),
                // 读超时（Windows=TimedOut，Unix=WouldBlock）：仅在已请求退出时结束；
                // 否则继续阻塞等待下一条指令。
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if quit_requested() {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

/// 主程序侧客户端：把 [`BannerCommand`] 推送给横幅进程。
///
/// 宿主（主程序 / `rdcore-ffi`）应依赖本 crate，用它把 `SecurityIndicator`
/// 映射后的 [`BannerState`] 推送给独立的横幅进程。
pub struct BannerClient {
    socket: UdpSocket,
    addr: SocketAddr,
}

impl BannerClient {
    /// 连到本地 `port` 上的横幅进程（UDP）。
    pub fn udp(port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        Ok(Self {
            socket,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
        })
    }

    /// 发送任意指令。
    pub fn send(&self, cmd: &BannerCommand) -> io::Result<usize> {
        let bytes = serde_json::to_vec(cmd)?;
        self.socket.send_to(&bytes, self.addr)
    }

    /// 便捷：推送一次状态刷新。
    pub fn update(&self, state: &BannerState) -> io::Result<usize> {
        self.send(&BannerCommand::Update(state.clone()))
    }

    /// 便捷：结束会话并关闭横幅进程。
    pub fn close(&self, reason: &str) -> io::Result<usize> {
        self.send(&BannerCommand::Close {
            reason: reason.to_string(),
        })
    }
}

/// 便利：宿主一行即可把当前状态刷新推送给默认端口上的横幅进程。
///
/// 宿主（主程序 / `rdcore-ffi`）在 P4/P5 得出新的 [`crate::BannerState`] 后调用：
/// ```no_run
/// let state = rdcore_banner::BannerState::default();
/// rdcore_banner::push_state(&state).ok();
/// ```
pub fn push_state(state: &BannerState) -> io::Result<()> {
    BannerClient::udp(DEFAULT_BANNER_PORT)?.update(state)?;
    Ok(())
}

/// 单行摘要（供 Windows 原生窗口 / 日志使用）。
#[cfg_attr(not(feature = "windows-native"), allow(dead_code))]
pub(crate) fn banner_summary(s: &BannerState) -> String {
    let enc = if s.encrypted { "ON" } else { "OFF" };
    let scopes = if s.granted_scopes.is_empty() {
        "-".to_string()
    } else {
        s.granted_scopes.join(",")
    };
    let fp = if s.peer_fingerprint_spaced.is_empty() {
        "-".to_string()
    } else {
        s.peer_fingerprint_spaced.clone()
    };
    format!(
        "● RDCORE | peer:{} | fp:{} | E2E:{} | {} | scopes:{}",
        if s.peer_name.is_empty() {
            "-"
        } else {
            s.peer_name.as_str()
        },
        fp,
        enc,
        s.phase.as_str(),
        scopes
    )
}

/// 多行带边框文本（供控制台渲染使用）。
pub(crate) fn render_banner_text(s: &BannerState) -> String {
    let enc = if s.encrypted { "ON" } else { "OFF" };
    let scopes = if s.granted_scopes.is_empty() {
        "-".to_string()
    } else {
        s.granted_scopes.join(", ")
    };
    let fp = if s.peer_fingerprint_spaced.is_empty() {
        "-".to_string()
    } else {
        s.peer_fingerprint_spaced.clone()
    };
    let device = if s.peer_device_id.is_empty() {
        "-".to_string()
    } else {
        s.peer_device_id.clone()
    };
    let line1 = format!(
        " {} REMOTE SESSION — {} ",
        "● RDCORE",
        s.phase.as_str().to_uppercase()
    );
    let line2 = format!(" Peer: {}   Device: {}", s.peer_name, device);
    let line3 = format!(" Fingerprint: {}", fp);
    let line4 = format!(" E2E Encryption: {}    Scopes: {}", enc, scopes);
    let extra = match (&s.message, &s.closed_reason) {
        (Some(m), _) => format!(" Note: {}", m),
        (None, Some(r)) => format!(" Closed: {}", r),
        (None, None) => String::new(),
    };
    let width = 60;
    let bar: String = "═".repeat(width);
    let mut out = String::new();
    out.push_str(&format!("╔{}╗\n", bar));
    out.push_str(&format!("║{:width$}║\n", line1, width = width));
    out.push_str(&format!("║{:width$}║\n", line2, width = width));
    out.push_str(&format!("║{:width$}║\n", line3, width = width));
    out.push_str(&format!("║{:width$}║\n", line4, width = width));
    if !extra.is_empty() {
        out.push_str(&format!("║{:width$}║\n", extra, width = width));
    }
    out.push_str(&format!("╚{}╝", bar));
    out
}

#[cfg(feature = "windows-native")]
mod native;

#[cfg(all(feature = "macos-native", target_os = "macos"))]
mod macos_native;

#[cfg(feature = "consent")]
mod consent_impl;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn banner_state_default_is_setup() {
        let s = BannerState::default();
        assert_eq!(s.phase, BannerPhase::Setup);
        assert!(!s.encrypted);
    }

    #[test]
    fn command_roundtrip_over_json() {
        let s = BannerState {
            peer_name: "friend-phone".into(),
            encrypted: true,
            phase: BannerPhase::Active,
            granted_scopes: vec!["view".into(), "input".into()],
            ..Default::default()
        };
        let cmd = BannerCommand::Update(s.clone());
        let json = serde_json::to_string(&cmd).unwrap();
        let back: BannerCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, BannerCommand::Update(s));
    }

    #[test]
    fn close_command_serializes() {
        let cmd = BannerCommand::Close {
            reason: "revoked".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: BannerCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn summary_reports_encrypted_and_phase() {
        let s = BannerState {
            peer_name: "bob".into(),
            encrypted: true,
            phase: BannerPhase::Active,
            granted_scopes: vec!["view".into()],
            ..Default::default()
        };
        let sum = banner_summary(&s);
        assert!(sum.contains("bob"));
        assert!(sum.contains("E2E:ON"));
        assert!(sum.contains("active"));
    }

    /// 真实回路：客户端经 UDP 推送 Update + Close，横幅进程收到并正确渲染/退出。
    #[test]
    fn banner_loop_receives_update_then_close() {
        let (ipc, port) = UdpBannerIpc::bind(0).unwrap();
        let captured = Arc::new(Mutex::new(Vec::<BannerState>::new()));
        let spy = SpyRenderer {
            states: captured.clone(),
        };
        let handle = thread::spawn(move || {
            run_banner_with(Box::new(spy), Box::new(ipc));
        });

        let client = BannerClient::udp(port).unwrap();
        let st = BannerState {
            peer_name: "friend-phone".into(),
            encrypted: true,
            phase: BannerPhase::Active,
            granted_scopes: vec!["view".into(), "input".into()],
            ..Default::default()
        };
        client.update(&st).unwrap();
        thread::sleep(Duration::from_millis(50));
        client.close("revoked").unwrap();
        handle.join().unwrap();

        let log = captured.lock().unwrap();
        assert!(
            log.iter()
                .any(|s| s.peer_name == "friend-phone" && s.encrypted),
            "应渲染出 friend-phone 且 E2E=ON 的状态"
        );
        assert_eq!(log.last().unwrap().phase, BannerPhase::Closed);
        assert_eq!(
            log.last().unwrap().closed_reason.as_deref(),
            Some("revoked")
        );
    }

    /// 测试用渲染器：把每次 render 的状态记下来，供断言。
    struct SpyRenderer {
        states: Arc<Mutex<Vec<BannerState>>>,
    }
    impl BannerRenderer for SpyRenderer {
        fn render(&self, s: &BannerState) {
            self.states.lock().unwrap().push(s.clone());
        }
    }

    /// `consent` feature：SecurityIndicator → BannerState 字段映射正确。
    #[cfg(feature = "consent")]
    #[test]
    fn from_security_indicator_maps_fields() {
        use rdcore_consent::{ConnectionState, ConsentScope, SecurityIndicator};
        use rdcore_crypto::Fingerprint;

        let si = SecurityIndicator {
            display_name: "friend".into(),
            device_id: [0xabu8; 16],
            fingerprint: Fingerprint([0xcd; 32]),
            fingerprint_spaced: "CD CD CD CD".into(),
            state: ConnectionState::Active {
                scopes: [ConsentScope::View, ConsentScope::Input]
                    .into_iter()
                    .collect(),
                expires_at: None,
            },
            encrypted: true,
        };
        let b: BannerState = si.into();
        assert_eq!(b.peer_name, "friend");
        assert!(b.encrypted);
        assert_eq!(b.phase, BannerPhase::Active);
        assert!(b.granted_scopes.contains(&"view".to_string()));
        assert!(b.granted_scopes.contains(&"input".to_string()));
        assert_eq!(b.peer_fingerprint, "CDCDCDCD");
    }
}
