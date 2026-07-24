//! Track B（韧性面）：连接生命周期管理（重连 + 心跳超时断连）。
//!
//! 本模块只负责"让已建立的连接在网络抖动/断线时可观测、可恢复、能超时判死"，
//! **不碰**媒体面（`host_media`），也**不修改** `Connection` 的定义——仅以组合方式
//! 调用 `Connection` 已暴露的 `pub` 方法（`connection_state`/`wait_connected`/`is_active`/
//! `send_app`/`recv_app`）。物理上与 `host_media.rs` 零冲突（互不 `use` 对方内部符号）。
//!
//! 身份持久化（B4）与重连（B5）：A0 已把 `Connection::new_host/new_viewer` 放宽为
//! `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 并新增 `create_pairing`/`reconnect`；
//! 本模块的 [`identity_store_handle`] 装配持久化身份喂给 `Connection`（B4），
//! [`ConnectionSupervisor::enable_auto_reconnect`] 在连接判死后驱动 `Connection::reconnect`（B5）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch, Notify};

use rdcore_rtc::RTCPeerConnectionState;

use crate::{AppMessage, Connection};
use rdcore_proto::Heartbeat;

/// 连接健康的高层相位（面向 UI / 横幅，比原始 `RTCPeerConnectionState` 更语义化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPhase {
    /// 已建立且对端在发心跳（健康）。
    Up,
    /// 底层曾 Disconnected/Failed，正在等待恢复（重连窗口内）。
    Degraded,
    /// 超过心跳超时仍未恢复（判定死亡，上层应关闭或重回 Signaling）。
    Dead,
}

/// 生命周期监督器配置。
#[derive(Clone)]
pub struct SupervisorConfig {
    /// 心跳发送间隔（本端向对端证明存活）。
    pub heartbeat_interval: Duration,
    /// 心跳超时：距上次收到对端心跳超过该时长即判 `Dead`。
    pub heartbeat_timeout: Duration,
    /// 心跳序号起点。
    pub seq_start: u64,
    /// （预留，待 Track A 放宽 `Connection` 签名后启用）持久化身份目录，来自 B1。
    pub identity_dir: Option<std::path::PathBuf>,
    /// B5：判 `Dead` 后自动重连的最大尝试次数；`0` = 禁用自动重连（默认，保持现有行为）。
    pub max_reconnect_attempts: u32,
    /// B5：两次重连尝试之间的基础退避（第 n 次等待 `reconnect_backoff * n`，线性退避）。
    pub reconnect_backoff: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(2),
            heartbeat_timeout: Duration::from_secs(10),
            seq_start: 0,
            identity_dir: None,
            max_reconnect_attempts: 0,
            reconnect_backoff: Duration::from_secs(2),
        }
    }
}

/// 连接生命周期监督器：组合一个已 `establish` 的 `Connection`，驱动心跳与重连观测。
///
/// 用法：连接建立后 `ConnectionSupervisor::start(conn, cfg)` 返回句柄；
/// `phase()` 查询当前健康相位；`stop()` 停止后台任务。
pub struct ConnectionSupervisor {
    phase_tx: watch::Sender<LinkPhase>,
    stop: Arc<AtomicBool>,
    /// 上次收到对端心跳的毫秒时间戳（Unix epoch-ms）。
    last_hb_ms: Arc<AtomicU64>,
    notify: Arc<Notify>,
    /// 非心跳的业务消息（Input/Clipboard/…）经 broadcast 转发：supervisor 的 `recv_business`
    /// 与 `Connection::recv_input` 各持一个订阅者，互不争抢（契约 §9 闭环）。
    business_rx: tokio::sync::Mutex<broadcast::Receiver<AppMessage>>,
    /// B5：被监督的连接（`Arc<Connection>`，与 `Connection::reconnect(&self)` 的 `&self` 匹配）。
    /// 重连循环用它原地重建 PeerConnection + 重跑握手。
    conn: Arc<Connection>,
}

impl ConnectionSupervisor {
    /// 当前 Unix epoch 毫秒（心跳时间戳用）。
    ///
    /// 用 Unix epoch 而非"进程启动时钟"：心跳 `timestamp_ms` 会发给对端，
    /// 两端进程启动时间不同、无法互读；Unix epoch 是双方共有的时间原点。
    /// 本端的 `last_hb_ms` 也用它，保证"距今多久"判断同原点、自洽。
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// 在已建立的连接上启动监督（心跳发送 + 接收监听 + 相位推断）。
    ///
    /// `conn` 必须已完成 `establish`（E2E 密钥就绪），否则 `send_app` 会报 `NoSessionKey`。
    ///
    /// 契约 §9 闭环：本方法把业务消息经 `broadcast` 通道一式两份——`recv_business` 持一个
    /// 订阅者，另一个经 [`Connection::set_business_receiver`] 注入给 `Connection::recv_input`。
    /// 这样 supervisor 独占 `recv_app`（心跳就地处理），Track A 的输入接收走 broadcast，
    /// 两侧物理零冲突。
    pub async fn start(conn: Arc<Connection>, cfg: SupervisorConfig) -> Self {
        let (phase_tx, _) = watch::channel(LinkPhase::Up);
        let stop = Arc::new(AtomicBool::new(false));
        let last_hb_ms = Arc::new(AtomicU64::new(Self::now_ms()));
        let notify = Arc::new(Notify::new());
        // 业务消息 broadcast 通道：supervisor（recv_business）与 Connection（recv_input）各持订阅者。
        let (biz_tx, biz_rx) = broadcast::channel::<AppMessage>(64);
        // 注入给 Connection::recv_input（契约 §9 闭环的那一行）。
        conn.set_business_receiver(biz_tx.subscribe()).await;

        // ── 心跳发送循环：按间隔向对端发 Heartbeat（经 E2E 加密控制通道）。──
        {
            let conn = conn.clone();
            let stop = stop.clone();
            let mut seq = cfg.seq_start;
            let interval = cfg.heartbeat_interval;
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    let hb = AppMessage::Heartbeat(Heartbeat {
                        seq,
                        timestamp_ms: Self::now_ms(),
                    });
                    // 发送失败（如对端已走）不致命，下一轮再试；相位由监听环判定。
                    let _ = conn.send_app(&hb).await;
                    seq = seq.wrapping_add(1);
                    tokio::time::sleep(interval).await;
                }
            });
        }

        // ── 接收监听环（独占 recv_app）：心跳刷新时间戳，业务消息经 broadcast 一式多投。──
        // 关键：supervisor 是 `recv_app` 的唯一消费者。若上层也直接调 `recv_app`，
        // 两者会争抢同一条 DataChannel（心跳环吃掉业务消息 / 上层吃掉心跳）。
        // 因此上层必须经 [`Self::recv_business`] 或 [`Connection::recv_input`] 取业务消息，
        // 不得再调 `conn.recv_app()`。
        {
            let conn = conn.clone();
            let stop = stop.clone();
            let last_hb_ms = last_hb_ms.clone();
            let notify = notify.clone();
            let biz_tx = biz_tx.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    match tokio::time::timeout(Duration::from_millis(500), conn.recv_app()).await {
                        Ok(Ok(Some(AppMessage::Heartbeat(_)))) => {
                            last_hb_ms.store(Self::now_ms(), Ordering::SeqCst);
                            notify.notify_one();
                        }
                        Ok(Ok(Some(AppMessage::RequestKeyframe))) => {
                            // Viewer 请求关键帧（丢帧/花屏/积压恢复）：就地置位，
                            // 不等输入环消费——输入未授权 / 输入环未启动时画面也能自愈。
                            conn.note_video_keyframe_requested();
                        }
                        Ok(Ok(Some(other))) => {
                            // 非心跳：broadcast 给所有订阅者（无订阅者时 send 返回 Err，忽略）。
                            let _ = biz_tx.send(other);
                        }
                        Ok(Ok(None)) => break, // 通道关闭
                        Ok(Err(_)) => { /* 解密/协议错误：忽略，继续 */ }
                        Err(_) => { /* 超时：周期性唤醒以便判 Dead */ }
                    }
                }
            });
        }

        // ── 相位推断环：综合底层 PeerConnection 状态 + 心跳新鲜度。──
        {
            let conn = conn.clone();
            let phase_tx = phase_tx.clone();
            let stop = stop.clone();
            let last_hb_ms = last_hb_ms.clone();
            let timeout = cfg.heartbeat_timeout;
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    let pc = conn.connection_state();
                    let since_hb = Self::now_ms().saturating_sub(last_hb_ms.load(Ordering::SeqCst));
                    let phase = if since_hb > timeout.as_millis() as u64 {
                        LinkPhase::Dead
                    } else {
                        match pc {
                            RTCPeerConnectionState::Connected => LinkPhase::Up,
                            RTCPeerConnectionState::Disconnected
                            | RTCPeerConnectionState::Failed
                            | RTCPeerConnectionState::Connecting
                            | RTCPeerConnectionState::New => LinkPhase::Degraded,
                            RTCPeerConnectionState::Closed => LinkPhase::Dead,
                            _ => LinkPhase::Degraded,
                        }
                    };
                    let _ = phase_tx.send(phase);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        }

        // ── B5 自动重连环：判 Dead 后按线性退避重试 `Connection::reconnect`，
        //    重建 PeerConnection + 重跑握手（复用持久化身份 + 上次授权），成功后相位回 Up。──
        if cfg.max_reconnect_attempts > 0 {
            let conn = conn.clone();
            let stop = stop.clone();
            let phase_tx = phase_tx.clone();
            let max_attempts = cfg.max_reconnect_attempts;
            let backoff = cfg.reconnect_backoff;
            let mut phase_rx = phase_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    // 等到判 Dead（或 stop）。
                    loop {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        if *phase_rx.borrow() == LinkPhase::Dead {
                            break;
                        }
                        if phase_rx.changed().await.is_err() {
                            return;
                        }
                    }
                    // 连续重试，直到成功（相位回 Up）或耗尽次数。
                    let mut attempt = 0u32;
                    while attempt < max_attempts && !stop.load(Ordering::SeqCst) {
                        attempt += 1;
                        // 线性退避：第 n 次先等 backoff * n，给网络恢复留时间。
                        tokio::time::sleep(backoff * attempt).await;
                        match conn.reconnect().await {
                            Ok(()) => break,    // 重连成功：心跳环会刷新相位回 Up，退出重试回外层监听。
                            Err(_) => continue, // 本次失败，退避后重试。
                        }
                    }
                    // 无论成功或耗尽，都回到外层继续监听下一次 Dead（耗尽则维持 Dead，等上层处置）。
                }
            });
        }

        Self {
            phase_tx,
            stop,
            last_hb_ms,
            notify,
            business_rx: tokio::sync::Mutex::new(biz_rx),
            conn,
        }
    }

    /// 取一条非心跳的业务消息（Input/Clipboard/…）。通道关闭返回 `None`。
    ///
    /// supervisor 已独占 `conn.recv_app()`，心跳就地处理、业务消息经 broadcast 一式多投。
    /// 上层**不得**再直接调 `conn.recv_app()`（会争抢通道）；本方法与
    /// [`Connection::recv_input`] 各持 broadcast 的一个订阅者，互不影响。
    /// 缓冲溢出（`Lagged`）时跳过滞留消息，取最新一条。
    pub async fn recv_business(&self) -> Option<AppMessage> {
        let mut rx = self.business_rx.lock().await;
        loop {
            match rx.recv().await {
                Ok(m) => return Some(m),
                Err(broadcast::error::RecvError::Lagged(_)) => continue, // 溢出：跳过旧消息
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// 订阅当前健康相位（watch 通道，UI/横幅可持续观察）。
    pub fn subscribe(&self) -> watch::Receiver<LinkPhase> {
        self.phase_tx.subscribe()
    }

    /// 当前相位快照。
    pub fn phase(&self) -> LinkPhase {
        *self.phase_tx.borrow()
    }

    /// 等待直到进入 `Dead`（或 `stop` 被调用）。用于上层在判死时回收/重回 Signaling。
    pub async fn wait_dead(&self) {
        let mut rx = self.phase_tx.subscribe();
        while *rx.borrow() != LinkPhase::Dead {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    /// 上次收到对端心跳距今的毫秒数（供调试/横幅显示"已多久无响应"）。
    pub fn since_last_heartbeat_ms(&self) -> u64 {
        Self::now_ms().saturating_sub(self.last_hb_ms.load(Ordering::SeqCst))
    }

    /// 停止所有后台任务。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// B5：被监督的连接句柄（供上层在重连后继续收发媒体/输入）。
    pub fn connection(&self) -> Arc<Connection> {
        self.conn.clone()
    }
}

impl Drop for ConnectionSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 相位推断的纯逻辑（与 tokio/连接解耦，便于单测）。
///
/// - 心跳超时 → `Dead`（无论底层状态如何，沉默即死）。
/// - 底层 `Closed` → `Dead`。
/// - 心跳新鲜 + 底层 `Connected` → `Up`。
/// - 其余（Disconnected/Failed/Connecting/New 但心跳还没超时）→ `Degraded`（重连窗口）。
pub fn classify(
    pc: RTCPeerConnectionState,
    since_last_heartbeat: Duration,
    heartbeat_timeout: Duration,
) -> LinkPhase {
    if since_last_heartbeat > heartbeat_timeout {
        return LinkPhase::Dead;
    }
    match pc {
        RTCPeerConnectionState::Closed => LinkPhase::Dead,
        RTCPeerConnectionState::Connected => LinkPhase::Up,
        _ => LinkPhase::Degraded,
    }
}

/// B5：第 `attempt` 次（从 1 起）重连前的线性退避时长。纯逻辑，便于单测。
pub fn reconnect_backoff_for(attempt: u32, base: Duration) -> Duration {
    base * attempt.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_timeout_is_dead_even_if_connected() {
        // 关键回归：心跳沉默超过阈值，即使底层还显示 Connected，也必须判 Dead（防僵尸会话）。
        let p = classify(
            RTCPeerConnectionState::Connected,
            Duration::from_secs(11),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Dead);
    }

    #[test]
    fn connected_with_fresh_heartbeat_is_up() {
        let p = classify(
            RTCPeerConnectionState::Connected,
            Duration::from_secs(1),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Up);
    }

    #[test]
    fn disconnected_within_window_is_degraded() {
        let p = classify(
            RTCPeerConnectionState::Disconnected,
            Duration::from_secs(2),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Degraded);
    }

    #[test]
    fn failed_within_window_is_degraded() {
        let p = classify(
            RTCPeerConnectionState::Failed,
            Duration::from_secs(2),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Degraded);
    }

    #[test]
    fn closed_is_dead_regardless_of_heartbeat() {
        let p = classify(
            RTCPeerConnectionState::Closed,
            Duration::from_secs(0),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Dead);
    }

    #[test]
    fn boundary_exact_timeout_not_dead() {
        // since == timeout 不算超（严格大于才算），避免边界抖动。
        let p = classify(
            RTCPeerConnectionState::Connected,
            Duration::from_secs(10),
            Duration::from_secs(10),
        );
        assert_eq!(p, LinkPhase::Up);
    }

    #[test]
    fn reconnect_backoff_is_linear() {
        let base = Duration::from_secs(2);
        assert_eq!(reconnect_backoff_for(1, base), Duration::from_secs(2));
        assert_eq!(reconnect_backoff_for(3, base), Duration::from_secs(6));
        // attempt=0 兜底为 1（不除零、不零等待）。
        assert_eq!(reconnect_backoff_for(0, base), Duration::from_secs(2));
    }

    #[test]
    fn auto_reconnect_disabled_by_default() {
        // 默认 max_reconnect_attempts=0：不启用自动重连，保持现有行为（向后兼容）。
        let cfg = SupervisorConfig::default();
        assert_eq!(cfg.max_reconnect_attempts, 0);
    }
}

// ───────────────────────── B4（韧性面，kimi-k3）：身份持久化接入 ─────────────────────────
//
// 目标（计划 §4 B4）：配对重启不丢 TOFU 指纹；`Connection::new_host/new_viewer` 接
// `PersistentIdentityStore`。基础设施 `rdcore-identity::persist` 已就绪且测试绿，本模块只做
// 「装配」——把持久化身份 + 私钥喂给 `Connection`。
//
// 集成状态（A0 已落地）：`Connection::new_host/new_viewer` 现接
// `store: Arc<Mutex<dyn IdentityStore + Send + Sync>>`（见 `lib.rs` A0 注释）。本模块的
// [`identity_store_handle`] 把 [`PersistentIdentity`] 装配成该形态，直接传给构造函数即可。

use std::path::PathBuf;

use rdcore_crypto::{Ed25519CryptoProvider, SecretKey};
use rdcore_identity::{
    IdentityStore, PassphraseKeyProvider, PeerIdentity, PersistentIdentityStore,
};

/// B4 装配产物：一个已加载/新建的持久化身份存储 + 本机私钥。
///
/// `store` 实现 [`IdentityStore`]（TOFU：重启后已记住对端不丢）；`secret` 用于
/// `Connection` 对 Offer/Answer/临时密钥签名。经 [`identity_store_handle`] 包成
/// `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 后喂给 `Connection::new_host/new_viewer`。
pub struct PersistentIdentity {
    /// 持久化身份存储（本机身份 + 已记住对端表）。
    pub store: PersistentIdentityStore,
    /// 本机私钥（仅进程内持有，不落盘明文；`PersistentIdentityStore` 存的是加密形式）。
    pub secret: SecretKey,
    /// 身份目录（`identity.json` + `secret.enc` 所在处），供调试/日志。
    pub dir: PathBuf,
}

impl std::fmt::Debug for PersistentIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentIdentity")
            .field("device_id", &self.store.local_identity().id)
            .field("fingerprint", &self.store.local_identity().fingerprint)
            .field("dir", &self.dir)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// B4 接入配置：从哪个目录加载身份、用什么口令保护私钥。
#[derive(Clone)]
pub struct IdentityPersistenceConfig {
    /// 身份目录（`identity.json` / `secret.enc`）。通常 `<app_data>/rdcore/identity`。
    pub dir: PathBuf,
    /// 保护私钥的口令（生产应来自 OS 钥匙串/用户设置；此处为可注入来源）。
    pub passphrase: String,
    /// 本机展示名（首次创建身份时用）。
    pub display_name: String,
}

/// 加载或新建持久化身份（B4 核心入口）。
///
/// - 目录已有身份 → 加载（重启路径）：已记住对端指纹恢复，TOFU 不重新告警。
/// - 目录无身份 → 新建并落盘（首装路径）：生成密钥对 + 随机 DeviceId，私钥加密落盘。
///
/// 返回 [`PersistentIdentity`]；用 [`identity_store_handle`] 包成 trait object 后传给 `Connection`。
pub fn load_or_create_persistent_identity(
    cfg: &IdentityPersistenceConfig,
) -> Result<PersistentIdentity, rdcore_identity::PersistError> {
    let provider = Ed25519CryptoProvider;
    let keys = PassphraseKeyProvider::new(cfg.passphrase.clone());
    let (store, secret) =
        PersistentIdentityStore::load_or_create(&cfg.dir, &provider, &cfg.display_name, &keys)?;
    Ok(PersistentIdentity {
        store,
        secret,
        dir: cfg.dir.clone(),
    })
}

/// 把持久化身份装配为 `Connection::new_host/new_viewer` 要的形态（A0 接口）。
///
/// 用法：
/// ```ignore
/// let id = load_or_create_persistent_identity(&cfg)?;
/// let conn = Connection::new_host(url, session, id.secret.clone(),
///             identity_store_handle(id), rtc_cfg, timeout).await?;
/// ```
pub fn identity_store_handle(
    identity: PersistentIdentity,
) -> std::sync::Arc<std::sync::Mutex<dyn IdentityStore + Send + Sync>> {
    std::sync::Arc::new(std::sync::Mutex::new(identity.store))
}

/// 记住一个已配对对端并立即落盘（TOFU 关键承诺：配对不丢）。
///
/// 用 `try_remember`（返回 `Result`）而非 trait 的 `remember`（静默到 stderr）——
/// 写盘失败必须让上层知晓，否则用户以为配对成功、重启后却丢设备。
pub fn remember_peer_persistent(
    identity: &mut PersistentIdentity,
    peer: PeerIdentity,
) -> Result<(), rdcore_identity::PersistError> {
    identity.store.try_remember(peer)
}

/// 默认身份目录（跨平台）：`<系统配置目录>/rdcore/identity`。
///
/// Windows: `%APPDATA%\rdcore\identity`；macOS: `~/Library/Application Support/rdcore/identity`；
/// Linux: `~/.config/rdcore/identity`。取不到时回退到当前目录下的 `rdcore/identity`。
pub fn default_identity_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("HOME").map(|h| {
            let h = PathBuf::from(h);
            #[cfg(target_os = "macos")]
            {
                h.join("Library").join("Application Support")
            }
            #[cfg(not(target_os = "macos"))]
            {
                h.join(".config")
            }
        })
    });
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("rdcore")
        .join("identity")
}

#[cfg(test)]
mod b4_tests {
    use super::*;
    use std::path::Path;

    fn cfg_in(dir: &Path) -> IdentityPersistenceConfig {
        IdentityPersistenceConfig {
            dir: dir.to_path_buf(),
            passphrase: "test-pass".into(),
            display_name: "b4-test-device".into(),
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rdcore_b4_{}_{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        d
    }

    #[test]
    fn first_run_creates_then_reload_keeps_identity() {
        let dir = tmp_dir("create");
        let cfg = cfg_in(&dir);
        // 首装：创建。
        let first = load_or_create_persistent_identity(&cfg).expect("首装创建");
        let fp_first = first.store.local_identity().fingerprint.clone();
        let id_first = first.store.local_identity().id;
        drop(first);
        // 重启：重新加载，指纹/设备 ID 必须一致（TOFU 不重新告警）。
        let second = load_or_create_persistent_identity(&cfg).expect("重启加载");
        assert_eq!(second.store.local_identity().fingerprint, fp_first);
        assert_eq!(second.store.local_identity().id, id_first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remembered_peer_survives_reload() {
        let dir = tmp_dir("peer");
        let cfg = cfg_in(&dir);
        let mut id = load_or_create_persistent_identity(&cfg).expect("创建");
        // 模拟配对：记住一个对端。
        let (peer, _sk) =
            rdcore_identity::create_local_identity(&Ed25519CryptoProvider, "peer-phone");
        let peer_fp = peer.fingerprint.clone();
        remember_peer_persistent(&mut id, peer.clone()).expect("记住对端落盘");
        let peer_id = peer.id;
        drop(id);
        // 重启后该对端仍在（TOFU 配对不丢）。
        let id2 = load_or_create_persistent_identity(&cfg).expect("重启加载");
        let found = id2
            .store
            .lookup(&peer_id)
            .expect("重启后应仍能查到已配对对端");
        assert_eq!(found.fingerprint, peer_fp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_passphrase_fails_decrypt() {
        let dir = tmp_dir("pass");
        let cfg = cfg_in(&dir);
        let _ = load_or_create_persistent_identity(&cfg).expect("创建");
        // 错误口令重新加载 → 解密私钥失败（绝不静默成功）。
        let bad = IdentityPersistenceConfig {
            passphrase: "wrong".into(),
            ..cfg_in(&dir)
        };
        assert!(load_or_create_persistent_identity(&bad).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_dir_is_under_config_root() {
        let d = default_identity_dir();
        assert!(d.ends_with("rdcore/identity") || d.ends_with(r"rdcore\identity"));
    }

    #[test]
    fn identity_store_handle_yields_a0_trait_object() {
        // B4↔A0 集成：装配产物必须能包成 `Arc<Mutex<dyn IdentityStore + Send + Sync>>`。
        let dir = tmp_dir("handle");
        let cfg = cfg_in(&dir);
        let id = load_or_create_persistent_identity(&cfg).expect("创建");
        let expected_fp = id.store.local_identity().fingerprint.clone();
        let handle = identity_store_handle(id);
        // 经 trait object 锁内访问 local_identity，验证 Send+Sync 装配成立。
        let fp = handle.lock().unwrap().local_identity().fingerprint.clone();
        assert_eq!(fp, expected_fp);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
