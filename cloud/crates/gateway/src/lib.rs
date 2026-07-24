//! gateway — API 网关（控制面单入口）。
//!
//! 缺口 L 的云端控制面收口处。本 crate 把四个库服务（auth / permission / audit /
//! registry）编排为一个 HTTP 服务：
//!
//! - **路由**：`/health`、`/auth/login` 公开；其余端点受 Bearer 令牌保护。
//! - **鉴权中间件**：从 `Authorization: Bearer <token>` 取出令牌，用 `auth` 验签，
//!   把 `Claims` 注入请求扩展供下游 handler 读取；失败返回 401。
//! - **限流中间件**：固定窗口（每 client 每秒 N 次），超限 429。
//! - **TLS 终止**：生产用 `axum-server` + rustls（`build_tls_server_config` 复用与
//!   signaling-svc 一致的 PEM 加载逻辑）；`main.rs` 按环境变量决定走 http 还是 https。
//!
//! 设计约束（与三独立通道一致）：网关只处理控制面元数据，**绝不**接触屏幕/键击/
//! 剪贴板/媒体内容——那些走 media / data-channel，不在云端可见范围内。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use audit::{AuditEvent, AuditKind, AuditService};
use auth::AuthService;
use axum::{
    body::Body,
    extract::{Extension, Path, Request, State},
    http::{header, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use permission::PermissionService;
use registry::{DeviceRecord, Platform, RegistryService};

/// 权限范围（来自 permission，即 auth::Scope 的再导出，系统唯一真源）。
pub use permission::Scope;

/// 固定窗口限流器（每 key 在 window 内最多 limit 次）。
pub struct RateLimiter {
    window: Duration,
    limit: usize,
    buckets: Mutex<HashMap<String, (u64, usize)>>, // key -> (窗口起点秒, 计数)
}

impl RateLimiter {
    pub fn new(window: Duration, limit: usize) -> Arc<Self> {
        Arc::new(Self {
            window,
            limit,
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// 放行返回 true，超限返回 false。
    pub fn allow(&self, key: &str) -> bool {
        let now = now_secs();
        let mut b = self.buckets.lock().unwrap();
        let entry = b.entry(key.to_string()).or_insert((now, 0));
        // 窗口已过期则重置。
        if now >= entry.0 + self.window.as_secs() {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit {
            false
        } else {
            entry.1 += 1;
            true
        }
    }
}

/// 网关聚合状态（共享、线程安全）。
pub struct GatewayState {
    pub auth: AuthService,
    pub permission: PermissionService,
    pub audit: AuditService,
    pub registry: RegistryService,
    pub rate_limiter: Arc<RateLimiter>,
}

impl GatewayState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            auth: AuthService::new(),
            permission: PermissionService::new(),
            audit: AuditService::new(10_000),
            registry: RegistryService::new(),
            rate_limiter: RateLimiter::new(Duration::from_secs(1), 1_000),
        })
    }
}

// ---------------------------------------------------------------------------
// 请求/响应 DTO
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct LoginRequest {
    id: String,
    password: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LoginResponse {
    token: String,
    scopes: Vec<Scope>,
    expires_in: u64,
}

#[derive(serde::Deserialize)]
struct RegistryRegisterRequest {
    id: String,
    name: String,
    platform: Platform,
    public_key: String,
}

#[derive(serde::Deserialize)]
struct PermissionGrantRequest {
    scopes: Vec<Scope>,
}

#[derive(serde::Deserialize, Default)]
struct AuditQueryRequest {
    #[serde(default)]
    kind: Option<AuditKind>,
    #[serde(default)]
    subject: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers（公开）
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn login(
    State(state): State<Arc<GatewayState>>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    match state.auth.authenticate(&req.id, &req.password) {
        Ok(sub) => {
            let scopes = state.permission.list(&sub);
            let token = state.auth.issue_token(&sub, &scopes);
            state.audit.record(AuditKind::Login, &sub, "login ok");
            let resp = LoginResponse {
                token: token.raw().to_string(),
                scopes,
                expires_in: auth::TOKEN_TTL.as_secs(),
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(_) => (StatusCode::UNAUTHORIZED, "bad credentials").into_response(),
    }
}

// ---------------------------------------------------------------------------
// Handlers（受保护：注册表）
// ---------------------------------------------------------------------------

async fn list_devices(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(state.registry.list())).into_response()
}

async fn register_device(
    State(state): State<Arc<GatewayState>>,
    Extension(claims): Extension<auth::Claims>,
    Json(req): Json<RegistryRegisterRequest>,
) -> impl IntoResponse {
    let rec: DeviceRecord =
        state
            .registry
            .register(&req.id, &req.name, req.platform, &req.public_key);
    state.audit.record(
        AuditKind::DeviceRegister,
        &claims.sub,
        &format!("register {}", req.id),
    );
    (StatusCode::OK, Json(rec)).into_response()
}

async fn get_device(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.registry.get(&id) {
        Some(rec) => (StatusCode::OK, Json(rec)).into_response(),
        None => (StatusCode::NOT_FOUND, "device not found").into_response(),
    }
}

async fn touch_device(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if state.registry.update_last_seen(&id) {
        (StatusCode::OK, Json(true)).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(false)).into_response()
    }
}

async fn deregister_device(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let ok = state.registry.deregister(&id);
    (StatusCode::OK, Json(ok)).into_response()
}

// ---------------------------------------------------------------------------
// Handlers（受保护：权限）
// ---------------------------------------------------------------------------

async fn list_perms(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(state.permission.list(&id))).into_response()
}

async fn grant_perms(
    State(state): State<Arc<GatewayState>>,
    Extension(claims): Extension<auth::Claims>,
    Path(id): Path<String>,
    Json(req): Json<PermissionGrantRequest>,
) -> impl IntoResponse {
    state.permission.grant(&id, &req.scopes);
    let summary = format!(
        "grant {:?} to {}",
        req.scopes
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>(),
        id
    );
    state
        .audit
        .record(AuditKind::PermissionChange, &claims.sub, &summary);
    (StatusCode::OK, Json(state.permission.list(&id))).into_response()
}

async fn revoke_perms(
    State(state): State<Arc<GatewayState>>,
    Extension(claims): Extension<auth::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    state.permission.revoke(&id);
    state.audit.record(
        AuditKind::PermissionChange,
        &claims.sub,
        &format!("revoke {}", id),
    );
    (StatusCode::OK).into_response()
}

// ---------------------------------------------------------------------------
// Handlers（受保护：审计）
// ---------------------------------------------------------------------------

async fn query_audit(
    State(state): State<Arc<GatewayState>>,
    Extension(_claims): Extension<auth::Claims>,
    Json(req): Json<AuditQueryRequest>,
) -> impl IntoResponse {
    let q = audit::AuditQuery {
        kind: req.kind,
        subject: req.subject,
    };
    let events: Vec<AuditEvent> = state.audit.query(&q);
    (StatusCode::OK, Json(events)).into_response()
}

// ---------------------------------------------------------------------------
// 中间件
// ---------------------------------------------------------------------------

/// 鉴权中间件：校验 Bearer 令牌，把 Claims 注入扩展。
async fn auth_mw(
    State(state): State<Arc<GatewayState>>,
    mut req: Request<Body>,
    next: middleware::Next,
) -> impl IntoResponse {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| t.to_string());

    let token = match token {
        Some(t) => t,
        None => return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response(),
    };

    match state.auth.verify_token(&token) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            next.run(req).await
        }
        Err(_) => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
    }
}

/// 限流中间件：固定窗口，按 `X-Client-Id`（缺省 anon）计数。
async fn rate_limit_mw(
    State(state): State<Arc<GatewayState>>,
    req: Request<Body>,
    next: middleware::Next,
) -> impl IntoResponse {
    let key = req
        .headers()
        .get("x-client-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anon")
        .to_string();
    if state.rate_limiter.allow(&key) {
        next.run(req).await
    } else {
        (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response()
    }
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

/// 构建网关路由（已带鉴权 + 限流中间件，状态已固化）。
pub fn build_app(state: Arc<GatewayState>) -> Router {
    let protected = Router::new()
        .route("/registry", get(list_devices).post(register_device))
        .route("/registry/{id}", get(get_device).delete(deregister_device))
        .route("/registry/{id}/seen", post(touch_device))
        .route(
            "/permission/{id}",
            get(list_perms).post(grant_perms).delete(revoke_perms),
        )
        .route("/audit/query", post(query_audit))
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw));

    Router::new()
        .route("/health", get(health))
        .route("/auth/login", post(login))
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit_mw))
        .with_state(state)
}

/// 从 PEM 文件构建 rustls 服务端配置（与 signaling-svc 的 `build_tls_acceptor` 同源逻辑）。
pub fn build_tls_server_config(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> std::io::Result<rustls::ServerConfig> {
    use std::io::BufReader;
    let cert_bytes = std::fs::read(cert_path)?;
    let key_bytes = std::fs::read(key_path)?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_bytes.as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("读取证书 PEM 失败: {e}"),
                )
            })?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_bytes.as_slice()))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("读取私钥 PEM 失败: {e}"),
            )
        })?
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "私钥 PEM 中未找到私钥")
        })?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("构建 TLS 配置失败: {e}"),
            )
        })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{header, Method, Request};
    use rcgen::generate_simple_self_signed;
    use tower::ServiceExt;

    fn mk_req(method: Method, uri: &str, body: Option<&str>, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(Body::from(body.unwrap_or("").to_string()))
            .unwrap()
    }

    async fn body_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_ok() {
        let app = build_app(GatewayState::new());
        let resp = app
            .oneshot(mk_req(Method::GET, "/health", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_rejects_bad_credentials() {
        let state = GatewayState::new();
        state.auth.register_device("dev-1", "pw");
        let app = build_app(state);
        let resp = app
            .oneshot(mk_req(
                Method::POST,
                "/auth/login",
                Some(r#"{"id":"dev-1","password":"wrong"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn full_flow_login_then_protected_crud_and_audit() {
        let state = GatewayState::new();
        state.auth.register_device("dev-1", "pw");
        state
            .permission
            .grant("dev-1", &[Scope::View, Scope::Input]);
        let app = build_app(state);

        // 1) login
        let resp = app
            .clone()
            .oneshot(mk_req(
                Method::POST,
                "/auth/login",
                Some(r#"{"id":"dev-1","password":"pw"}"#),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let login: LoginResponse = body_json(resp).await;
        assert!(!login.token.is_empty());
        let got: std::collections::HashSet<_> = login.scopes.into_iter().collect();
        let expected: std::collections::HashSet<_> =
            vec![Scope::View, Scope::Input].into_iter().collect();
        assert_eq!(got, expected);

        // 2) 无令牌访问受保护 -> 401
        let resp = app
            .clone()
            .oneshot(mk_req(Method::GET, "/registry", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 3) 持令牌列出设备（空）
        let resp = app
            .clone()
            .oneshot(mk_req(Method::GET, "/registry", None, Some(&login.token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list: Vec<DeviceRecord> = body_json(resp).await;
        assert!(list.is_empty());

        // 4) 注册设备
        let resp = app
            .clone()
            .oneshot(mk_req(
                Method::POST,
                "/registry",
                Some(r#"{"id":"dev-2","name":"Bob","platform":"MacOS","public_key":"abc"}"#),
                Some(&login.token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let rec: DeviceRecord = body_json(resp).await;
        assert_eq!(rec.id, "dev-2");
        assert_eq!(rec.platform, Platform::MacOS);

        // 5) 授权 dev-2 拥有 Clipboard
        let resp = app
            .clone()
            .oneshot(mk_req(
                Method::POST,
                "/permission/dev-2",
                Some(r#"{"scopes":["Clipboard"]}"#),
                Some(&login.token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let scopes: Vec<Scope> = body_json(resp).await;
        assert!(scopes.contains(&Scope::Clipboard));

        // 6) 审计查询：应能看到 dev-1 的 DeviceRegister 事件
        let resp = app
            .clone()
            .oneshot(mk_req(
                Method::POST,
                "/audit/query",
                Some(r#"{"kind":"DeviceRegister","subject":null}"#),
                Some(&login.token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let events: Vec<AuditEvent> = body_json(resp).await;
        assert!(events.iter().any(|e| e.subject == "dev-1"));

        // 7) 撤销 dev-2 权限 -> 200
        let resp = app
            .clone()
            .oneshot(mk_req(
                Method::DELETE,
                "/permission/dev-2",
                None,
                Some(&login.token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_rejected() {
        let app = build_app(GatewayState::new());
        let resp = app
            .oneshot(mk_req(
                Method::GET,
                "/registry",
                None,
                Some("garbage.token.here"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rate_limiter_fixed_window() {
        let rl = RateLimiter::new(Duration::from_secs(1), 3);
        assert!(rl.allow("k"));
        assert!(rl.allow("k"));
        assert!(rl.allow("k"));
        assert!(!rl.allow("k")); // 第 4 次超限
        assert!(rl.allow("other")); // 不同 key 独立计数
    }

    #[test]
    fn tls_server_config_builds_from_self_signed_pem() {
        // 显式安装 ring Provider：与 rdcore-rtc（aws-lc-rs）同 workspace 构建时
        // rustls 两个 Provider 都被启用，0.23 无法自动判定进程级默认，须手动指定。
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir();
        let cert = dir.join("gw_test_cert.pem");
        let key = dir.join("gw_test_key.pem");
        let certified = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();

        let cfg = build_tls_server_config(&cert, &key);
        assert!(cfg.is_ok(), "TLS 配置构建失败: {cfg:?}");

        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);
    }
}
