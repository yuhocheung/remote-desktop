//! rdcore-desktop — 受控端 Host Agent（纯 Rust 后台服务）入口。
//!
//! 子命令：
//! - `run`     前台运行 Host Agent（生成配对 → 等 Viewer → 投屏 + 控制 + 不可伪造横幅）。
//! - `install` / `uninstall` / `service`  服务生命周期（仅 `service` feature；
//!   Windows 走 SCM，macOS 走 launchd）。

// Release 构建用 Windows GUI 子系统：安装包部署后（安装器 Exec / HKLM Run 自启 /
// 开始菜单快捷方式）启动时不弹出黑色终端窗口，配对二维码由 banner 进程的置顶
// 窗口与托盘弹窗呈现。Debug 构建保留控制台子系统，开发联调可看到 eprintln 日志。
// 注意：Release 下 `install`/`uninstall` 等 CLI 子命令不再向控制台输出——
// 其结果以退出码 / 服务管理器（services.msc）状态为准。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod banner;
mod config;
mod token_db;
#[cfg(all(feature = "service", target_os = "windows"))]
mod service;
#[cfg(all(feature = "service", target_os = "macos"))]
mod service_macos;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use crate::config::{Cli, Command};

// 服务子命令（install/uninstall/service）的平台分派：feature `service` 在支持的
// 平台上映射到对应实现；未启用 feature 或在不支持的平台给出同一错误。
// Windows → SCM（service.rs）；macOS → launchd（service_macos.rs）。
macro_rules! service_dispatch {
    ($cmd:ident) => {{
        #[cfg(all(feature = "service", target_os = "windows"))]
        {
            service::$cmd()
        }
        #[cfg(all(feature = "service", target_os = "macos"))]
        {
            service_macos::$cmd()
        }
        #[cfg(not(all(feature = "service", any(target_os = "windows", target_os = "macos"))))]
        {
            anyhow::bail!("服务封装未启用或不支持本平台；请以 `--features service` 重新构建")
        }
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => {
            let cfg = args.to_config();
            let shutdown = Arc::new(AtomicBool::new(false));
            agent::run_host_agent(cfg, shutdown).await
        }
        Command::Install => service_dispatch!(install),
        Command::Uninstall => service_dispatch!(uninstall),
        Command::Service => service_dispatch!(run_as_service),
    }
}
