//! signaling-svc — 信令中继服务（仅转发 SDP/ICE，不处理媒体或控制数据）。
//!
//! 设计要点（对应架构文档 §1/§5）：
//! - 控制通道走 WebSocket；本服务只做**按 `session_id` 的纯中继**，不解析、不处理消息内容。
//! - 连接建立时，session_id 通过 WebSocket URL 路径携带（`ws://host/<session_hex>`），
//!   服务端据此在连接一建立就把该连接注册进对应"房间"。这样无论哪一端先发消息，
//!   另一端只要已连上就能立即收到（解决"先发方在对方注册前发消息"的会合问题）。
//! - 非法/缺失的 session_id 在 WebSocket 握手期即被拒绝（返回 400），不完成升级、不入房间，
//!   提前挡掉明显异常的请求（对应之前"先升级再关"的浪费）。
//! - 接收侧强制 P0 的 `decode_limited`（F3 上限），防分配炸弹；非法/超长帧直接丢弃。
//! - 对内容中立：属于本房间的 `InputEvent`/`Clipboard`/`Heartbeat` 也会被原样转发；
//!   但按架构它们最终应走 WebRTC DataChannel（P3），信令服务器只见 SDP/ICE。

use std::collections::HashMap;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rdcore_proto::{decode_limited, SessionId, MAX_SIGNALING_MESSAGE_LEN};
use rustls::pki_types::CertificateDer;
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::http::{Response as HttpResponse, StatusCode};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// 单个连接对内发送通道（writer 任务把字节写回该 WebSocket）。
type PeerTx = UnboundedSender<WsMessage>;
/// 房间表：`session_id` -> 该房间内所有连接的发送端（连同连接 id 便于断线清理）。
type RoomMap = Arc<Mutex<HashMap<SessionId, Vec<(usize, PeerTx)>>>>;

/// 配对 token 记录的存活期（§5.1：自登记起 15 分钟）。
///
/// 这是内存条目的兜底清理期，**不是配对的有效期**：同机文件模式下，每次握手前的
/// 文件回灌会用 Host 当前发布的配对刷新条目（见 [`TokenStore::reload_from_file`]），
/// 因此只要受控端在线并心跳刷新 token 库文件，配对就持续有效、可重复扫码连接。
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);

/// token 库文件的心跳新鲜度阈值：文件 mtime 超过该时长未更新，视为「受控端已退出
/// （或崩溃）」，文件内容不再作为授权依据，所有文件来源条目被回收。
/// Host 侧（rdcore-desktop Agent / FFI 发布）需以明显小于本阈值的周期重写文件。
pub const TOKEN_FILE_STALE_AFTER: Duration = Duration::from_secs(3 * 60);

/// 死连接回收：每 30s 向每个连接发一次 WebSocket Ping。
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// 死连接回收：超过 90s 没收到对端任何帧（含 Pong）即判定死亡并回收连接。
///
/// 判定依据是「连 Pong 都不回」，不是「没有业务消息」——Host 空闲等 Viewer 时
/// 可能数小时无业务流量，但只要协议栈活着就会自动回 Pong，不会被误杀。
const REAP_AFTER: Duration = Duration::from_secs(90);

/// per-session 配对 token 记录（§5.1）。
///
/// token 由受控端（Host）的 `Connection::create_pairing()` 生成并经 [`TokenStore::register`]
/// 或共享 token 库文件登记。配对在受控端在线期间持续有效：Viewer 扫码/输码后携带
/// `?token=` 连信令，校验通过即可入房，**不焚毁**——断线后重扫同一二维码可直接重连。
/// 失效路径：受控端主动取消 / 刷新配对（覆写或删除 token 库文件），或受控端退出
/// （token 库文件心跳停更，超过 [`TOKEN_FILE_STALE_AFTER`] 后被判陈旧）。
#[derive(Clone, Debug)]
struct TokenEntry {
    /// 过期时刻（登记/回灌 + TOKEN_TTL；文件回灌每次握手都会刷新，故仅为兜底）。
    expires_at: Instant,
    /// 配对 token 的 SHA-256（hex 字符串的摘要）。`None` 仅兼容历史空 token 注册。
    /// 持有 token 值即可在握手期比对，防止"知道 session 已注册"就冒领（会话 id 难猜，
    /// 但 token 值才是 viewer 身份的真正凭证）。服务端不存储明文 token。
    token_hash: Option<[u8; 32]>,
    /// 是否来自 token 库文件回灌。文件来源条目受「文件即事实」 reconcile 管理
    /// （文件删行 / 删文件 / 心跳过期即回收）；内存注册条目不受文件影响。
    from_file: bool,
}

/// 配对 token 存储：`session_id -> TokenEntry`。
///
/// 与房间表分离：token 只管"能不能进房间握手"。配对不焚毁，受控端在线期间
/// 同一配对码可重复建连；真正鉴权仍由 E2E 签名 + 同意层保证。
/// 线程安全，供握手回调与配对注册共享。
#[derive(Clone, Default)]
pub struct TokenStore {
    inner: Arc<Mutex<HashMap<SessionId, TokenEntry>>>,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// host 配对时注册一条 token（内存登记，TOKEN_TTL 兜底过期）。
    /// `token_hex` 经 SHA-256 存入 `token_hash`；握手期 viewer 须出示原 token，
    /// 服务端比对哈希，避免"只凭 session 已注册就冒领"。
    pub fn register(&self, session: SessionId, token_hex: &str) {
        let hash = sha256(token_hex);
        let mut g = self.inner.lock().unwrap();
        g.insert(
            session,
            TokenEntry {
                expires_at: Instant::now() + TOKEN_TTL,
                token_hash: Some(hash),
                from_file: false,
            },
        );
    }

    /// 校验 token 值：仅当 token 存在、未过期、**且出示的 token 哈希匹配**时返回 true。
    /// 不焚毁——受控端在线期间同一配对码可重复建连（断线重扫重连）。
    /// token 值错误一律拒（不比对则放行属于历史兼容，见 `token_hash == None`）。
    /// 比对用 SHA-256，服务端不存储明文 token。
    pub fn verify(&self, session: &SessionId, presented: &str) -> bool {
        let g = self.inner.lock().unwrap();
        match g.get(session) {
            Some(e) if Instant::now() < e.expires_at => match &e.token_hash {
                // 历史兼容：未登记 token 哈希（空 token 注册 / 文件无 token 列）时仅检查未过期。
                Some(h) => Sha256::digest(presented.as_bytes())[..] == h[..],
                None => true,
            },
            _ => false,
        }
    }

    /// 该 session 是否已注册（且未过期）。供 Host（注册方）**不带 token** 连接时
    /// 判断放行。见 `handle_conn` 的 A5↔B2 分支。
    pub fn has_session(&self, session: &SessionId) -> bool {
        let g = self.inner.lock().unwrap();
        matches!(g.get(session), Some(e) if Instant::now() < e.expires_at)
    }

    /// 清理所有过期记录（后台周期任务调用）。
    pub fn sweep(&self) {
        let now = Instant::now();
        self.inner.lock().unwrap().retain(|_, e| now < e.expires_at);
    }

    /// 从「共享 token 库文件」装载当前活跃配对，并以文件为事实做 reconcile（A5↔B2 对接点）。
    ///
    /// 同机部署下，受控端（`rdcore-desktop` Agent 或 FFI 发布的 Flutter Host）把配对写入该文件
    /// 并以心跳周期重写保持新鲜；signaling-svc 在每次握手校验前调用本函数刷新内存库：
    /// - 文件中的每条 `session_hex[\ttoken_hex]` 都会（重新）登记为文件来源条目；
    /// - 内存中**不再出现在文件里**的文件来源条目被回收——受控端主动取消配对（删文件）、
    ///   刷新二维码（覆写为新 session）即在下一次握手时生效；
    /// - 文件 mtime 超过 [`TOKEN_FILE_STALE_AFTER`]（受控端退出/崩溃，心跳停更）时按
    ///   空文件处理，回收全部文件来源条目——配对自动失效；
    /// - 内存注册（[`TokenStore::register`]）的条目不受文件影响。
    ///
    /// 带 token 列时登记其哈希，使经文件回灌的配对也走 token 值校验；缺 token 列则仅按
    /// session 注册（历史兼容）。文件缺失 / 格式错误时格式错误行静默跳过。
    pub fn reload_from_file(&self, path: &std::path::Path) {
        // 心跳新鲜度检查：受控端存活期间会以 << TOKEN_FILE_STALE_AFTER 的周期重写文件；
        // 文件陈旧（受控端已退出/崩溃）时视为无授权，回收所有文件来源条目。
        let fresh = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or(Duration::MAX) < TOKEN_FILE_STALE_AFTER)
            .unwrap_or(false);
        let content = if fresh {
            std::fs::read_to_string(path).unwrap_or_default()
        } else {
            String::new()
        };

        let mut file_sessions = std::collections::HashSet::new();
        let mut parsed = Vec::new();
        for line in content.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            let sid_hex = parts.first().copied().unwrap_or("").trim();
            if sid_hex.is_empty() {
                continue;
            }
            let bytes = from_hex(sid_hex);
            if bytes.is_none() || bytes.as_ref().unwrap().len() != 16 {
                continue;
            }
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes.unwrap());
            let sid = SessionId(arr);
            // 第二列（可选）为 token_hex，登记其哈希，使文件回灌的配对也需校验 token 值。
            let token_hash = parts.get(1).map(|t| sha256(t.trim()));
            file_sessions.insert(sid);
            parsed.push((sid, token_hash));
        }

        let mut g = self.inner.lock().unwrap();
        // reconcile：回收文件中已不存在的文件来源条目（主动取消 / 刷新 / 退出 / 心跳过期）。
        g.retain(|sid, e| !e.from_file || file_sessions.contains(sid));
        for (sid, token_hash) in parsed {
            g.insert(
                sid,
                TokenEntry {
                    expires_at: Instant::now() + TOKEN_TTL,
                    token_hash,
                    from_file: true,
                },
            );
        }
    }
}

/// TLS 配置（缺口 L / P0-D 后续）：启用后信令服务器在 `wss://` 上服务，
/// 证书与私钥为 PEM 文件。客户端须改用 `wss://` 连接。生产部署建议由网关终结 TLS，
/// 此配置用于"无网关、单二进制直出 TLS"的场景（避免信令明文暴露凭据/token）。
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// 证书链 PEM 路径（含服务端证书，可含中间证书）。
    pub cert_path: PathBuf,
    /// 私钥 PEM 路径（PKCS#8 或 SEC1/传统 PEM）。
    pub key_path: PathBuf,
}

/// B5（韧性面）+ L（TLS）信令加固配置。
///
/// 鉴权（shared-secret 或 per-session 配对 token）经 query 参数 `?token=` 在握手期校验；
/// `tls` 为 `Some` 时再叠加传输层加密（`wss://`），信令不再以明文暴露。
#[derive(Clone, Default)]
pub struct SignalingConfig {
    /// 预共享密钥（旧模式，向后兼容）。`Some` 时客户端必须带 `?token=<该值>`，否则 401。
    /// 与 per-session token 二选一；生产走 [`Self::per_session`]。
    pub auth_token: Option<String>,
    /// 单个房间允许的最大并发连接数（限流，防 session_id 爆破 / 房间被挤爆）。默认 8。
    pub max_per_room: usize,
    /// per-session 配对 token 存储（B2）。`Some` 时按 §5.1 校验（不焚毁）；优先于 `auth_token`。
    pub token_store: Option<TokenStore>,
    /// TLS 配置。`Some` 时服务升级为 `wss://`；`None` 仍为明文 `ws://`（仅限可信内网）。
    pub tls: Option<TlsConfig>,
}

impl std::fmt::Debug for SignalingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalingConfig")
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("max_per_room", &self.max_per_room)
            .field("token_store", &self.token_store.as_ref().map(|_| "<store>"))
            .finish()
    }
}

impl SignalingConfig {
    /// 无鉴权的默认配置（本地/测试）。
    pub fn open() -> Self {
        Self {
            auth_token: None,
            max_per_room: 8,
            token_store: None,
            tls: None,
        }
    }

    /// 启用 shared-secret 鉴权的配置（旧模式，向后兼容）。
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            auth_token: Some(token.into()),
            max_per_room: 8,
            token_store: None,
            tls: None,
        }
    }

    /// 启用 per-session 配对 token 鉴权（B2，生产模式）。
    ///
    /// 传入的 `TokenStore` 与 host 配对流程共享：host `create_pairing()` 后经
    /// [`TokenStore::register`] 或共享 token 库文件登记；viewer 握手时按 session_id
    /// 校验（不焚毁，受控端在线期间可重复扫码建连）。
    pub fn per_session(store: TokenStore) -> Self {
        Self {
            auth_token: None,
            max_per_room: 8,
            token_store: Some(store),
            tls: None,
        }
    }

    /// 启用 TLS（wss://）的配置（无鉴权，仅加密）。生产可在此基础上叠加
    /// `auth_token` / `token_store`（用 [`Self::per_session`] 等构造后改 `tls` 字段）。
    pub fn with_tls(tls: TlsConfig) -> Self {
        Self {
            auth_token: None,
            max_per_room: 8,
            token_store: None,
            tls: Some(tls),
        }
    }
}

/// 绑定的地址并永久服务（接受连接）。一般用于独立进程入口。
pub async fn run_on(addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_listener(listener).await
}

/// 在已绑定的 listener 上服务（测试里可先 bind 到 `127.0.0.1:0` 拿到空闲端口）。
///
/// 默认无鉴权（向后兼容现有测试）；要鉴权用 [`serve_listener_with_config`]。
pub async fn serve_listener(listener: TcpListener) -> std::io::Result<()> {
    serve_listener_with_config(listener, SignalingConfig::open()).await
}

/// 带加固配置的服务入口（B5：shared-secret 鉴权 + 房间限流；L：可选 TLS）。
///
/// `cfg.tls` 为 `Some` 时自动升级为 `wss://`（先 TLS 握手再 WebSocket 升级），
/// 为 `None` 时仍是明文 `ws://`（仅限可信内网）。
pub async fn serve_listener_with_config(
    listener: TcpListener,
    cfg: SignalingConfig,
) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    match &cfg.tls {
        Some(tls) => {
            let acceptor = Arc::new(build_tls_acceptor(&tls.cert_path, &tls.key_path)?);
            serve_tls(listener, acceptor, cfg).await
        }
        None => serve_plain(listener, cfg).await,
    }
}

/// 明文（ws://）服务循环。
async fn serve_plain(listener: TcpListener, cfg: Arc<SignalingConfig>) -> std::io::Result<()> {
    let rooms: RoomMap = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicUsize::new(1));
    loop {
        let (stream, _peer) = listener.accept().await?;
        let rooms = rooms.clone();
        let next_id = next_id.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, rooms, &next_id, &cfg).await {
                // 连接异常结束（含非法/缺失 session_id、鉴权失败被拒）。生产应走结构化日志。
                eprintln!("signaling connection ended: {e}");
            }
        });
    }
}

/// 加密（wss://）服务循环：每个连接先过 TLS 握手，再进 WebSocket 升级 + 中继逻辑。
async fn serve_tls(
    listener: TcpListener,
    acceptor: Arc<TlsAcceptor>,
    cfg: Arc<SignalingConfig>,
) -> std::io::Result<()> {
    let rooms: RoomMap = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicUsize::new(1));
    loop {
        let (stream, _peer) = listener.accept().await?;
        let rooms = rooms.clone();
        let next_id = next_id.clone();
        let cfg = cfg.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            // 先 TLS 握手；失败（证书/协议错误）直接丢弃该连接，不进入 WebSocket。
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("signaling TLS handshake failed: {e}");
                    return;
                }
            };
            if let Err(e) = handle_conn(tls_stream, rooms, &next_id, &cfg).await {
                eprintln!("signaling connection ended: {e}");
            }
        });
    }
}

async fn handle_conn<S>(
    stream: S,
    rooms: RoomMap,
    next_id: &Arc<AtomicUsize>,
    cfg: &SignalingConfig,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 从 WebSocket 升级请求的路径里取出 session_id（如 `/7f7f...7f`），query 里取 token。
    let mut session_hex = String::new();
    let mut presented_token: Option<String> = None;
    // `Callback::on_request` 的返回类型由 tokio-tungstenite 固定为
    // `Result<Response, ErrorResponse>`，其 Err 变体较大，触发 clippy 的
    // `result_large_err`；此处无法改返回类型，故局部放行。
    #[allow(clippy::result_large_err)]
    let ws = tokio_tungstenite::accept_hdr_async(
        stream,
        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
         resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
            session_hex = req
                .uri()
                .path()
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or("")
                .to_string();
            presented_token = req.uri().query().and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("token=").map(|v| v.to_string()))
            });
            // session_id 非法/缺失则在握手期直接拒绝（返回 400），
            // 避免"先完成升级再关连接"的浪费，也提前挡掉明显异常的请求。
            let sid_bytes = from_hex(&session_hex);
            if sid_bytes.is_none() {
                return Err(HttpResponse::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Some(
                        "missing or invalid session_id in url path".to_string(),
                    ))
                    .unwrap());
            }
            // B2 per-session 配对 token（优先）：按 session_id 校验（不焚毁）。
            // viewer 必须带非空 `?token=` 作为"持有配对凭据"的声明；真正的访问控制
            // 是 token 库中该 session 的记录存在、未过期且哈希匹配。受控端在线期间
            // 同一配对码可重复建连（断线重扫重连）；取消/刷新/退出经文件回灌
            // reconcile 生效（见 `reload_from_file`）。
            if let Some(store) = &cfg.token_store {
                // A5↔B2 对接点：同机受控端把配对写入 `SIGNALING_TOKEN_DB` 文件并心跳
                // 保鲜；此处校验前先以文件为事实 reconcile 内存库（拾取新配对、
                // 回收已取消/已过期配对，不受启动时序影响）。
                if let Ok(db) = std::env::var("SIGNALING_TOKEN_DB") {
                    store.reload_from_file(std::path::Path::new(&db));
                }
                let session = SessionId(sid_bytes.unwrap());
                let presented = matches!(&presented_token, Some(t) if !t.is_empty());
                // Host（注册方）不带 token 连接即视为房主，凭"session 已注册"放行；
                // Viewer 须带配对 token，哈希校验通过即放行（不焚毁，可重复扫码）。
                // 两端都以"session 已注册"为前提，session_id 不可猜测（16 字节随机），
                // 真正鉴权仍由 E2E 签名 + 同意层保证。
                let admitted = if presented {
                    // viewer 必须出示与登记时一致的 token（SHA-256 哈希比对）。
                    store.verify(&session, presented_token.as_deref().unwrap_or(""))
                } else {
                    // 房主（host）不带 token 连接：session 已注册即放行。
                    store.has_session(&session)
                };
                if !admitted {
                    return Err(HttpResponse::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Some(
                            "unauthorized: invalid/expired/revoked pairing token".to_string(),
                        ))
                        .unwrap());
                }
            } else if let Some(expected) = &cfg.auth_token {
                // 旧模式（向后兼容）：shared-secret 比对。
                let ok = matches!(&presented_token, Some(t) if t == expected);
                if !ok {
                    return Err(HttpResponse::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Some("unauthorized: bad or missing token".to_string()))
                        .unwrap());
                }
            }
            Ok(resp)
        },
    )
    .await
    .map_err(std::io::Error::other)?;

    let sid_bytes = from_hex(&session_hex)
        .ok_or_else(|| std::io::Error::other("missing/invalid session_id in url path"))?;
    let session = SessionId(sid_bytes);

    let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
    let (mut write, mut read) = ws.split();

    // 每个连接一条对内发送通道，由 writer 任务写回 WebSocket。
    // 通道承载完整 WsMessage：中继负载用 Binary，保活探测用 Ping。
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                eprintln!("[room] conn#{conn_id} writer 写失败，中断");
                break;
            }
        }
    });

    // 连接一建立就注册进房间（不等首条消息），解决会合问题。
    // B5 限流：房间已满（≥max_per_room）则拒绝并中止 writer，防 session_id 爆破/挤爆。
    {
        let mut guard = rooms.lock().unwrap();
        let list = guard.entry(session).or_default();
        if list.len() >= cfg.max_per_room {
            eprintln!(
                "[room] conn#{conn_id} 被拒：房间 {}… 已满（{} 个连接）",
                &session_hex[..8],
                list.len()
            );
            drop(guard);
            writer.abort();
            return Err(std::io::Error::other("room full (rate limited)"));
        }
        list.push((conn_id, tx.clone()));
        eprintln!(
            "[room] conn#{conn_id} 加入房间 {}…（现 {} 个连接）",
            &session_hex[..8],
            list.len()
        );
    }

    // 死连接回收（keepalive）：对端 NAT 映射过期 / iOS 应用被挂起杀死时，TCP 呈半开
    // （服务器侧永远 ESTAB），若不主动探测，尸体连接会永久占着房间名额（上限
    // max_per_room），把后续所有真实 Viewer 挡在门外。策略：每 PING_INTERVAL 发一次
    // WebSocket Ping（活跃客户端协议栈自动回 Pong），超过 REAP_AFTER 没收到任何帧
    // （含 Pong）即判定死亡、跳出循环走清理。注意 Host 空闲等 Viewer 时无业务流量，
    // 因此绝不能按"无业务消息"超时，只能按"连 Pong 都没有"判定。
    let mut last_seen = Instant::now();
    let mut ping_tick = tokio::time::interval(PING_INTERVAL);
    // interval 首次 tick 立即触发：跳过它，避免一连上就发 Ping。
    ping_tick.tick().await;
    loop {
        tokio::select! {
            msg = read.next() => {
                let Some(Ok(msg)) = msg else { break }; // EOF / 协议错误：连接关闭
                last_seen = Instant::now();
                let bytes = match msg {
                    WsMessage::Binary(b) => b,
                    WsMessage::Close(_) => break,
                    _ => continue, // 忽略 ping / pong / text（pong 已刷新 last_seen）
                };
                // 解码 + 强制 F3 限长护栏：超长/非法帧直接丢弃（连接仍可用）。
                if decode_limited(&bytes, MAX_SIGNALING_MESSAGE_LEN).is_err() {
                    eprintln!("[room] conn#{conn_id} 帧非法/超长，丢弃");
                    continue;
                }
                // 转发给同房间其它连接（不含自己）。
                let guard = rooms.lock().unwrap();
                if let Some(list) = guard.get(&session) {
                    let mut delivered = 0usize;
                    for (pid, ptx) in list {
                        if *pid != conn_id {
                            if ptx.send(WsMessage::Binary(bytes.clone())).is_ok() {
                                delivered += 1;
                            } else {
                                eprintln!("[room] 中继 → conn#{pid} 失败（对端 writer 已死）");
                            }
                        }
                    }
                    eprintln!(
                        "[room] conn#{conn_id} → 房间 {}…：{} 字节，中继给 {delivered}/{} 个连接",
                        &session_hex[..8],
                        bytes.len(),
                        list.len().saturating_sub(1)
                    );
                }
            }
            _ = ping_tick.tick() => {
                if last_seen.elapsed() > REAP_AFTER {
                    break; // 对端已死：回收连接，释放房间名额
                }
                if tx.send(WsMessage::Ping(Vec::new().into())).is_err() {
                    break;
                }
            }
        }
    }

    // 清理：从房间移除本连接。
    let mut guard = rooms.lock().unwrap();
    if let Some(list) = guard.get_mut(&session) {
        list.retain(|(pid, _)| *pid != conn_id);
        eprintln!(
            "[room] conn#{conn_id} 离开房间 {}…（剩 {} 个连接）",
            &session_hex[..8],
            list.len()
        );
        if list.is_empty() {
            guard.remove(&session);
        }
    }
    writer.abort();
    Ok(())
}

/// 把 `SessionId` 编成 32 位十六进制串（用于 WebSocket URL 路径）。
pub fn session_hex(session: &SessionId) -> String {
    to_hex(&session.0)
}

/// 十六进制编码（仅用于 session_id 的 URL 路径，避免引入额外依赖）。
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(std::char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(std::char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// 十六进制解码为 16 字节（session_id）。非法输入返回 None。
fn from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = (bytes[2 * i] as char).to_digit(16)?;
        let lo = (bytes[2 * i + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// SHA-256 摘要（per-session 配对 token 比对用，服务端不存明文 token）。
fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out: [u8; 32] = h.finalize().into();
    out
}

/// 从 PEM 文件构建 TLS 接受器（服务器证书链 + 私钥）。供 `serve_listener_with_config`
/// 在 `cfg.tls` 命中时调用。证书/私钥格式为 rustls 接受的 PEM（证书含可选中间链，
/// 私钥支持 PKCS#8 / 传统 PEM）。
pub fn build_tls_acceptor(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> std::io::Result<TlsAcceptor> {
    let cert_bytes = std::fs::read(cert_path)?;
    let key_bytes = std::fs::read(key_path)?;
    let certs: Vec<CertificateDer<'static>> =
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
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("构建 TLS 配置失败: {e}"),
            )
        })?;
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_proto::{
        decode, encode, Capabilities, ConnectionAnswer, ConnectionOffer, FrameMetadata, InputCaps,
        Message, SessionId, VideoCodec,
    };
    use tokio_tungstenite::connect_async;
    // TLS 集成测试依赖（`__rustls-tls` 特性在 dev-dependencies 中开启）：自签证书 + wss 客户端连接器。
    use rcgen::generate_simple_self_signed;
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::Connector;
    // 证书校验器（dev 测试用）：信任任意服务端证书，等价于 Flutter 开发模式 `badCertificateCallback`。
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
    use rustls::DigitallySignedStruct;
    use rustls::SignatureScheme;
    use rustls_pki_types::{ServerName, UnixTime};

    async fn start() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_listener(listener).await;
        });
        addr
    }

    fn caps() -> Capabilities {
        Capabilities {
            video_codecs: vec![VideoCodec::Raw],
            max_width: 1920,
            max_height: 1080,
            fps: 30,
            clipboard: true,
            input: InputCaps {
                mouse: true,
                keyboard: true,
                wheel: true,
            },
        }
    }

    fn frame_meta() -> FrameMetadata {
        FrameMetadata {
            width: 64,
            height: 48,
            fps: 30,
            codec: VideoCodec::Raw,
        }
    }

    #[tokio::test]
    async fn relay_exchanges_offer_answer_ice() {
        let addr = start().await;
        let session = SessionId([3u8; 16]);
        // session_id 通过 URL 路径携带。
        let url = format!("ws://{addr}/{}", session_hex(&session));
        let (mut v, _) = connect_async(&url).await.unwrap();
        let (mut h, _) = connect_async(&url).await.unwrap();

        // Viewer 发 Offer → Host 应收到（两端都已注册进房间）。
        let offer = Message::Offer(ConnectionOffer {
            session_id: session,
            from: [1u8; 16],
            sdp: "offer".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        });
        v.send(WsMessage::Binary(encode(&offer).unwrap()))
            .await
            .unwrap();
        if let WsMessage::Binary(b) = h.next().await.unwrap().unwrap() {
            assert!(matches!(decode(&b).unwrap(), Message::Offer(_)));
        } else {
            panic!("期望收到二进制帧");
        }

        // Host 发 Answer → Viewer 应收到。
        let answer = Message::Answer(ConnectionAnswer {
            session_id: session,
            from: [2u8; 16],
            sdp: "answer".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        });
        h.send(WsMessage::Binary(encode(&answer).unwrap()))
            .await
            .unwrap();
        if let WsMessage::Binary(b) = v.next().await.unwrap().unwrap() {
            assert!(matches!(decode(&b).unwrap(), Message::Answer(_)));
        } else {
            panic!("期望收到二进制帧");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_session_id_at_handshake() {
        let addr = start().await;
        // 长度不对 / 非 hex → 握手应被拒（客户端 connect_async 应返回 Err，而非完成升级）。
        let bad = format!("ws://{addr}/not-a-valid-session-id");
        let res = connect_async(&bad).await;
        assert!(
            res.is_err(),
            "非法 session_id 应在握手期被拒绝（返回 400），而非完成升级后再关闭"
        );
    }

    // ── B5：鉴权 + 限流 ──

    async fn start_with(cfg: SignalingConfig) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_listener_with_config(listener, cfg).await;
        });
        addr
    }

    #[tokio::test]
    async fn auth_rejects_missing_or_wrong_token() {
        let addr = start_with(SignalingConfig::with_token("s3cr3t")).await;
        let session = SessionId([4u8; 16]);
        let hex = session_hex(&session);

        // 无 token → 拒。
        let no_token = format!("ws://{addr}/{hex}");
        assert!(connect_async(&no_token).await.is_err(), "无 token 应被拒");

        // 错 token → 拒。
        let bad = format!("ws://{addr}/{hex}?token=wrong");
        assert!(connect_async(&bad).await.is_err(), "错 token 应被拒");

        // 对 token → 通过。
        let ok = format!("ws://{addr}/{hex}?token=s3cr3t");
        assert!(connect_async(&ok).await.is_ok(), "正确 token 应能通过握手");
    }

    #[tokio::test]
    async fn open_config_allows_without_token() {
        // 向后兼容：open()（无鉴权）时没带 token 也能连（现有测试路径）。
        let addr = start_with(SignalingConfig::open()).await;
        let session = SessionId([5u8; 16]);
        let url = format!("ws://{addr}/{}", session_hex(&session));
        assert!(connect_async(&url).await.is_ok());
    }

    #[tokio::test]
    async fn room_full_is_rate_limited() {
        let cfg = SignalingConfig {
            auth_token: None,
            max_per_room: 2,
            token_store: None,
            tls: None,
        };
        let addr = start_with(cfg).await;
        let session = SessionId([6u8; 16]);
        let url = format!("ws://{addr}/{}", session_hex(&session));

        // 前两个占满房间并保持连接。
        let _c1 = connect_async(&url).await.unwrap().0;
        let _c2 = connect_async(&url).await.unwrap().0;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 第三个：握手可能成功（鉴权/房间检查在升级后），但服务端检测到房间满会立即关闭它。
        // 判定"被限流"的可靠信号是：连接很快走到终结（Close / None / 读错误），而不是能正常收发。
        let (mut c3, _) = connect_async(&url).await.unwrap();
        let mut terminated = false;
        for _ in 0..20 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                futures_util::StreamExt::next(&mut c3),
            )
            .await
            {
                // 收到 Close 帧 / 流结束 / 读错误：连接被终结。
                Ok(Some(Ok(WsMessage::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                    terminated = true;
                    break;
                }
                // 收到普通消息：理论上房间满不该有消息转发给它，继续观察。
                Ok(Some(Ok(_))) => continue,
                // 超时：还没关，继续等（服务端关闭是异步的）。
                Err(_) => continue,
            }
        }
        assert!(terminated, "房间满时第三个连接应被限流关闭");
    }

    // ── B2：per-session 配对 token（§5.1：不焚毁，受控端在线期间可重复扫码）──

    #[tokio::test]
    async fn per_session_token_allows_rescan_while_pairing_active() {
        let store = TokenStore::new();
        let session = SessionId([7u8; 16]);
        // host 配对登记一条 token。
        store.register(session, "deadbeef");
        let addr = start_with(SignalingConfig::per_session(store.clone())).await;
        let hex = session_hex(&session);

        // 首次携带 token → 握手通过。
        let url = format!("ws://{addr}/{hex}?token=deadbeef");
        assert!(
            connect_async(&url).await.is_ok(),
            "首次携带有效 token 应通过握手"
        );

        // 配对不焚毁：断线后重扫同一二维码（同一 token）应可再次建连。
        let rescan = format!("ws://{addr}/{hex}?token=deadbeef");
        assert!(
            connect_async(&rescan).await.is_ok(),
            "配对未取消/未过期前，重扫同一配对码应可重连"
        );
    }

    #[tokio::test]
    async fn per_session_token_rejects_unknown_session() {
        let store = TokenStore::new(); // 空库：未注册任何 token
        let addr = start_with(SignalingConfig::per_session(store)).await;
        let session = SessionId([8u8; 16]);
        let url = format!("ws://{addr}/{}?token=whatever", session_hex(&session));
        assert!(
            connect_async(&url).await.is_err(),
            "未注册的 session（无配对 token）应被拒"
        );
    }

    #[tokio::test]
    async fn per_session_host_without_token_admitted_as_room_owner() {
        // 新模型（A5↔B2）：Host（注册方）不带 token 连接即视为房主，凭"session 已注册"
        // 放行。真正鉴权由 E2E 签名 + 同意层保证。
        let store = TokenStore::new();
        let session = SessionId([9u8; 16]);
        store.register(session, "abc");
        let addr = start_with(SignalingConfig::per_session(store)).await;
        // Host 不带 ?token=，但 session 已注册 → 作为房主放行。
        let url = format!("ws://{addr}/{}", session_hex(&session));
        assert!(
            connect_async(&url).await.is_ok(),
            "已注册 session 的 Host 不带 token 应作为房主放行"
        );
    }

    #[tokio::test]
    async fn per_session_unknown_session_without_token_rejected() {
        // 房主模型边界：未注册的 session（库里无记录）即使不带 token 也应被拒——
        // 否则任何人可猜 session_id 占房。session_id 16 字节随机已难猜，但仍拒空房。
        let store = TokenStore::new(); // 空库
        let addr = start_with(SignalingConfig::per_session(store)).await;
        let session = SessionId([13u8; 16]);
        let url = format!("ws://{addr}/{}", session_hex(&session));
        assert!(
            connect_async(&url).await.is_err(),
            "未注册 session 不带 token 应被拒（防猜 session 占房）"
        );
    }

    // ── L / P0-D 后续：TLS（wss://）加密 + 鉴权联调 ──

    /// 测试用：信任任意服务端证书（等价于 Flutter 开发模式的 `badCertificateCallback`）。
    /// 仅用于测试，不校验主机名/SAN，避免 rcgen 自签证书的 SAN/主机名复杂性。
    #[derive(Debug)]
    struct DevCertVerifier;
    impl ServerCertVerifier for DevCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            // dev 测试用：接受任意签名方案（与 verify_server_cert 的"信任一切"一致）。
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
            ]
        }
    }

    /// 用 rcgen 生成自签证书 + 私钥，写入临时 PEM 文件，返回 (cert_path, key_path)。
    /// 文件名带进程内唯一计数，避免并行测试互相覆盖。
    fn self_signed_temp_pair() -> (std::path::PathBuf, std::path::PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let certified = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir();
        let cert = dir.join(format!("signaling_tls_test_{id}_cert.pem"));
        let key = dir.join(format!("signaling_tls_test_{id}_key.pem"));
        std::fs::write(&cert, certified.cert.pem()).unwrap();
        std::fs::write(&key, certified.key_pair.serialize_pem()).unwrap();
        (cert, key)
    }

    /// 构造一个信任任意证书的 wss 客户端连接器（dev 测试用）。
    fn dev_tls_connector() -> Connector {
        let client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DevCertVerifier))
            .with_no_client_auth();
        Connector::Rustls(Arc::new(client_config))
    }

    #[tokio::test]
    async fn wss_relay_exchanges_over_tls() {
        let (cert, key) = self_signed_temp_pair();
        let cfg = SignalingConfig::with_tls(TlsConfig {
            cert_path: cert,
            key_path: key,
        });
        let addr = start_with(cfg).await;
        let session = SessionId([20u8; 16]);
        let url = format!("wss://127.0.0.1:{}/{}", addr.port(), session_hex(&session));
        // 两端都走 wss（TLS）握手并交换 SDP/ICE。
        let (mut v, _) =
            connect_async_tls_with_config(&url, None, false, Some(dev_tls_connector()))
                .await
                .expect("viewer wss 握手应成功");
        let (mut h, _) =
            connect_async_tls_with_config(&url, None, false, Some(dev_tls_connector()))
                .await
                .expect("host wss 握手应成功");

        let offer = Message::Offer(ConnectionOffer {
            session_id: session,
            from: [1u8; 16],
            sdp: "offer-over-tls".into(),
            capabilities: caps(),
            frame: Some(frame_meta()),
            signature: None,
        });
        v.send(WsMessage::Binary(encode(&offer).unwrap()))
            .await
            .unwrap();
        if let WsMessage::Binary(b) = h.next().await.unwrap().unwrap() {
            assert!(matches!(decode(&b).unwrap(), Message::Offer(_)));
        } else {
            panic!("期望收到二进制帧");
        }
    }

    #[tokio::test]
    async fn wss_with_shared_secret_rejects_bad_token() {
        let (cert, key) = self_signed_temp_pair();
        // TLS + shared-secret 鉴权同时启用。
        let cfg = SignalingConfig {
            auth_token: Some("s3cr3t".into()),
            max_per_room: 8,
            token_store: None,
            tls: Some(TlsConfig {
                cert_path: cert,
                key_path: key,
            }),
        };
        let addr = start_with(cfg).await;
        let session = SessionId([21u8; 16]);
        let hex = session_hex(&session);

        // 错 token → 拒（TLS 握手成功，但 WS 升级被 401 拦下）。
        let bad = format!("wss://127.0.0.1:{}/{hex}?token=wrong", addr.port());
        assert!(
            connect_async_tls_with_config(&bad, None, false, Some(dev_tls_connector()))
                .await
                .is_err(),
            "wss 错 token 应被拒"
        );
        // 对 token → 通过。
        let ok = format!("wss://127.0.0.1:{}/{hex}?token=s3cr3t", addr.port());
        assert!(
            connect_async_tls_with_config(&ok, None, false, Some(dev_tls_connector()))
                .await
                .is_ok(),
            "wss 正确 token 应通过握手"
        );
    }

    #[tokio::test]
    async fn wss_per_session_rejects_wrong_token() {
        let (cert, key) = self_signed_temp_pair();
        let store = TokenStore::new();
        let session = SessionId([22u8; 16]);
        store.register(session, "abc");
        // TLS + per-session 配对 token 同时启用。
        let cfg = SignalingConfig {
            auth_token: None,
            max_per_room: 8,
            token_store: Some(store),
            tls: Some(TlsConfig {
                cert_path: cert,
                key_path: key,
            }),
        };
        let addr = start_with(cfg).await;
        let hex = session_hex(&session);

        // 错 token → 拒（即便 session 已注册，也须出示正确 token 哈希）。
        let wrong = format!("wss://127.0.0.1:{}/{hex}?token=WRONG", addr.port());
        assert!(
            connect_async_tls_with_config(&wrong, None, false, Some(dev_tls_connector()))
                .await
                .is_err(),
            "wss per-session 错 token 应被拒"
        );
        // 对 token → 通过。
        let ok = format!("wss://127.0.0.1:{}/{hex}?token=abc", addr.port());
        assert!(
            connect_async_tls_with_config(&ok, None, false, Some(dev_tls_connector()))
                .await
                .is_ok(),
            "wss per-session 正确 token 应通过握手"
        );
    }

    #[test]
    fn token_store_verify_allows_repeat_and_rejects_wrong() {
        let store = TokenStore::new();
        let session = SessionId([10u8; 16]);
        store.register(session, "tok");
        // 配对不焚毁：同一 token 可重复校验通过（断线重扫重连）。
        assert!(store.verify(&session, "tok"));
        assert!(store.verify(&session, "tok"));
        // 错误 token 失败（哈希不匹配）。
        assert!(!store.verify(&session, "wrong"));
        // 未注册的 session 失败。
        assert!(!store.verify(&SessionId([11u8; 16]), "tok"));
    }

    /// 文件即事实：回灌装载的配对可重复扫码；文件删行 / 删文件即回收（主动取消 /
    /// 刷新二维码 / 受控端退出在下一次握手时生效）。
    #[test]
    fn reload_from_file_is_source_of_truth() {
        let dir =
            std::env::temp_dir().join(format!("signaling_tokendb_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token_db.txt");
        let session = SessionId([42u8; 16]);
        std::fs::write(&path, format!("{}\tdeadbeef\n", session_hex(&session))).unwrap();

        let store = TokenStore::new();
        // 首次扫码：回灌 + 校验通过。
        store.reload_from_file(&path);
        assert!(store.verify(&session, "deadbeef"), "首次扫码应通过握手");
        // 配对不焚毁 + 文件仍发布该配对：断线后重扫同一二维码应可重连。
        store.reload_from_file(&path);
        assert!(store.verify(&session, "deadbeef"), "重扫同一二维码应可重连");
        // Host 刷新配对（文件覆写为新 session/token）：旧配对回收失效，新配对可用。
        let new_session = SessionId([43u8; 16]);
        std::fs::write(&path, format!("{}\tnewtoken\n", session_hex(&new_session))).unwrap();
        store.reload_from_file(&path);
        assert!(
            !store.verify(&session, "deadbeef"),
            "刷新后旧配对（不在文件中）应被回收"
        );
        assert!(store.verify(&new_session, "newtoken"), "新配对应可用");
        // Host 主动取消（删除文件）：配对失效。
        std::fs::remove_file(&path).unwrap();
        store.reload_from_file(&path);
        assert!(
            !store.verify(&new_session, "newtoken"),
            "文件删除（取消配对）后应失效"
        );
        // 内存注册的条目不受文件 reconcile 影响。
        let mem_session = SessionId([44u8; 16]);
        store.register(mem_session, "memtok");
        store.reload_from_file(&path);
        assert!(
            store.verify(&mem_session, "memtok"),
            "内存注册条目不应被文件 reconcile 回收"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
