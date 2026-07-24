//! gateway 可执行入口：把控制面四个库服务编排为 HTTP(S) 服务。
//!
//! 环境变量：
//! - `GATEWAY_ADDR`（默认 `0.0.0.0:8080`）：监听地址。
//! - `GATEWAY_TLS_CERT` / `GATEWAY_TLS_KEY`：同时存在时启用 `wss`/HTTPS（rustls）。
//!   缺失则退回明文 HTTP（仅建议本地/内网联调用）。

use std::path::Path;
use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use gateway::{build_app, build_tls_server_config, GatewayState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state: Arc<GatewayState> = GatewayState::new();
    let app = build_app(state);

    let addr: std::net::SocketAddr = std::env::var("GATEWAY_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;

    match (
        std::env::var("GATEWAY_TLS_CERT"),
        std::env::var("GATEWAY_TLS_KEY"),
    ) {
        (Ok(cert), Ok(key)) => {
            let server_config = build_tls_server_config(Path::new(&cert), Path::new(&key))?;
            let rustls_config = RustlsConfig::from_config(Arc::new(server_config));
            println!("gateway listening on https://{addr}");
            axum_server::bind_rustls(addr, rustls_config)
                .serve(app.into_make_service())
                .await?;
        }
        _ => {
            println!("gateway listening on http://{addr}");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app.into_make_service()).await?;
        }
    }
    Ok(())
}
