//! Windows Service Control Manager 封装（仅 `service` feature 编译）。
//!
//! Host Agent 以「纯 Rust 后台服务」形态常驻：开机自启、低权限沙箱、不拖 Flutter engine。
//! 真正的控制逻辑（配对 / 投屏 / 注入 / 横幅）在 [`agent::run_host_agent`]，本模块只负责
//! 把 SCM 的生命周期事件（start/stop/shutdown）翻译成 `shutdown` 标志，并回报服务状态。

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{register, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::agent;
use crate::config::AgentConfig;

const SERVICE_NAME: &str = "RdCoreDesktopAgent";
const SERVICE_DISPLAY: &str = "RdCore Remote Desktop Host Agent";

/// `service` 子命令入口：把控制权交给 SCM（阻塞，直到服务停止）。
pub fn run_as_service() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .context("注册到 Windows 服务控制管理器失败")?;
    Ok(())
}

/// SCM 期望的原始 `extern "system"` 服务入口；把宽字符参数数组转成 `Vec<OsString>` 后转交
/// Rust 侧的 [`service_main`]。
extern "system" fn ffi_service_main(argc: u32, argv: *mut *mut u16) {
    let args = parse_service_args(argc, argv);
    service_main(args);
}

/// 解析 SCM 传入的 `argc` / `argv`（宽字符指针数组）为 `Vec<OsString>`。
fn parse_service_args(argc: u32, argv: *mut *mut u16) -> Vec<OsString> {
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        let ptr = unsafe { *argv.add(i) };
        let mut len = 0usize;
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = slice::from_raw_parts(ptr, len);
            args.push(OsString::from_wide(slice));
        }
    }
    args
}

/// 从 SCM 接收的「服务主函数」（Rust 侧）。
fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service_inner() {
        eprintln!("service error: {e:#}");
    }
}

fn run_service_inner() -> Result<()> {
    // 1) 注册控制处理器（stop / shutdown）。返回 shutdown 标志的发送端。
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_handler = shutdown.clone();
    let event_handler = move |control_event| match control_event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            shutdown_for_handler.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = register(SERVICE_NAME, event_handler)
        .context("注册服务控制处理器失败")?;

    // 2) 报告「启动中」。
    set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    )
    .context("回报 StartPending 失败")?;

    // 3) 在一个专用 tokio runtime 上跑 Host Agent。
    let cfg = AgentConfig::service_default();
    let rt = tokio::runtime::Runtime::new().context("创建 tokio runtime 失败")?;
    let agent_handle = {
        let shutdown = shutdown.clone();
        rt.spawn(async move { agent::run_host_agent(cfg, shutdown).await })
    };

    // 4) 报告「运行中」。
    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    )
    .context("回报 Running 失败")?;

    // 5) 阻塞直至 shutdown（控制处理器置位）或 agent 自身退出。
    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }

    // 6) 等 agent 收尾。
    let _ = rt.block_on(agent_handle);

    // 7) 报告「已停止」。
    set_status(&status_handle, ServiceState::Stopped, ServiceControlAccept::empty())
        .context("回报 Stopped 失败")?;
    Ok(())
}

/// 构造并汇报一次服务状态。
fn set_status(
    handle: &windows_service::service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    accept: ServiceControlAccept,
) -> windows_service::Result<()> {
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    })
}

/// 安装服务：在 SCM 注册本可执行文件，参数 `service`（按需启动）。
pub fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
        .context("连接服务控制管理器失败")?;
    let exe = std::env::current_exe().context("无法获取当前可执行文件路径")?;
    let service_info = windows_service::service::ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: windows_service::service::ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    manager
        .create_service(&service_info, ServiceAccess::empty())
        .context("创建服务失败（可能权限不足，请以管理员运行）")?;
    println!("✓ 服务 {SERVICE_NAME} 已安装（按需启动）。用 `sc start {SERVICE_NAME}` 启动。");
    Ok(())
}

/// 卸载服务。
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("连接服务控制管理器失败")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            windows_service::service::ServiceAccess::DELETE,
        )
        .context("打开服务失败（可能未安装）")?;
    service.delete().context("删除服务失败")?;
    println!("✓ 服务 {SERVICE_NAME} 已卸载。");
    Ok(())
}
