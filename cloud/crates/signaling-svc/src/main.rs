//! signaling-svc 独立进程入口。
//!
//! 用法（环境变量）：
//! - 监听地址：`SIGNALING_ADDR`（默认 `127.0.0.1:8080`）。
//! - TLS：另设 `SIGNALING_TLS_CERT`（证书 PEM）与 `SIGNALING_TLS_KEY`（私钥 PEM），
//!   服务即升级为 `wss://`（证书私钥由 rustls 直接加载，无需外部网关）。
//! - 鉴权（三选一，互斥；都不设则为**开放模式**）：
//!   - `SIGNALING_TOKEN_DB=<path>`：per-session 一次性 token（B2，生产模式）。
//!     要求 Host 与 signaling-svc **同机** —— Host 的 `create_pairing()` 把 session 写入该文件。
//!     ⚠️ 跨机 / 远程 VPS 部署时 Windows Host 无法写入 VPS 的文件系统，token 校验会对每个连接
//!     返回 401。远程部署**不要**用此模式。
//!   - `SIGNALING_AUTH_TOKEN=<secret>`：shared-secret 模式，所有客户端须带 `?token=<secret>`。
//!     ⚠️ 当前 Host 连接不携带 token（`build_signaling_url` 不追加），故 shared-secret 模式会
//!     拒绝 Host。仅当 Viewer 端也统一带同一 secret 时有意义。
//!   - 都不设：**开放模式**（仅按 session 是否注册放行；session 为 16 字节随机值不可猜，
//!     但无任何凭据校验）。自托管演示可用，公网多租户请勿用。

use std::path::PathBuf;

/// 按环境变量构造鉴权/TLS 加固配置（与 `SignalingConfig` 的各构造器对齐）。
fn build_config() -> signaling_svc::SignalingConfig {
    // 鉴权优先级：per-session token 文件 > shared-secret > 开放。
    if let Ok(db) = std::env::var("SIGNALING_TOKEN_DB") {
        if !db.trim().is_empty() {
            let cfg = signaling_svc::SignalingConfig::per_session(signaling_svc::TokenStore::new());
            // 启动即装载一次（握手期仍会按连接刷新，见 lib.rs 的 reload_from_file）。
            if let Some(store) = &cfg.token_store {
                store.reload_from_file(std::path::Path::new(db.trim()));
            }
            return cfg;
        }
    }
    if let Ok(tok) = std::env::var("SIGNALING_AUTH_TOKEN") {
        if !tok.trim().is_empty() {
            return signaling_svc::SignalingConfig::with_token(tok.trim().to_string());
        }
    }
    signaling_svc::SignalingConfig::open()
}

fn main() -> std::io::Result<()> {
    let addr = std::env::var("SIGNALING_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let mut cfg = build_config();

    // TLS 叠加（可选）；与鉴权模式正交。
    match (
        std::env::var("SIGNALING_TLS_CERT"),
        std::env::var("SIGNALING_TLS_KEY"),
    ) {
        (Ok(cert), Ok(key)) if !cert.is_empty() && !key.is_empty() => {
            eprintln!("signaling-svc listening on wss://{addr} (TLS enabled)");
            cfg.tls = Some(signaling_svc::TlsConfig {
                cert_path: PathBuf::from(cert),
                key_path: PathBuf::from(key),
            });
        }
        _ => {
            eprintln!("signaling-svc listening on ws://{addr} (plaintext, internal only)");
        }
    }

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        signaling_svc::serve_listener_with_config(listener, cfg).await
    })
}
