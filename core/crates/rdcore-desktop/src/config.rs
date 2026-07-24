//! Host Agent 配置与 CLI 定义。
//!
//! 两类入口：
//! - 交互 / 控制台：`run`（前台运行，生成配对、等待 Viewer、投屏 + 控制）。
//! - 服务化常驻：`install` / `uninstall` / `service`（仅 `service` feature；
//!   Windows 走 SCM，macOS 走 launchd）。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use rdcore_consent::ConsentScope;
use rdcore_rtc::{IceServer, RtcConfig};

/// 内置联调服务器：信令基址。可用 `--signal` 或环境变量 `RDCORE_SIGNALING` 覆盖。
/// （纯 IP 无域名明文部署，见 `cloud/deploy/README.md`「无域名纯 IP 部署」。）
pub const DEFAULT_SIGNALING: &str = "ws://8.138.237.243:8080";
/// 内置联调 STUN（与 DEFAULT_SIGNALING 同机）。环境变量 `RDCORE_STUN` 可覆盖。
pub const DEFAULT_STUN: &str = "stun:8.138.237.243:3478";
/// 内置联调 TURN（对称 NAT / 蜂窝兜底，TURN 仅见 E2E 密文）。`RDCORE_TURN_*` 可覆盖。
pub const DEFAULT_TURN_URL: &str = "turn:8.138.237.243:3478?transport=udp";
/// 内置联调 TURN 用户名（与 coturn `--user` 一致）。
pub const DEFAULT_TURN_USER: &str = "rdcore";
/// 内置联调 TURN 凭据（联调专用；生产应改为动态凭据并经安全配置通道下发）。
pub const DEFAULT_TURN_PASS: &str = "84d9e822b2be47739710013bfd15aec91b5cd4363c61b78c";

/// 默认目标帧率（fps）。CLI `--fps` 缺省值与服务模式 `service_default()` 共用此常量，
/// 避免两条配置路径的默认值漂移；临时压测可用 `--fps` 显式覆盖。
/// 60fps 依托 GPU 硬编（NVENC/QSV/AMF，hwcodec 特性）；软编回退时 CPU 占用上升，
/// 低端机可用 `--fps 30` 退回。
pub const DEFAULT_FPS: u16 = 60;

/// 默认 RTC 配置：内置联调 STUN + TURN。
/// 仅在用户未配置任何 `RDCORE_STUN` / `RDCORE_TURN_*` 环境变量时使用（见 agent.rs）。
pub fn default_rtc_config() -> RtcConfig {
    RtcConfig {
        ice_servers: vec![
            IceServer::from(DEFAULT_STUN),
            IceServer::turn([DEFAULT_TURN_URL], DEFAULT_TURN_USER, DEFAULT_TURN_PASS),
        ],
        ..RtcConfig::default()
    }
}

/// rdcore-desktop — 受控端 Host Agent（纯 Rust 后台服务）。
#[derive(Parser, Debug)]
#[command(name = "rdcore-desktop", about = "Remote-desktop Host Agent (纯 Rust 服务)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// 前台运行 Host Agent：生成配对、等待 Viewer、开始投屏与控制。
    Run(RunArgs),
    /// 安装为系统服务（仅 `service` feature；Windows 走 SCM，macOS 走 launchd）。
    Install,
    /// 卸载系统服务（仅 `service` feature）。
    Uninstall,
    /// 作为服务入口运行（由 SCM / launchd 拉起，勿手动执行）。
    Service,
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// 本机展示名（出现在对端不可伪造横幅上）。省略时回退到「操作系统 - 电脑名」，
    /// 例如 "Windows 11 - Yuho'pc"。
    #[arg(long, short)]
    pub name: Option<String>,

    /// 信令服务器地址（WebSocket，不含 session 路径）。
    /// 优先级：本参数 > 环境变量 `RDCORE_SIGNALING` > 内置默认（联调服务器）。
    #[arg(long, short)]
    pub signal: Option<String>,

    /// 授权范围，逗号分隔：`view,input,clipboard,file`。
    #[arg(long, default_value = "view,input")]
    pub scopes: String,

    /// 目标帧率（fps）。
    #[arg(long, default_value_t = DEFAULT_FPS)]
    pub fps: u16,

    /// 心跳超时（秒）；对端失联超过此时长则拆连。
    #[arg(long, default_value_t = 30)]
    pub heartbeat_timeout: u64,

    /// 无显示器 / 测试时启用：用合成的 Null 捕获源与记录型注入器，不触碰本机。
    #[arg(long)]
    pub headless: bool,

    /// 把 127.0.0.1 / ::1 纳入 ICE 候选（仅同机回环联调用）。
    #[arg(long)]
    pub loopback: bool,

    /// 显式指定 `rdcore-banner` 二进制路径；默认在自身同级目录查找。
    #[arg(long)]
    pub banner_bin: Option<PathBuf>,

    /// 不拉起 OS 横幅（无置顶窗口；状态仅打日志）。
    #[arg(long)]
    pub no_banner: bool,

    /// （B4 身份持久化）身份目录；默认 `<系统配置目录>/rdcore/identity`。
    /// 重启后指纹保持一致（TOFU 不重新告警），是验收 §7 第 7 步的关键。
    #[arg(long)]
    pub identity_dir: Option<PathBuf>,

    /// （B4 身份持久化）保护私钥的口令；默认从环境变量 `RDCORE_IDENTITY_PASS` 读，
    /// 都未设则用设备级默认口令（生产应经 OS 钥匙串注入）。
    #[arg(long)]
    pub identity_pass: Option<String>,
}

impl RunArgs {
    /// 解析 CLI 为运行期配置。
    pub fn to_config(&self) -> AgentConfig {
        let signal = self
            .signal
            .clone()
            .or_else(|| std::env::var("RDCORE_SIGNALING").ok())
            .unwrap_or_else(|| DEFAULT_SIGNALING.into());
        AgentConfig {
            display_name: self
                .name
                .clone()
                .unwrap_or_else(default_host_display_name),
            signaling_addr: signal,
            scopes: parse_scopes(&self.scopes),
            fps: self.fps,
            heartbeat_timeout: Duration::from_secs(self.heartbeat_timeout),
            headless: self.headless,
            loopback: self.loopback,
            banner_bin: self.banner_bin.clone(),
            no_banner: self.no_banner,
            identity_dir: self
                .identity_dir
                .clone()
                .unwrap_or_else(default_identity_dir),
            identity_pass: self
                .identity_pass
                .clone()
                .or_else(|| std::env::var("RDCORE_IDENTITY_PASS").ok())
                .unwrap_or_else(|| "rdcore-device-default".into()),
        }
    }
}

/// 默认身份目录（B4）：与 `rdcore_app::connection_lifecycle::default_identity_dir` 一致。
fn default_identity_dir() -> PathBuf {
    rdcore_app::connection_lifecycle::default_identity_dir()
}

/// Host Agent 运行期配置（与 CLI 解耦，便于服务模式用默认值构造）。
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub display_name: String,
    pub signaling_addr: String,
    pub scopes: HashSet<ConsentScope>,
    pub fps: u16,
    pub heartbeat_timeout: Duration,
    pub headless: bool,
    pub loopback: bool,
    pub banner_bin: Option<PathBuf>,
    pub no_banner: bool,
    /// （B4）身份目录（重启保指纹）。
    pub identity_dir: PathBuf,
    /// （B4）保护私钥的口令。
    pub identity_pass: String,
}

impl AgentConfig {
    /// 服务模式的默认配置：从环境变量取信令地址，授予 view+input，30s 心跳。
    #[cfg(feature = "service")]
    pub fn service_default() -> Self {
        let mut scopes = HashSet::new();
        scopes.insert(ConsentScope::View);
        scopes.insert(ConsentScope::Input);
        AgentConfig {
            display_name: default_host_display_name(),
            signaling_addr: std::env::var("RDCORE_SIGNALING")
                .unwrap_or_else(|_| DEFAULT_SIGNALING.into()),
            scopes,
            fps: DEFAULT_FPS,
            heartbeat_timeout: Duration::from_secs(30),
            headless: false,
            loopback: false,
            banner_bin: None,
            no_banner: false,
            identity_dir: default_identity_dir(),
            identity_pass: std::env::var("RDCORE_IDENTITY_PASS")
                .unwrap_or_else(|_| "rdcore-device-default".into()),
        }
    }
}

/// 受控端默认展示名：操作系统 + 电脑名，例如 "Windows 11 - Yuho'pc"。
///
/// 仅当用户未用 `--name` 显式指定时采用；Viewer 收到后经由安全指示器原样显示为
/// 「对端」名称，无需改动 FFI / 身份契约。
///
/// 实现：电脑名取自环境变量（Windows 的 `COMPUTERNAME` / 类 Unix 的 `HOSTNAME`，缺失时回退
/// 到 `hostname` 命令）；操作系统在 Windows 下通过 `cmd /c ver` 解析 build 号区分 10/11，
/// 其余平台用 [`std::env::consts::OS`] 的友好名。全部 best-effort，失败回退到
/// 平台原始标识，保证永不抛异常。
pub fn default_host_display_name() -> String {
    let computer = computer_name();
    let os = os_label();
    format!("{} - {}", os, computer)
}

/// 本机电脑名（展示名第二部分）。
fn computer_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "未知设备".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "未知设备".into())
    }
}

/// 操作系统友好名（展示名第一部分）。
fn os_label() -> String {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/c", "ver"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| parse_windows_version(&s))
            .unwrap_or_else(|| "Windows".into())
    }
    #[cfg(not(windows))]
    {
        let os = std::env::consts::OS;
        let mut chars = os.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => os.to_string(),
        }
    }
}

/// 从 `cmd /c ver` 的输出（形如 "Microsoft Windows [Version 10.0.22621.1]"）解析出
/// "Windows 11" / "Windows 10"。build 号 >= 22000 即 Windows 11。
#[cfg(windows)]
fn parse_windows_version(s: &str) -> Option<String> {
    let idx = s.find("Version")?;
    let rest = &s[idx + "Version".len()..];
    // 提取连续的数字段（期望 " 10.0.22621.1 ]"），取前三个：major.minor.build。
    let mut nums: Vec<&str> = Vec::new();
    for tok in rest.split(|c: char| !c.is_ascii_digit()) {
        if !tok.is_empty() {
            nums.push(tok);
            if nums.len() == 3 {
                break;
            }
        }
    }
    if nums.len() >= 3 {
        let major: u32 = nums[0].parse().ok()?;
        let build: u32 = nums[2].parse().ok()?;
        if major == 10 && build >= 22000 {
            return Some("Windows 11".into());
        }
        if major == 10 {
            return Some("Windows 10".into());
        }
    }
    None
}

/// 把逗号分隔的范围字符串解析为集合；空集合时退化为仅 `view`（最小可用）。
pub fn parse_scopes(s: &str) -> HashSet<ConsentScope> {
    let mut set = HashSet::new();
    for part in s.split(',') {
        match part.trim().to_ascii_lowercase().as_str() {
            "view" => {
                set.insert(ConsentScope::View);
            }
            "input" => {
                set.insert(ConsentScope::Input);
            }
            "clipboard" => {
                set.insert(ConsentScope::Clipboard);
            }
            "file" | "filetransfer" => {
                set.insert(ConsentScope::FileTransfer);
            }
            other if !other.is_empty() => {
                eprintln!("⚠ 未知授权范围 `{other}`，已忽略");
            }
            _ => {}
        }
    }
    if set.is_empty() {
        set.insert(ConsentScope::View);
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rtc_config_includes_builtin_stun_and_turn() {
        let cfg = default_rtc_config();
        assert_eq!(cfg.ice_servers.len(), 2);
        assert!(cfg.ice_servers[0].urls.iter().any(|u| u == DEFAULT_STUN));
        assert!(cfg.ice_servers[1].urls.iter().any(|u| u == DEFAULT_TURN_URL));
        assert_eq!(cfg.ice_servers[1].username.as_deref(), Some(DEFAULT_TURN_USER));
        assert_eq!(cfg.ice_servers[1].credential.as_deref(), Some(DEFAULT_TURN_PASS));
    }

    #[test]
    fn parse_scopes_basic() {
        let s = parse_scopes("view,input,clipboard");
        assert!(s.contains(&ConsentScope::View));
        assert!(s.contains(&ConsentScope::Input));
        assert!(s.contains(&ConsentScope::Clipboard));
        assert!(!s.contains(&ConsentScope::FileTransfer));
    }

    #[test]
    fn parse_scopes_file_alias() {
        let s = parse_scopes("file");
        assert!(s.contains(&ConsentScope::FileTransfer));
    }

    #[test]
    fn parse_scopes_empty_falls_back_to_view() {
        let s = parse_scopes("");
        assert_eq!(s.len(), 1);
        assert!(s.contains(&ConsentScope::View));
    }

    #[test]
    fn parse_scopes_ignores_unknown() {
        let s = parse_scopes("view,bogus");
        assert!(s.contains(&ConsentScope::View));
        assert_eq!(s.len(), 1);
    }
}
