//! OS 级不可伪造横幅的宿主侧管理。
//!
//! 横幅由**独立进程**（`rdcore-banner` 二进制，生产以 `windows-native` 构建为置顶窗口）绘制，
//! 本模块只负责：① 在自身同级目录或显式路径找到并拉起该二进制；② 提供一个把
//! [`rdcore_banner::BannerState`] 推送到默认 UDP 端口（48173）的客户端。主程序**无法**伪造
//! 横幅内容——横幅只被动接收来自密码学确认状态（`SecurityIndicator`）的映射结果。

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::Context;
use rdcore_banner::{BannerClient, DEFAULT_BANNER_PORT};

#[cfg(windows)]
use std::ffi::c_void;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::*;
#[cfg(windows)]
use windows_sys::Win32::Foundation::*;

/// 在自身同级目录或显式路径查找 `rdcore-banner` 二进制。
fn resolve_banner_bin(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for name in ["rdcore-banner.exe", "rdcore-banner"] {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// 拉起独立的横幅进程（真实置顶窗口由该二进制在 `windows-native` 下绘制）。
///
/// `qr_code`：配对码（`<32hex session>:<64hex token>`），经 `--qr` 传给横幅进程，
/// 用于其托盘图标的「配对二维码」弹窗；`None` 时横幅仅作状态条。
/// 找不到二进制时返回 `None`（Agent 仍可运行，仅无 OS 级横幅），并经 `on_missing` 提示。
pub fn spawn_banner(
    explicit: Option<PathBuf>,
    qr_code: Option<String>,
    on_missing: &dyn Fn(&str),
) -> anyhow::Result<Option<Child>> {
    let bin = match resolve_banner_bin(explicit) {
        Some(b) => b,
        None => {
            on_missing("未找到 rdcore-banner 二进制；将以无 OS 横幅模式运行（状态仅打日志）。");
            return Ok(None);
        }
    };
    let mut cmd = Command::new(&bin);
    if let Some(code) = qr_code {
        cmd.arg("--qr").arg(code);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("拉起横幅进程失败: {}", bin.display()))?;
    // 把 banner 子进程塞进 Job Object：父进程（含被硬杀）退出时 OS 自动终止它，杜绝孤儿残留。
    #[cfg(windows)]
    unsafe {
        attach_kill_on_parent_exit(&child);
    }
    Ok(Some(child))
}

/// 把子进程放进「父进程退出即被 OS 终止」的 Job Object（仅 Windows 编译）。
///
/// 机制：创建一个 Job，设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，把 `child` 塞进去；
/// Job 句柄由父进程持有。父进程**无论怎么死**（正常退出 / 崩溃 / 被 `TerminateProcess` /
/// 服务被 SCM 强停），OS 关闭 Job 句柄时都会把 Job 内所有进程一并终止。这是 Windows 上
/// 唯一能扛住「父被硬杀」的孤儿回收手段（`Ctrl-C` handler / `atexit` / panic hook 都不行）。
///
/// 成功后**故意不关闭** Job 句柄（它是原始 `HANDLE`，无 Drop），让它随父进程退出被 OS 关闭，
/// 从而触发自动终止。若创建/赋值失败（如父进程自身已在别的 Job 中），静默回退到 agent
/// 优雅退出路径里的 `child.kill()`，不报错。
#[cfg(windows)]
unsafe fn attach_kill_on_parent_exit(child: &Child) {
    let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
    if job == 0 {
        return;
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if SetInformationJobObject(
        job,
        JobObjectExtendedLimitInformation,
        &info as *const _ as *const c_void,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    ) == 0
    {
        CloseHandle(job);
        return;
    }
    let h = child.as_raw_handle() as HANDLE;
    if AssignProcessToJobObject(job, h) == 0 {
        // 父进程已被外层 Job 包含（调试器 / 服务管理器）→ 容错回退，不强行塞。
        CloseHandle(job);
        return;
    }
    // 成功：保持 Job 句柄打开，直到父进程退出由 OS 关闭 → 自动终止 banner。
}

/// 构造一个指向默认横幅端口（UDP 回环 48173）的推送客户端。
pub fn banner_client() -> anyhow::Result<BannerClient> {
    BannerClient::udp(DEFAULT_BANNER_PORT).context("无法创建横幅推送客户端（绑定 UDP 失败）")
}
