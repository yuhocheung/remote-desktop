//! macOS launchd 封装（仅 `service` feature 编译）。
//!
//! Host Agent 以「纯 Rust 后台服务」形态常驻：开机自启（LaunchAgent）、用户会话内运行。
//! 之所以用 LaunchAgent 而非 LaunchDaemon：屏幕录制 / 辅助功能权限按用户授予，
//! 进程必须跑在该用户的登录会话里，root daemon 拿不到这些 TCC 授权。
//!
//! 真正的控制逻辑（配对 / 投屏 / 注入 / 横幅）在 [`agent::run_host_agent`]，本模块只负责
//! 把 launchd 的生命周期翻译成 `shutdown` 标志：
//! - `service`  由 launchd 拉起，阻塞直至收到 SIGTERM/SIGINT（launchctl unload / 关机）；
//! - `install`  写 `~/Library/LaunchAgents/<LABEL>.plist` 并 `launchctl load`；
//! - `uninstall` `launchctl unload` 并删除 plist。

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent;
use crate::config::AgentConfig;

/// launchd 服务标识（反向域名约定，与 NSIS 安装器同源）。
const LABEL: &str = "com.rdcore.host-agent";

/// `service` 子命令入口：由 launchd 拉起，阻塞直至 shutdown。
///
/// launchd 直接执行本二进制（plist 的 `RunAtLoad`），进程本身即是服务——
/// 无需像 Windows 那样向 SCM 注册控制处理器；launchctl 需要停止时发 SIGTERM，
/// tokio 的信号监听把 shutdown 置位，agent 优雅收尾后进程退出，launchd 标记服务停止。
pub fn run_as_service() -> Result<()> {
    let cfg = AgentConfig::service_default();
    let shutdown = Arc::new(AtomicBool::new(false));

    // launchd 停服务 = SIGTERM；开发联调也可 Ctrl-C（SIGINT）。二者都置位 shutdown。
    let rt = tokio::runtime::Runtime::new().context("创建 tokio runtime 失败")?;
    rt.block_on(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("注册 SIGTERM 监听失败")?;
        let sig_shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => eprintln!("● 收到 SIGTERM（launchctl unload / 关机），正在停止…"),
                r = tokio::signal::ctrl_c() => {
                    if r.is_ok() {
                        eprintln!("● 收到 SIGINT，正在停止…");
                    }
                }
            }
            sig_shutdown.store(true, Ordering::SeqCst);
        });

        agent::run_host_agent(cfg, shutdown).await
    })
}

/// LaunchAgent plist 路径：`~/Library/LaunchAgents/<LABEL>.plist`。
fn plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("无法确定 HOME 目录")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// 生成 LaunchAgent plist 内容。
///
/// - `RunAtLoad`：加载即启动（用户登录后由 launchd 拉起）；
/// - `KeepAlive`：崩溃自动重启（与 Windows 服务的「失败恢复」语义对齐）；
/// - 日志落 `~/Library/Logs/`，避免 launchd 吞掉 stdout/stderr 后无处排查。
fn render_plist(exe: &std::path::Path) -> Result<String> {
    let home = std::env::var("HOME").context("无法确定 HOME 目录")?;
    let exe = exe.to_str().context("可执行文件路径包含非 UTF-8 字符")?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>service</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{home}/Library/Logs/rdcore-host-agent.out.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/Library/Logs/rdcore-host-agent.err.log</string>
</dict>
</plist>
"#
    ))
}

/// 安装服务：写 LaunchAgent plist 并 `launchctl load`（用户登录后自启）。
pub fn install() -> Result<()> {
    let path = plist_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("创建 LaunchAgents 目录失败")?;
    }
    let exe = std::env::current_exe().context("无法获取当前可执行文件路径")?;
    std::fs::write(&path, render_plist(&exe)?).context("写入 LaunchAgent plist 失败")?;

    // 先 unload 再 load：幂等——重复 install 时让新 plist 立即生效，无需先手动卸载。
    let _ = Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();
    let status = Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status()
        .context("执行 launchctl load 失败")?;
    if !status.success() {
        anyhow::bail!("launchctl load 失败（退出码 {status}）");
    }
    println!("✓ 服务 {LABEL} 已安装（用户登录后自启）。用 `launchctl list | grep rdcore` 查看。");
    Ok(())
}

/// 卸载服务：`launchctl unload` 并删除 plist。
pub fn uninstall() -> Result<()> {
    let path = plist_path()?;
    // 未加载 / plist 不存在时 unload 会报错，属正常，不视为失败（幂等）。
    if path.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", &path.to_string_lossy()])
            .status();
        std::fs::remove_file(&path).context("删除 LaunchAgent plist 失败")?;
    }
    println!("✓ 服务 {LABEL} 已卸载。");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_label_args_and_keepalive() {
        let exe = PathBuf::from("/usr/local/bin/rdcore-desktop");
        // HOME 在测试进程里必存在（cargo test 继承 shell 环境）。
        let xml = render_plist(&exe).unwrap();
        assert!(xml.contains(&format!("<string>{LABEL}</string>")));
        assert!(xml.contains("<string>/usr/local/bin/rdcore-desktop</string>"));
        assert!(xml.contains("<string>service</string>"));
        assert!(xml.contains("<key>RunAtLoad</key>"));
        assert!(xml.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn plist_path_under_user_launch_agents() {
        let path = plist_path().unwrap();
        let s = path.to_string_lossy();
        assert!(s.contains("Library/LaunchAgents"));
        assert!(s.ends_with(&format!("{LABEL}.plist")));
    }
}
