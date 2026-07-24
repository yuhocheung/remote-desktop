//! rdcore-app — 把分散的库（身份 / 信令 / WebRTC / 会话密钥 / 同意门控 / 媒体）
//! 编排成一条**真正可运行的端到端连接**。
//!
//! 这是 P0–P7 全部库之上的一层「胶水」：让身份、Ed25519 验签、真实 WebRTC、同意门控、
//! X25519 端到端加密从孤立的单元测变成一条连贯的连接生命周期。它与 Flutter 的
//! `ConnectionController` 在结构上对称：一侧负责「发起 / 接受」，另一侧负责「同意 / 授权」。
//!
//! # 安全边界（同架构文档 §1/§5）
//! - 信令 WebSocket **只传 SDP/ICE**（Offer/Answer/Ice）；媒体帧、输入、剪贴板、心跳
//!   永不经云端控制面。
//! - 握手期用 Ed25519 验签确认对端身份（防冒充 / MITM）；同意门控由已认证身份驱动，
//!   因此不可伪造横幅的数据源一定来自密码学确认的对端。
//! - 两条 negotiated 数据通道 open 后，双方用 X25519 临时密钥（由 Ed25519 签名绑定到
//!   长期身份）协商端到端会话密钥；此后**媒体像素与控制消息全部用该密钥 AEAD 加密**
//!   （即使 WebRTC 传输本身被监听也无明文）。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use rdcore_capture::CaptureSource;
use rdcore_consent::{ConsentDecision, ConsentGate, ConsentMode, ConsentScope, SecurityIndicator};
use rdcore_crypto::{
    aead_open, aead_seal, ephemeral_x25519_keypair, x25519_public_bytes, Ed25519CryptoProvider,
    SecretKey, SessionKey,
};
use rdcore_decode::{self, DecodedFrame, Decoder};
use rdcore_identity::{IdentityStore, PeerIdentity};
use rdcore_media::{AudioChannel, DataChannel, MediaChannel};
use rdcore_proto::{
    AudioCodec, AudioFrame, Capabilities, Ciphertext, ClipboardEvent, ConnectionAnswer,
    ConnectionOffer, Heartbeat, IceCandidate, InputCaps, InputEvent, MediaFrame, Message,
    SessionId, VideoCodec,
};
use rdcore_render::{self, RenderedFrame};
use rdcore_rtc::{
    sdp_has_active_video, RTCIceCandidateInit, RTCPeerConnectionState, RtcConfig, VideoReceiver,
    WebRtcPeer,
};
use rdcore_session::{
    establish_session_key, sign_answer, sign_ephemeral_key, sign_offer, verify_answer,
    verify_offer, HandshakeError, VerifiedPeer,
};
use rdcore_signaling::SignalingClient;
use serde::{Deserialize, Serialize};

/// Track A 媒体面：Host 抓取→媒体通道发送循环（与 Track B 的 connection_lifecycle 互不依赖）。
pub mod host_media;
pub use host_media::HostMediaPump;
/// Track A 音频面：Host 采集→音频通道发送循环（与视频平行、互不阻塞）。
pub mod host_audio;
pub use host_audio::HostAudioPump;
// Track B（韧性面，kimi-k3 负责）子模块；与本模块物理隔离、互不依赖。
pub mod connection_lifecycle;
pub mod file_transfer;

/// 应用层控制消息：E2E 通道建立后，控制通道上传输的一切都封装为此枚举（再经 AEAD 加密）。
///
/// 注意：心跳 / 输入 / 剪贴板原本是 `rdcore_proto::Message` 的变体；此处统一收口到应用层
/// 协议，整条控制通道因此「先加密、再传输」，云端即使拿到 WebRTC 链路也只见密文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AppMessage {
    /// 心跳存活探针。
    Heartbeat(Heartbeat),
    /// 远程输入事件（鼠标 / 键盘 / 滚轮）。
    Input(InputEvent),
    /// 剪贴板同步事件。
    Clipboard(ClipboardEvent),
    /// Host 给 Viewer 的授权决定（Grant 含范围 / Deny 含原因）。
    Consent(ConsentDecision),
    /// Host 撤销连接。
    Revoke,
    /// Viewer 请求 Host 下一帧输出关键帧（IDR）：P 帧流下解码端丢帧/花屏/积压后
    /// 的快速恢复手段（对应标准 PLI/FIR 语义）。只能追加在枚举末尾——postcard 按
    /// 变体下标编码，严禁在中间插入或重排。
    RequestKeyframe,
}

/// 一次配对的邀请信息：Host 经 [`Connection::create_pairing`] 生成，通过带外（二维码 / 输码）
/// 交给 Viewer。字段格式已钉死（见 `docs/plan_iphone_windows_public.md` §5 协调点1）：
///
/// - `session_id`：16 字节二进制（`SessionId([u8;16])`），hex 展示串 = 32 字符 `[0-9a-f]`。
/// - `token`：一次性配对码，32 字节随机 → hex 展示 64 字符 `[0-9a-f]`；仅用于信令注册鉴权 +
///   QR/输码展示，**不进 `Message` 线协议**；建连即失效、首次成功校验即焚（外加 15 分钟 TTL）。
///
/// 重连（B5）不依赖此 token：重连复用持久化身份（TOFU）+ session_id 重新签名鉴权。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingInfo {
    /// 16 字节会话 ID。
    pub session_id: SessionId,
    /// 一次性配对码（hex 展示 64 字符）。
    pub token: String,
}

/// 编排层错误：把各底层 crate 的错误统一收口，便于上层（FFI / UI）一处处理。
#[derive(Debug)]
pub enum AppError {
    /// 信令通道错误。
    Signaling(rdcore_signaling::SignalingError),
    /// WebRTC PeerConnection 错误。
    Rtc(rdcore_rtc::RtcError),
    /// 媒体通道错误。
    Media(rdcore_media::MediaChannelError),
    /// 音频通道错误。
    Audio(rdcore_media::AudioChannelError),
    /// 数据通道错误。
    Data(rdcore_media::DataChannelError),
    /// 握手验签错误（防冒充 / MITM 的拒绝信号）。
    Handshake(HandshakeError),
    /// 协议层（编解码 / 限长）。
    Protocol(rdcore_proto::ProtocolError),
    /// JSON 序列化（ICE 候选穿梭信令用）。
    Json(serde_json::Error),
    /// 端到端会话密钥尚未建立就尝试收发加密数据。
    NoSessionKey,
    /// 密码学 / 逻辑层面的其它失败。
    Crypto(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Signaling(e) => write!(f, "signaling: {e}"),
            AppError::Rtc(e) => write!(f, "rtc: {e}"),
            AppError::Media(e) => write!(f, "media: {e}"),
            AppError::Audio(e) => write!(f, "audio: {e}"),
            AppError::Data(e) => write!(f, "data: {e}"),
            AppError::Handshake(e) => write!(f, "handshake: {e}"),
            AppError::Protocol(e) => write!(f, "protocol: {e}"),
            AppError::Json(e) => write!(f, "json: {e}"),
            AppError::NoSessionKey => write!(f, "会话密钥尚未建立"),
            AppError::Crypto(s) => write!(f, "crypto: {s}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<rdcore_signaling::SignalingError> for AppError {
    fn from(e: rdcore_signaling::SignalingError) -> Self {
        AppError::Signaling(e)
    }
}
impl From<rdcore_rtc::RtcError> for AppError {
    fn from(e: rdcore_rtc::RtcError) -> Self {
        AppError::Rtc(e)
    }
}
impl From<rdcore_media::MediaChannelError> for AppError {
    fn from(e: rdcore_media::MediaChannelError) -> Self {
        AppError::Media(e)
    }
}
impl From<rdcore_media::AudioChannelError> for AppError {
    fn from(e: rdcore_media::AudioChannelError) -> Self {
        AppError::Audio(e)
    }
}
impl From<rdcore_media::DataChannelError> for AppError {
    fn from(e: rdcore_media::DataChannelError) -> Self {
        AppError::Data(e)
    }
}
impl From<HandshakeError> for AppError {
    fn from(e: HandshakeError) -> Self {
        AppError::Handshake(e)
    }
}
impl From<rdcore_proto::ProtocolError> for AppError {
    fn from(e: rdcore_proto::ProtocolError) -> Self {
        AppError::Protocol(e)
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::Json(e)
    }
}

/// 一条端到端连接（Host 或被控端 / Viewer 或控制端）。
///
/// 构造后调用 [`Connection::establish`] 跑完握手 + 验签 + ICE + E2E 密钥 + 同意，
/// 之后用 [`Connection::send_media`] / [`Connection::recv_media`] / [`Connection::send_app`]
/// / [`Connection::recv_app`] 收发**已加密**的媒体与控制流量。
pub struct Connection {
    is_host: bool,
    provider: Ed25519CryptoProvider,
    self_secret: SecretKey,
    /// 身份存储（A0：由 `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 放宽，可注入
    /// `PersistentIdentityStore` 等持久化实现；B4 依赖此）。`remember(&mut self)` 需内部
    /// 可变性，故用 `Mutex` 包裹 trait object。
    store: Arc<Mutex<dyn IdentityStore + Send + Sync>>,
    session_id: SessionId,
    /// WebRTC PeerConnection（A0：内部可变性，支持 `reconnect` 原地重建）。
    peer: Mutex<Arc<WebRtcPeer>>,
    /// 保留构造时的 RTC 配置，供 `reconnect` 重建 PeerConnection（A0）。
    rtc_cfg: RtcConfig,
    sig: Arc<SignalingClient>,
    consent_mode: ConsentMode,
    heartbeat_timeout: Duration,
    /// Host 侧：P4 验签得到的对端身份 + 同意状态机。
    consent: Mutex<Option<ConsentGate>>,
    /// 两端：E2E 会话密钥（通道 open 且密钥交换完成后才有）。
    session_key: Mutex<Option<SessionKey>>,
    /// 两端：P4 验签得到的对端身份（供不可伪造横幅展示）。
    peer_verified: Mutex<Option<VerifiedPeer>>,
    /// Viewer 侧：从 Host 收到的授权决定（用于反映本端状态）。
    remote_decision: Mutex<Option<ConsentDecision>>,
    /// 重连时复用的上一次 Host 授权决定（A0：让 `reconnect` 自包含）。
    last_host_decision: Mutex<Option<ConsentDecision>>,
    /// Track B 集成点（契约 §9）：挂上 `ConnectionSupervisor` 后，由 supervisor 把业务转发
    /// 通道（`broadcast::Receiver<AppMessage>`）注入此处；`recv_input` 据此改走业务通道，
    /// 不再与 supervisor 争抢 `recv_app`。无 supervisor 时为 `None`，`recv_input` 回退到
    /// 直接收 E2E 加密控制通道（孤立场景 / 单元测试）。
    business_rx: tokio::sync::Mutex<Option<tokio::sync::broadcast::Receiver<AppMessage>>>,
    /// 协商的视频编解码器（Host 决定，Viewer 须匹配；默认 `Raw` 直通）。A1：驱动 Host 编码器
    /// 与 Viewer 解码器的选择。
    video_codec: Mutex<VideoCodec>,
    /// Viewer 侧持久解码器（跨帧复用以保留 SPS/PPS 状态；codec 变更时重建）。A1。
    media_decoder: Mutex<Option<Box<dyn Decoder + Send + Sync>>>,
    /// 协商的音频编解码器（Host 决定，Viewer 须匹配；默认 `Raw` 直通）。C 音频管线。
    audio_codec: Mutex<AudioCodec>,
    /// Viewer 侧持久音频解码器（跨帧复用以保留 Opus 状态；codec 变更时重建）。C 音频管线。
    audio_decoder: Mutex<Option<Box<dyn rdcore_audio::AudioDecoder + Send + Sync>>>,
    /// 会话存续期间预读的信令消息缓冲（FIFO）。Host 在「等 Viewer 掉线 / 等重扫」阶段
    /// 经 [`Connection::wait_peer_gone_or_rescan`] 预取的消息存这里，随后的
    /// establish/reconnect 的 `recv_offer`/`recv_answer` 优先消费，保证重扫消息不丢。
    pending_sig: Mutex<std::collections::VecDeque<Message>>,
    /// 本端是否优先使用 RTP 视频轨道（构造时由 `RDCORE_VIDEO_TRANSPORT` 决定；
    /// `dc` / `datachannel` 强制 DataChannel，用于排障与新旧对照实验）。仅构造期赋值。
    prefer_video_rtp: bool,
    /// 本会话是否已协商启用 RTP 视频轨道（establish 里按 Offer/Answer 的视频 m-line
    /// 判定；Viewer 首帧兜底回退时摘回 false）。Host 据此路由 `send_media`。
    video_rtp: Mutex<bool>,
    /// Viewer 侧 RTP 收帧句柄（establish 协商成功后挂上；首帧兜底回退时摘除）。
    video_rx: Mutex<Option<VideoReceiver>>,
    /// Viewer 侧 RTP 首帧兜底状态（旧 Host「假激活」检测）；未启用 RTP 时为 None。
    rtp_recv: Mutex<Option<RtpRecvState>>,
    /// Host 侧：「下一帧必须编码为 IDR」的一次性请求标志（Arc 共享给媒体泵线程）。
    /// 置位来源：`AppMessage::RequestKeyframe`（Viewer 丢帧/花屏/积压恢复）、
    /// DC 发送缓冲丢帧（自愈）、泵内背压丢帧（自愈）。泵在每帧编码前消费，
    /// 有编码器即调 `Encoder::request_keyframe`。Viewer 侧此标志无意义（恒 false）。
    video_keyframe_requested: Arc<AtomicBool>,
}

/// Viewer 侧 RTP 收帧兜底状态（旧 Host 检测用）。
///
/// 场景：新 Viewer 的 Offer 声明了视频 m-line，但旧版本 Host 的协议栈可能回出
/// 「m-line 看似激活、实际永不发帧」的 Answer（未启用轨道）。此时 Viewer 若死等
/// RTP 首帧将永远黑屏——超过 [`RTP_FIRST_FRAME_TIMEOUT`] 一帧未收即回退 DataChannel。
#[derive(Debug, Clone, Copy)]
struct RtpRecvState {
    /// 是否已收到首帧（收到即不再回退：RTP 链路已被证明可用）。
    got_first_frame: bool,
    /// 首帧兜底截止（tokio 时钟，配合 `timeout_at` 使用）。
    first_frame_deadline: tokio::time::Instant,
}

/// Viewer 侧 RTP 首帧兜底时限（见 [`RtpRecvState`]）。
const RTP_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// DC 视频路径因发送缓冲积压而累计丢弃的帧数（诊断日志用）。
static DC_VIDEO_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// DC 视频发送缓冲阈值（字节）：`media` DataChannel 的 `buffered_amount` 超过它
/// 即丢帧（直播语义：积压 = 过期画面 = 延迟）。默认 256 KiB——1080p60 全 IDR 约
/// 34KB/帧，相当于约 7 帧余量，10 Mbps 链路上界约 200ms 附加延迟；
/// `RDCORE_DC_MAX_BUFFERED_KB` 可覆盖（弱网调小更激进地保实时）。读取一次后缓存。
fn dc_max_buffered_bytes() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RDCORE_DC_MAX_BUFFERED_KB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&kb| kb > 0)
            .map(|kb| kb * 1024)
            .unwrap_or(256 * 1024)
    })
}

/// 读取 `RDCORE_VIDEO_TRANSPORT` 环境变量：`dc` / `datachannel` 强制 DataChannel
/// 视频路径（排障 / 新旧对照实验用）；缺省或任意其它值为 RTP 优先（生产路径）。
fn prefer_video_rtp_from_env() -> bool {
    !matches!(
        std::env::var("RDCORE_VIDEO_TRANSPORT").as_deref(),
        Ok("dc") | Ok("datachannel")
    )
}

/// Host 会话存续期间等待的结果（见 [`Connection::wait_peer_gone_or_rescan`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWaitOutcome {
    /// 当前 Viewer 已掉线（ICE Disconnected/Failed/Closed，或信令通道关闭）。
    Gone,
    /// 收到新 Viewer 的重扫消息（PeerHello / Offer），应立即 `reconnect` 抢占接入。
    Rescan,
}

/// 信令消息的可读类别名（仅日志留痕用，不含载荷，避免泄露 SDP/密钥材料）。
fn sig_msg_kind(m: &Message) -> &'static str {
    match m {
        Message::Offer(_) => "Offer",
        Message::Answer(_) => "Answer",
        Message::Ice(_) => "Ice",
        Message::PeerHello(_) => "PeerHello",
        _ => "其它",
    }
}

/// Offer/Answer 交换之后、握手剩余阶段（数据通道 open → E2E 密钥 → 同意）的总时限。
/// 对端在握手中途死掉（App 超时杀连接 / NAT 抖动）时，无界等待会把 Host 永久
/// 楔死在 establish 里，后续所有 Viewer 的 Offer 无人读取（「重扫一直等待对端
/// 确认」）。取值需大于蜂窝网络经 TURN 中继的最坏 ICE+DTLS 耗时（实测秒级），
/// 同时小于用户可接受的「故障后自动恢复」等待。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

impl Connection {
    /// 生成一次配对的邀请信息（session_id + 一次性 token）。
    ///
    /// 纯函数、无副作用、无网络：Host 在开连接前调它拿到邀请并展示给用户；Viewer 输码 / 扫码后
    /// 连信令并附带 token 完成一次性鉴权。字段格式见 [`PairingInfo`] 文档与计划 §5 协调点1。
    pub fn create_pairing() -> PairingInfo {
        let mut sid = [0u8; 16];
        getrandom::getrandom(&mut sid).expect("系统随机数不可用，无法生成 session_id");
        let mut tok = [0u8; 32];
        getrandom::getrandom(&mut tok).expect("系统随机数不可用，无法生成 token");
        PairingInfo {
            session_id: SessionId(sid),
            token: hex::encode(tok),
        }
    }

    /// 构造 Viewer（控制端）连接并连上信令服务器。
    ///
    /// `store` 可为任意 [`IdentityStore`] 实现（`InMemoryIdentityStore` / `PersistentIdentityStore`），
    /// 由调用方以 `Arc<Mutex<dyn IdentityStore + Send + Sync>>` 注入（A0 放宽，解耦 Track B 的持久化）。
    pub async fn new_viewer(
        url: &str,
        session: SessionId,
        secret: SecretKey,
        store: Arc<Mutex<dyn IdentityStore + Send + Sync>>,
        rtc_cfg: RtcConfig,
        heartbeat_timeout: Duration,
    ) -> Result<Self, AppError> {
        let sig = Arc::new(SignalingClient::connect(url).await?);
        let peer = Arc::new(WebRtcPeer::with_config(rtc_cfg.clone()).await?);
        Ok(Self {
            is_host: false,
            provider: Ed25519CryptoProvider,
            self_secret: secret,
            store,
            session_id: session,
            peer: Mutex::new(peer),
            rtc_cfg,
            sig,
            consent_mode: ConsentMode::Interactive,
            heartbeat_timeout,
            consent: Mutex::new(None),
            session_key: Mutex::new(None),
            peer_verified: Mutex::new(None),
            remote_decision: Mutex::new(None),
            last_host_decision: Mutex::new(None),
            business_rx: tokio::sync::Mutex::new(None),
            video_codec: Mutex::new(VideoCodec::H264),
            media_decoder: Mutex::new(None),
            audio_codec: Mutex::new(AudioCodec::Raw),
            audio_decoder: Mutex::new(None),
            pending_sig: Mutex::new(std::collections::VecDeque::new()),
            prefer_video_rtp: prefer_video_rtp_from_env(),
            video_rtp: Mutex::new(false),
            video_rx: Mutex::new(None),
            rtp_recv: Mutex::new(None),
            video_keyframe_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 构造 Host（被控端）连接并连上信令服务器（同 [`Connection::new_viewer`]）。
    pub async fn new_host(
        url: &str,
        session: SessionId,
        secret: SecretKey,
        store: Arc<Mutex<dyn IdentityStore + Send + Sync>>,
        rtc_cfg: RtcConfig,
        heartbeat_timeout: Duration,
    ) -> Result<Self, AppError> {
        let sig = Arc::new(SignalingClient::connect(url).await?);
        let peer = Arc::new(WebRtcPeer::with_config(rtc_cfg.clone()).await?);
        Ok(Self {
            is_host: true,
            provider: Ed25519CryptoProvider,
            self_secret: secret,
            store,
            session_id: session,
            peer: Mutex::new(peer),
            rtc_cfg,
            sig,
            consent_mode: ConsentMode::Interactive,
            heartbeat_timeout,
            consent: Mutex::new(None),
            session_key: Mutex::new(None),
            peer_verified: Mutex::new(None),
            remote_decision: Mutex::new(None),
            last_host_decision: Mutex::new(None),
            business_rx: tokio::sync::Mutex::new(None),
            video_codec: Mutex::new(VideoCodec::H264),
            media_decoder: Mutex::new(None),
            audio_codec: Mutex::new(AudioCodec::Raw),
            audio_decoder: Mutex::new(None),
            pending_sig: Mutex::new(std::collections::VecDeque::new()),
            prefer_video_rtp: prefer_video_rtp_from_env(),
            video_rtp: Mutex::new(false),
            video_rx: Mutex::new(None),
            rtp_recv: Mutex::new(None),
            video_keyframe_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 跑完整条连接：握手 + Ed25519 验签 + ICE + E2E 会话密钥 + 同意。
    ///
    ///    /// - `stop`：ICE 中继循环用到的退出标志（两对等端共享，握手收尾后置 true）。
    /// - `decision`：仅 Host 使用；给 Viewer 的授权决定（Grant / Deny）。Viewer 传 `None`。
    ///
    /// 完成后 E2E 会话密钥已建立、同意状态已定，可安全收发加密流量。
    ///
    /// A0：改为 `&self`（所有可变状态已收进各 `Mutex` 字段），以便 B5 的 supervisor 以
    /// `Arc<Connection>` 持有并在网络断开后调用 [`Connection::reconnect`] 原地重建。
    pub async fn establish(
        &self,
        stop: Arc<AtomicBool>,
        decision: Option<ConsentDecision>,
    ) -> Result<(), AppError> {
        // 取出当前 PeerConnection 的拥有值句柄（Clone 出来，避免持 std::sync::Mutex 跨 await）。
        let peer = self.peer.lock().unwrap().clone();
        let store_id = self.store.lock().unwrap().local_identity().id;

        // 配对身份交换：先广播本端公开身份（DeviceId + 公钥）。对端按 TOFU 记住首个版本
        // （带外锚 = 一次性配对 token 保护的会话房间），随后的 Offer/Answer 验签才有公钥可查。
        // 只含公开信息；旧端不认识本变体时由 recv 循环自动跳过（见 recv_offer/recv_answer）。
        let hello = Message::PeerHello(self.store.lock().unwrap().local_identity().clone());
        self.sig.send(&hello).await?;

        if !self.is_host {
            // RTP 视频轨道（gap J 生产路径）：先在 Offer 里声明视频 m-line，Host 支持
            // 则 Answer 激活之（见下方 accept_answer 后判定）。失败仅回退、不阻断连接。
            if self.prefer_video_rtp {
                if let Err(e) = peer.setup_video_track().await {
                    eprintln!("[rdcore-app] setup_video_track 失败，视频回退 DataChannel: {e}");
                }
            }
            // Viewer：发签名 Offer → 收签名 Answer（验签）→ 设为 remote description。
            let sdp = peer.create_offer().await?;
            let offer = sign_offer(
                &self.provider,
                &self.self_secret,
                ConnectionOffer {
                    session_id: self.session_id,
                    from: store_id,
                    sdp,
                    capabilities: Self::capabilities(),
                    frame: None,
                    signature: None,
                },
            );
            self.sig.send(&Message::Offer(offer)).await?;

            // 必须设超时：否则 Host 不应答（未运行 / 网络不可达 / 会话失效）时，
            // Viewer 会永久阻塞在 recv_answer，App 侧表现为「一直等待对端确认」。
            let answer = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.recv_answer())
                .await
                .map_err(|_| {
                    AppError::Crypto(format!(
                        "握手超时（{}s）：未收到 Host 的 Answer。请确认 Windows Host 端的 \
                         rdcore-desktop 正在运行、iPhone 与 Host 网络可达（跨网需 TURN），\
                         且配对码未过期。",
                        HANDSHAKE_TIMEOUT.as_secs()
                    ))
                })??;
            let verified = {
                let store_guard = self.store.lock().unwrap();
                verify_answer(&self.provider, &*store_guard, &answer)?
            };
            *self.peer_verified.lock().unwrap() = Some(verified);
            // Answer 的视频 m-line 仍激活（未被 port=0 拒绝）且本端轨道就绪 → 启用 RTP 收帧；
            // 否则留在 DataChannel 回退路径（旧 Host / Web 端互操作）。
            let answer_has_video = sdp_has_active_video(&answer.sdp);
            peer.accept_answer(answer.sdp).await?;
            let rx = if answer_has_video {
                peer.video_receiver().await
            } else {
                None
            };
            if let Some(rx) = rx {
                eprintln!("[rdcore-app] 视频走 RTP 轨道（Answer 含激活视频 m-line）");
                *self.video_rx.lock().unwrap() = Some(rx);
                *self.video_rtp.lock().unwrap() = true;
                *self.rtp_recv.lock().unwrap() = Some(RtpRecvState {
                    got_first_frame: false,
                    first_frame_deadline: tokio::time::Instant::now() + RTP_FIRST_FRAME_TIMEOUT,
                });
            } else if answer_has_video {
                eprintln!("[rdcore-app] Answer 含视频 m-line 但本端轨道未就绪，视频走 DataChannel");
            }
        } else {
            // Host：收签名 Offer（验签）→ 建同意门控 → 回签名 Answer。
            // 同样设超时：Viewer 迟迟不发起 Offer（如重连失败 / 网络抖动）时，
            // 不让本函数永久楔死在 recv_offer，交由上层重连循环读取下一个 Viewer。
            let offer = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.recv_offer())
                .await
                .map_err(|_| {
                    AppError::Crypto(format!(
                        "握手超时（{}s）：未收到 Viewer 的 Offer（Viewer 未发起或网络不可达）",
                        HANDSHAKE_TIMEOUT.as_secs()
                    ))
                })??;
            let verified = {
                let store_guard = self.store.lock().unwrap();
                verify_offer(&self.provider, &*store_guard, &offer)?
            };
            *self.peer_verified.lock().unwrap() = Some(verified.clone());
            let mut gate =
                ConsentGate::new(verified, self.consent_mode.clone(), self.heartbeat_timeout);
            gate.request_consent(None);
            *self.consent.lock().unwrap() = Some(gate);

            // RTP 视频轨道（gap J 生产路径）：Viewer 的 Offer 含激活视频 m-line 才挂载
            //（旧端 / Web Viewer 无视频 m-line，自动留在 DataChannel 回退路径）。
            // 必须在 accept_offer 之前完成，Answer 才会携带视频 m-line。
            let offer_has_video = sdp_has_active_video(&offer.sdp);
            if self.prefer_video_rtp && offer_has_video {
                match peer.setup_video_track().await {
                    Ok(()) => {
                        *self.video_rtp.lock().unwrap() = true;
                        eprintln!("[rdcore-app] 视频走 RTP 轨道（Offer 含激活视频 m-line）");
                    }
                    Err(e) => {
                        eprintln!("[rdcore-app] setup_video_track 失败，视频回退 DataChannel: {e}");
                    }
                }
            }
            let sdp = peer.accept_offer(offer.sdp).await?;
            // 重发一次本端身份：establish 开头广播的 PeerHello 可能发在 Viewer 进房之前
            // （Host 常驻等连接的典型场景），Viewer 没收到就会在 verify_answer 报
            // UnknownPeer。同一信令连接上消息有序，Viewer 必先处理本消息再处理 Answer。
            self.sig.send(&hello).await?;
            let answer = sign_answer(
                &self.provider,
                &self.self_secret,
                ConnectionAnswer {
                    session_id: self.session_id,
                    from: store_id,
                    sdp,
                    capabilities: Self::capabilities(),
                    frame: None,
                    signature: None,
                },
            );
            self.sig.send(&Message::Answer(answer)).await?;
        }

        // ICE 中继循环（后台，跑在信令通道上），握手收尾由 `stop` 退出。
        let stop2 = stop.clone();
        let peer2 = peer.clone();
        let sig = self.sig.clone();
        let session = self.session_id;
        let from = store_id;
        let ice_task = tokio::spawn(async move {
            relay_ice(peer2, sig, session, from, stop2).await;
        });

        // 握手剩余阶段（数据通道 open → E2E 密钥 → 同意）打包并加总时限：
        // 对端在握手中途死掉（App 侧超时杀连接 / NAT 抖动 / P2P 未打通）时，
        // 无界等待会把 Host 永久楔死在本函数里——之后所有 Viewer 的 Offer 都
        // 无人读取（用户看到「重扫一直等待对端确认」）。超时报错，由上层重连
        // 循环读取并处理下一个 Viewer 的 Offer。
        let finish = async {
            // 等两条 negotiated 数据通道 open（ICE + DTLS 成功）。
            peer.wait_data_channels_open().await;

            // E2E 会话密钥交换（经控制通道，明文承载已签名的临时公钥）。
            self.exchange_session_key().await?;

            // 同意握手：Host 决定并下发；Viewer 接收并反映本端状态。
            if self.is_host {
                let decision = decision.unwrap_or(ConsentDecision::Deny {
                    reason: "未提供授权决定".into(),
                });
                {
                    let mut gate = self.consent.lock().unwrap();
                    if let Some(g) = gate.as_mut() {
                        g.decide(decision.clone());
                    }
                }
                // 记下本次 Host 授权决定，供 `reconnect` 自包含复用（B5）。
                *self.last_host_decision.lock().unwrap() = Some(decision.clone());
                self.send_app(&AppMessage::Consent(decision)).await?;
            } else {
                loop {
                    match self.recv_app().await? {
                        Some(AppMessage::Consent(d)) => {
                            *self.remote_decision.lock().unwrap() = Some(d);
                            break;
                        }
                        Some(_) => continue,
                        None => {
                            return Err(AppError::Crypto("连接关闭，未收到授权决定".into()));
                        }
                    }
                }
            }
            Ok::<(), AppError>(())
        };
        let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, finish).await;

        stop.store(true, Ordering::SeqCst);
        let _ = ice_task.await;
        match result {
            Ok(r) => r,
            Err(_) => Err(AppError::Crypto(format!(
                "握手超时（{}s）：对端中途离线或 P2P 未打通",
                HANDSHAKE_TIMEOUT.as_secs()
            ))),
        }
    }

    /// 断线后原地重建 P2P 连接并重跑握手 / 密钥 / 同意（B5 韧性面用）。
    ///
    /// 设计要点（A0）：B5 的 supervisor 以 `Arc<Connection>` 持有本连接，故 `reconnect` 必须
    /// `&self`。内部 `Mutex<Arc<WebRtcPeer>>` 在持锁瞬间换出一条全新 PeerConnection；换出后
    /// `channels()` 返回的仍是可 Clone 的拥有值句柄，旧任务持有的旧句柄自然失效、新任务用新句柄。
    /// 复用持久化身份（TOFU）+ 上一次 Host 授权决定（`last_host_decision`），无需再次走二维码配对。
    pub async fn reconnect(&self) -> Result<(), AppError> {
        // 1) 先 await 建好新 PeerConnection（不在持 std::sync::Mutex 期间 await），再持锁换出。
        let new_peer = WebRtcPeer::with_config(self.rtc_cfg.clone()).await?;
        {
            let mut g = self.peer.lock().unwrap();
            *g = Arc::new(new_peer);
        }
        // 2) 清空上一轮的可变握手态，准备重跑。
        *self.peer_verified.lock().unwrap() = None;
        *self.session_key.lock().unwrap() = None;
        *self.consent.lock().unwrap() = None;
        *self.remote_decision.lock().unwrap() = None;
        *self.media_decoder.lock().unwrap() = None;
        // RTP 视频状态一并复位：新 PeerConnection 的 establish 会重新协商/挂载轨道。
        *self.video_rx.lock().unwrap() = None;
        *self.video_rtp.lock().unwrap() = false;
        *self.rtp_recv.lock().unwrap() = None;
        let stop = Arc::new(AtomicBool::new(false));
        let last_decision = self.last_host_decision.lock().unwrap().clone();
        // 3) 重跑整条 establish（Host 复用上次的授权决定）。
        self.establish(stop, last_decision).await
    }

    /// 与 [`Connection::reconnect`] 相同，但显式指定本轮 Host 授权决定。
    ///
    /// `reconnect` 复用的 `last_host_decision` 只在 establish 走到同意下发阶段才赋值；
    /// 若首次 establish 在握手早期失败（信令抖动 / Viewer 中止），该值为 `None`，裸
    /// `reconnect` 会触发 `establish` 内的 `unwrap_or(Deny)` 把后续 Viewer 自动拒绝。
    /// 本变体先播种授权决定再走重连，供 Host 的「接受 → 断开 → 重连」常驻循环使用。
    pub async fn reconnect_with(&self, decision: ConsentDecision) -> Result<(), AppError> {
        *self.last_host_decision.lock().unwrap() = Some(decision);
        self.reconnect().await
    }

    /// 交换端到端会话密钥：双方各生成 X25519 临时密钥、用 Ed25519 签名后明文交换，
    /// 验签通过再 ECDH 派生两端一致的会话密钥。
    async fn exchange_session_key(&self) -> Result<(), AppError> {
        let peer = self.peer.lock().unwrap().clone();
        let (_media, dc) = peer.channels();
        let (pub_k, sec_k) = ephemeral_x25519_keypair();
        let store_id = self.store.lock().unwrap().local_identity().id;
        let ex = sign_ephemeral_key(
            &self.provider,
            &self.self_secret,
            self.session_id,
            store_id,
            x25519_public_bytes(&pub_k),
        );
        // 先发己方临时公钥（明文；临时公钥本就该公开，签名防 MITM 替换）。
        dc.send(&Message::SessionKey(ex)).await?;
        // 收对端临时公钥（明文）→ 验签 + ECDH 派生会话密钥。
        let their = loop {
            match dc.recv().await? {
                Some(Message::SessionKey(e)) => break e,
                Some(_) => continue,
                None => return Err(AppError::Crypto("控制通道关闭，未收到对端会话密钥".into())),
            }
        };
        let key = {
            let store_guard = self.store.lock().unwrap();
            establish_session_key(
                &self.provider,
                &*store_guard,
                &sec_k,
                &their,
                self.session_id,
            )?
        };
        *self.session_key.lock().unwrap() = Some(key);
        Ok(())
    }

    /// 发送一帧媒体（像素经 E2E 会话密钥 AEAD 加密后发出）。
    ///
    /// 路由：已协商 RTP 视频轨道时走 RTP（生产路径——传输不保证送达，丢帧由对端
    /// 重组器吸收，不会因发送缓冲堆积而累积延迟）；否则回退 `media` DataChannel
    ///（可靠有序，兼容旧端）。两种路径的线格式一致：postcard(MediaFrame)，
    /// 其中像素字段为 AEAD 密文——RTP 层只搬不透明字节，E2E 安全语义不变。
    pub async fn send_media(&self, frame: &MediaFrame) -> Result<(), AppError> {
        // 仅在同步段持锁取密钥，不跨 await 持有 MutexGuard。
        let ct = {
            let key = self.session_key.lock().unwrap();
            let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
            aead_seal(key, &frame.data)
        };
        // 仅加密像素字节：宽/高/编码非敏感，留明文便于对端解码器预分配缓冲。
        let sealed_data = postcard::to_stdvec(&ct).map_err(|e| AppError::Crypto(e.to_string()))?;
        let mut sealed = frame.clone();
        sealed.data = sealed_data;
        let peer = self.peer.lock().unwrap().clone();
        if *self.video_rtp.lock().unwrap() {
            let payload =
                postcard::to_stdvec(&sealed).map_err(|e| AppError::Crypto(e.to_string()))?;
            peer.push_video_frame(&payload).await?;
            return Ok(());
        }
        let (media, _dc) = peer.channels();
        // 直播语义丢帧（仅 DC 回退路径——web 端与旧 Viewer 走这里）：DC 可靠有序，
        // 发送缓冲一旦积压，里面的全是过期画面，延迟单调累积。SCTP 流控会把「链路慢 /
        // 对端消费慢」都反映成 buffered_amount 上涨；超过阈值说明前面还有未排空的帧，
        // 本帧直接不发送，从源头把延迟钉在 阈值/链路带宽 以内，而不是无限排队。
        // P 帧流下拉掉一帧会破坏其后参考链，故同时登记关键帧请求：泵在编码下一帧前
        // 消费该标志并强制 IDR，Viewer 最坏滞后一帧即自愈。RTP 路径天然无此问题
        // （无缓冲、丢帧由对端重组器吸收）。
        let buffered = peer.media_data_channel().buffered_amount().await;
        if buffered > dc_max_buffered_bytes() {
            self.note_video_keyframe_requested();
            let n = DC_VIDEO_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n == 1 || n % 120 == 0 {
                eprintln!(
                    "[rdcore-app] DC 视频发送缓冲积压 {buffered}B 超阈值 {}B，丢帧保实时（累计 {n}）",
                    dc_max_buffered_bytes()
                );
            }
            return Ok(());
        }
        media.send_frame(&sealed).await?;
        Ok(())
    }

    /// 接收一帧媒体（解密像素字节）。
    ///
    /// RTP 路径（已协商时）与 DataChannel 回退路径的关键差异：RTP 下丢包/损坏是
    /// 常态，**残帧静默丢弃继续等下一帧**（参考链若受损，由解码层的关键帧请求 +
    /// 1 秒周期 IDR 兜底恢复），绝不因单帧解密失败而断连；DataChannel 是可靠通道，
    /// 解密失败必为篡改/密钥错配，依旧报错。
    ///
    /// 取消安全：全部 await 点（mpsc `recv` / `timeout_at`）均可安全取消，FFI 拉帧侧
    /// 的 1ms 追帧超时频繁取消本调用不会吞半帧（分片重组态在 rtc 层持久）。
    pub async fn recv_media(&self) -> Result<Option<MediaFrame>, AppError> {
        let rx = self.video_rx.lock().unwrap().clone();
        if let Some(rx) = rx {
            loop {
                // 旧 Host 兜底：首帧未收前带死线等待，超时判定对端不发 RTP，
                // 摘除 RTP 状态并落到下方 DataChannel 回退（其缓冲帧不丢）。
                let deadline = {
                    let st = self.rtp_recv.lock().unwrap();
                    match *st {
                        Some(s) if !s.got_first_frame => Some(s.first_frame_deadline),
                        _ => None,
                    }
                };
                let got = match deadline {
                    Some(dl) => match tokio::time::timeout_at(dl, rx.recv()).await {
                        Ok(v) => v,
                        Err(_) => {
                            eprintln!(
                                "[rdcore-app] RTP 视频首帧超时（{}s），回退 DataChannel 视频",
                                RTP_FIRST_FRAME_TIMEOUT.as_secs()
                            );
                            *self.video_rx.lock().unwrap() = None;
                            *self.video_rtp.lock().unwrap() = false;
                            *self.rtp_recv.lock().unwrap() = None;
                            break;
                        }
                    },
                    None => rx.recv().await,
                };
                let Some(bytes) = got else {
                    return Ok(None);
                };
                let sealed: MediaFrame = match postcard::from_bytes(&bytes) {
                    Ok(f) => f,
                    Err(_) => {
                        eprintln!("[rdcore-app] RTP 帧 postcard 反序列化失败，丢帧");
                        continue;
                    }
                };
                let ct: Ciphertext = match postcard::from_bytes(&sealed.data) {
                    Ok(c) => c,
                    Err(_) => {
                        eprintln!("[rdcore-app] RTP 帧密文反序列化失败，丢帧");
                        continue;
                    }
                };
                let plain = {
                    let key = self.session_key.lock().unwrap();
                    let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
                    aead_open(key, &ct)
                };
                match plain {
                    Some(plain) => {
                        if let Some(st) = self.rtp_recv.lock().unwrap().as_mut() {
                            st.got_first_frame = true;
                        }
                        let mut f = sealed;
                        f.data = plain;
                        return Ok(Some(f));
                    }
                    None => {
                        // 丢包残帧 / 乱序错拼必然 AEAD 校验失败——这正是 RTP 路径的
                        // 完整性护栏：坏帧到不了解码器。丢帧继续，下一帧 IDR 恢复。
                        eprintln!("[rdcore-app] RTP 帧解密失败（丢包残帧 / 篡改），丢帧");
                        continue;
                    }
                }
            }
        }
        // DataChannel 回退路径（旧端 / RTP 未协商 / 首帧兜底后）。
        let peer = self.peer.lock().unwrap().clone();
        let (media, _dc) = peer.channels();
        match media.recv_frame().await? {
            None => Ok(None),
            Some(sealed) => {
                let ct: Ciphertext = postcard::from_bytes(&sealed.data)
                    .map_err(|e| AppError::Crypto(e.to_string()))?;
                let plain = {
                    let key = self.session_key.lock().unwrap();
                    let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
                    aead_open(key, &ct)
                };
                match plain {
                    Some(plain) => {
                        let mut f = sealed;
                        f.data = plain;
                        Ok(Some(f))
                    }
                    None => Err(AppError::Crypto(
                        "媒体帧解密失败（篡改 / 密钥不匹配）".into(),
                    )),
                }
            }
        }
    }

    /// 设置本连接使用的视频编解码器（Host 决定后调用；Viewer 须设为一致值）。
    ///
    /// 切换 codec 会丢弃既有 Viewer 解码器，下次 [`Connection::recv_rendered`] 按新 codec 重建。
    pub fn set_video_codec(&self, codec: VideoCodec) {
        *self.video_codec.lock().unwrap() = codec;
        *self.media_decoder.lock().unwrap() = None;
    }

    /// 当前协商的视频编解码器。
    pub fn video_codec(&self) -> VideoCodec {
        *self.video_codec.lock().unwrap()
    }

    /// Track A 媒体面：连接建立（E2E 加密已就绪）后，启动 Host 侧抓屏→编码→媒体通道发送循环。
    ///
    /// `factory` 在**捕获线程内**就地构造帧来源 `C`（`|| C`）。真实 `CaptureSource`（如
    /// `ScrapCaptureSource`，包裹 scrap 的 DXGI `Capturer`，持有 COM 裸指针，`!Send`）必须在
    /// 捕获线程内构造，绝不能从调用线程 move 过来；`factory` 自身必须是 `Send`
    /// （即不捕获任何 `!Send` 数据），但产出的 `C` 可以 `!Send`。`headless` 用 `|| NullCaptureSource::new(...)`
    /// 即可。
    ///
    /// 返回的 [`HostMediaPump`] 在 drop 或 `stop()` 时退出；循环内部按 `fps` 调
    /// `CaptureSource::next_frame`，经 [`rdcore_encode::RawEncoder`] 编码后，由
    /// [`Connection::send_media`] 把**像素走端到端 AEAD 加密**发出（与 [`Connection::recv_media`]
    /// 解密对称）。
    ///
    /// 注意：视频像素在本方法启动前已由 `establish()` 完成 E2E 密钥协商，循环只负责传输，
    /// 不涉及握手/加密逻辑（与 Track B 的韧性逻辑正交）。需 `Arc<Self>` 以便后台任务安全地
    /// 跨线程持有连接并调用 `send_media`。
    pub fn start_capture<C, F>(self: Arc<Self>, factory: F, fps: u16) -> HostMediaPump
    where
        C: CaptureSource + 'static,
        F: FnOnce() -> C + Send + 'static,
    {
        let codec = *self.video_codec.lock().unwrap();
        HostMediaPump::start_with(
            factory,
            self.clone(),
            fps,
            codec,
            self.video_keyframe_requested.clone(),
        )
    }

    /// 接收并解码一帧媒体为可显示缓冲：先 [`Connection::recv_media`]（E2E 解密像素），
    /// 再按 [`Connection::video_codec`] 选解码器（[`rdcore_decode::RawDecoder`] / `H264Decoder`）
    /// 还原 RGBA，最后 [`rdcore_render::render`] 产出 [`RenderedFrame`] 供 GUI 直接 blit。
    /// 通道关闭返回 `Ok(None)`。
    ///
    /// `DecodeError::NoFrame`（解码未产出画面）按**跳帧**处理、继续取下一帧：硬编
    /// （NVENC 等）启动期流水线延迟会产出仅含 SPS/PPS 的占位帧，RTP 路径丢包后解码器
    /// 也可能需要更多数据——二者都是正常暂态而非错误（SPS/PPS 逐帧前置、关键帧请求
    /// 机制兜底恢复），不应上报为拉帧失败。
    pub async fn recv_rendered(&self) -> Result<Option<RenderedFrame>, AppError> {
        // 连续解码失败计数：P 帧流下丢帧/包损坏会污染参考链，表现为连续解码错误；
        // 每次失败即请求 Host 发 IDR 并重置本地解码器，通常 1 个 RTT 内自愈；
        // 连续 30 次仍失败才判定链路级故障并上报。
        let mut dec_fail = 0u32;
        loop {
            match self.recv_media().await? {
                None => return Ok(None),
                Some(frame) => {
                    // 惰性构造并按需复用持久解码器（同 codec 跨帧共享，保留 SPS/PPS 状态）。
                    let codec = *self.video_codec.lock().unwrap();
                    let mut dec_guard = self.media_decoder.lock().unwrap();
                    if dec_guard.is_none() {
                        let dec = rdcore_decode::new_decoder(codec)
                            .map_err(|e| AppError::Crypto(format!("帧解码器创建失败: {e}")))?;
                        *dec_guard = Some(dec);
                    }
                    let decoded: DecodedFrame = match dec_guard.as_ref().unwrap().decode(&frame) {
                        Ok(d) => d,
                        Err(rdcore_decode::DecodeError::NoFrame) => {
                            eprintln!(
                                "[rdcore-app] 解码未产出画面（启动参数集帧 / 需更多数据），跳帧"
                            );
                            continue;
                        }
                        Err(e) => {
                            // P 帧流恢复：重置解码器（丢弃被污染的参考状态）+ 请求 Host
                            // 下一帧 IDR。std MutexGuard 非 Send，须先放锁再 await。
                            dec_fail += 1;
                            *dec_guard = None;
                            drop(dec_guard);
                            if dec_fail >= 30 {
                                return Err(AppError::Crypto(format!(
                                    "帧解码连续失败 {dec_fail} 次（关键帧请求未获响应）: {e}"
                                )));
                            }
                            eprintln!(
                                "[rdcore-app] 帧解码失败（第 {dec_fail} 次）：{e}，请求关键帧恢复"
                            );
                            let _ = self.send_keyframe_request().await;
                            continue;
                        }
                    };
                    drop(dec_guard);
                    let rendered = rdcore_render::render(&decoded)
                        .map_err(|e| AppError::Crypto(format!("帧渲染失败: {e}")))?;
                    return Ok(Some(rendered));
                }
            }
        }
    }

    // ───────────────────────────── C 音频管线（与视频平行、互不阻塞） ─────────────────────────────

    /// 设置本连接使用的音频编解码器（Host 决定后调用；Viewer 须设为一致值）。
    ///
    /// 切换 codec 会丢弃既有 Viewer 解码器，下次 [`Connection::recv_rendered_audio`] 按新 codec 重建。
    pub fn set_audio_codec(&self, codec: AudioCodec) {
        *self.audio_codec.lock().unwrap() = codec;
        *self.audio_decoder.lock().unwrap() = None;
    }

    /// 当前协商的音频编解码器。
    pub fn audio_codec(&self) -> AudioCodec {
        *self.audio_codec.lock().unwrap()
    }

    /// 发送一帧音频（字节经 E2E 会话密钥 AEAD 加密后发出；与视频同一条加密通道、不同 SCTP 通道）。
    ///
    /// 与 [`Connection::send_media`] 对称：仅加密 `data` 字节，`channels`/`sample_rate` 留明文便于
    /// 对端播放器预分配缓冲。音频走独立的 `audio` DataChannel（id=2），与视频互不阻塞。
    pub async fn send_audio(&self, frame: &AudioFrame) -> Result<(), AppError> {
        let ct = {
            let key = self.session_key.lock().unwrap();
            let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
            aead_seal(key, &frame.data)
        };
        let sealed_data = postcard::to_stdvec(&ct).map_err(|e| AppError::Crypto(e.to_string()))?;
        let mut sealed = frame.clone();
        sealed.data = sealed_data;
        let peer = self.peer.lock().unwrap().clone();
        let audio = peer.audio_channel();
        audio.send_frame(&sealed).await?;
        Ok(())
    }

    /// 接收一帧音频（解密字节）。
    pub async fn recv_audio(&self) -> Result<Option<AudioFrame>, AppError> {
        let peer = self.peer.lock().unwrap().clone();
        let audio = peer.audio_channel();
        match audio.recv_frame().await? {
            None => Ok(None),
            Some(sealed) => {
                let ct: Ciphertext = postcard::from_bytes(&sealed.data)
                    .map_err(|e| AppError::Crypto(e.to_string()))?;
                let plain = {
                    let key = self.session_key.lock().unwrap();
                    let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
                    aead_open(key, &ct)
                };
                match plain {
                    Some(plain) => {
                        let mut f = sealed;
                        f.data = plain;
                        Ok(Some(f))
                    }
                    None => Err(AppError::Crypto(
                        "音频帧解密失败（篡改 / 密钥不匹配）".into(),
                    )),
                }
            }
        }
    }

    /// Track A 音频面：连接建立（E2E 加密已就绪）后，启动 Host 侧采集→编码→音频通道发送循环。
    ///
    /// 与 [`Connection::start_capture`] 完全平行：`factory` 在捕获线程内就地构造 `AudioSource`
    /// （真实后端如 `CpalAudioSource` 可能 `!Send`），编码后经 [`Connection::send_audio`] 把
    /// 音频走端到端 AEAD 加密发出。音频抖动/丢帧不影响视频流畅度（独立 SCTP 通道）。
    pub fn start_audio_capture<C, F>(self: Arc<Self>, factory: F, fps: u16) -> HostAudioPump
    where
        C: rdcore_audio::AudioSource + 'static,
        F: FnOnce() -> C + Send + 'static,
    {
        let codec = *self.audio_codec.lock().unwrap();
        HostAudioPump::start_with(factory, self, fps, codec)
    }

    /// 接收并解出一帧音频为可播放的 Raw PCM（供 `AudioSink` 播放）。
    ///
    /// 先 [`Connection::recv_audio`]（E2E 解密字节），再按 [`Connection::audio_codec`] 选解码器
    /// （`Raw` 直通 / `Opus` 经 `real` feature）还原 16-bit 交错 PCM。通道关闭返回 `Ok(None)`。
    pub async fn recv_rendered_audio(&self) -> Result<Option<AudioFrame>, AppError> {
        match self.recv_audio().await? {
            None => Ok(None),
            Some(frame) => {
                let codec = *self.audio_codec.lock().unwrap();
                let mut dec_guard = self.audio_decoder.lock().unwrap();
                if dec_guard.is_none() {
                    let dec = rdcore_audio::new_decoder(codec)
                        .map_err(|e| AppError::Crypto(format!("音频解码器创建失败: {e}")))?;
                    *dec_guard = Some(dec);
                }
                let decoded: AudioFrame = dec_guard
                    .as_ref()
                    .unwrap()
                    .decode(&frame)
                    .map_err(|e| AppError::Crypto(format!("音频解码失败: {e}")))?;
                drop(dec_guard);
                Ok(Some(decoded))
            }
        }
    }

    /// 发送一条应用层控制消息（整条经 E2E 会话密钥 AEAD 加密后发出）。
    pub async fn send_app(&self, msg: &AppMessage) -> Result<(), AppError> {
        let bytes = postcard::to_stdvec(msg).map_err(|e| AppError::Crypto(e.to_string()))?;
        let ct = {
            let key = self.session_key.lock().unwrap();
            let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
            aead_seal(key, &bytes)
        };
        let peer = self.peer.lock().unwrap().clone();
        let (_media, dc) = peer.channels();
        dc.send(&Message::Encrypted(ct)).await?;
        Ok(())
    }

    /// 接收一条应用层控制消息（解密后还原 `AppMessage`）。
    pub async fn recv_app(&self) -> Result<Option<AppMessage>, AppError> {
        let peer = self.peer.lock().unwrap().clone();
        let (_media, dc) = peer.channels();
        loop {
            match dc.recv().await? {
                None => return Ok(None),
                Some(Message::Encrypted(ct)) => {
                    let opened = {
                        let key = self.session_key.lock().unwrap();
                        let key = key.as_ref().ok_or(AppError::NoSessionKey)?;
                        aead_open(key, &ct)
                    };
                    match opened {
                        Some(bytes) => {
                            if let Ok(m) = postcard::from_bytes::<AppMessage>(&bytes) {
                                return Ok(Some(m));
                            }
                            // 解密成功但反序列化失败（协议错配）→ 忽略，继续收。
                            continue;
                        }
                        None => continue, // 解密失败（篡改 / 错误密钥）→ 忽略
                    }
                }
                Some(_) => continue, // 握手期遗留的非加密消息 → 忽略
            }
        }
    }

    /// Track A 媒体面：发送一条远程输入事件（Viewer→Host）。
    ///
    /// 经已加密的控制通道（`AppMessage::Input` → AEAD → DataChannel）发出，云端只见密文。
    pub async fn send_input(&self, event: &InputEvent) -> Result<(), AppError> {
        self.send_app(&AppMessage::Input(event.clone())).await
    }

    /// Track A 媒体面：请求 Host 下一帧输出关键帧（IDR，Viewer→Host）。
    ///
    /// P 帧流下解码端丢帧/花屏/解码积压后的快速恢复手段（PLI/FIR 语义）；
    /// Host 侧由 supervisor / `recv_input` 就地置位，媒体泵在下一帧编码前消费。
    pub async fn send_keyframe_request(&self) -> Result<(), AppError> {
        self.send_app(&AppMessage::RequestKeyframe).await
    }

    /// Host 侧：登记「下一帧必须编码为 IDR」请求（supervisor / recv_input / 丢帧自愈共用）。
    pub(crate) fn note_video_keyframe_requested(&self) {
        self.video_keyframe_requested.store(true, Ordering::SeqCst);
    }

    /// Track B 集成点（契约 §9）：注入 supervisor 转发的业务消息通道。
    ///
    /// 一旦注入，`[`Connection::recv_input`]` 将优先从该 `broadcast` 通道读取 `Input` 业务消息，
    /// 而不再调用 `[`Connection::recv_app`]`——后者由 `ConnectionSupervisor` 独占（心跳就地处理、
    /// 业务消息经此 broadcast 转发）。这样 Track A 的输入接收与 Track B 的心跳环不再争抢同一条
    /// 控制通道。
    ///
    /// 调用方（Track B 的 `ConnectionSupervisor::start`）应传入其业务 `broadcast` 通道的一个
    /// 订阅者：`let rx = biz_tx.subscribe(); conn.set_business_receiver(rx).await;`
    /// 使用 `broadcast` 而非 `mpsc` 是为支持多消费者：supervisor 自身（`recv_business`）与
    /// 本连接（`recv_input`）可各自持有一个订阅者，互不影响。
    pub async fn set_business_receiver(&self, rx: tokio::sync::broadcast::Receiver<AppMessage>) {
        *self.business_rx.lock().await = Some(rx);
    }

    /// Track A 媒体面：接收一条远程输入事件（Host 侧轮询）。
    ///
    /// - **已挂载 supervisor（契约 §9）**：业务消息经 [`Connection::set_business_receiver`]
    ///   注入的 `broadcast` 通道到达，本方法从那里读取 `Input`，不再触碰 `recv_app`，
    ///   从而与 supervisor 的心跳环互不争抢控制通道。
    /// - **无 supervisor（孤立场景 / 单元测试）**：回退到 [`Connection::recv_app`] 的加密
    ///   控制通道，遇 `Input` 变体即返回，其它控制消息（心跳 / 授权 / 撤销）透明跳过。
    ///
    /// 通道关闭返回 `Ok(None)`。
    pub async fn recv_input(&self) -> Result<Option<InputEvent>, AppError> {
        if let Some(rx) = self.business_rx.lock().await.as_mut() {
            loop {
                match rx.recv().await {
                    Ok(AppMessage::Input(e)) => return Ok(Some(e)),
                    // Viewer 请求关键帧（丢帧/花屏/积压恢复）：置位，泵在下一帧编码前消费。
                    Ok(AppMessage::RequestKeyframe) => {
                        self.note_video_keyframe_requested();
                        continue;
                    }
                    Ok(_) => continue, // 其它业务消息（剪贴板 / 文件传输）：忽略
                    Err(broadcast::error::RecvError::Lagged(_)) => continue, // 缓冲溢出：跳过
                    Err(broadcast::error::RecvError::Closed) => return Ok(None),
                }
            }
        }
        // 回退路径：无 supervisor 时直接收 E2E 加密控制通道。
        loop {
            match self.recv_app().await? {
                None => return Ok(None),
                Some(AppMessage::Input(e)) => return Ok(Some(e)),
                Some(AppMessage::RequestKeyframe) => {
                    self.note_video_keyframe_requested();
                    continue;
                }
                Some(_) => continue, // 心跳 / 授权 / 撤销等：忽略，继续收
            }
        }
    }

    /// 不可伪造横幅所需的实时数据（Host 来自 `ConsentGate`，Viewer 来自收到的授权决定）。
    pub fn security_indicator(&self) -> Option<SecurityIndicator> {
        let encrypted = self.session_key.lock().unwrap().is_some();
        let vp = self.peer_verified.lock().unwrap().clone()?;
        let state = {
            let consent_guard = self.consent.lock().unwrap();
            if let Some(gate) = consent_guard.as_ref() {
                gate.state().clone()
            } else {
                let rd = self.remote_decision.lock().unwrap();
                match rd.as_ref() {
                    Some(ConsentDecision::Grant { scopes, duration }) => {
                        rdcore_consent::ConnectionState::Active {
                            scopes: scopes.clone(),
                            expires_at: duration.map(|d| Instant::now() + d),
                        }
                    }
                    Some(ConsentDecision::Deny { reason }) => {
                        rdcore_consent::ConnectionState::Denied {
                            reason: reason.clone(),
                        }
                    }
                    None => rdcore_consent::ConnectionState::AwaitingConsent,
                }
            }
        };
        Some(SecurityIndicator {
            display_name: vp.display_name.clone(),
            device_id: vp.id,
            fingerprint: vp.fingerprint.clone(),
            fingerprint_spaced: vp.fingerprint.to_spaced_hex(),
            state,
            encrypted,
        })
    }

    /// 是否已激活（已授权且未关闭）。
    pub fn is_active(&self) -> bool {
        {
            let consent_guard = self.consent.lock().unwrap();
            if let Some(gate) = consent_guard.as_ref() {
                return gate.is_active();
            }
        }
        let rd = self.remote_decision.lock().unwrap();
        matches!(rd.as_ref(), Some(ConsentDecision::Grant { .. }))
    }

    /// 当前授予的权限范围。
    pub fn granted_scopes(&self) -> HashSet<ConsentScope> {
        {
            let consent_guard = self.consent.lock().unwrap();
            if let Some(gate) = consent_guard.as_ref() {
                if let rdcore_consent::ConnectionState::Active { scopes, .. } = gate.state() {
                    return scopes.clone();
                }
            }
        }
        {
            let rd = self.remote_decision.lock().unwrap();
            if let Some(ConsentDecision::Grant { scopes, .. }) = rd.as_ref() {
                return scopes.clone();
            }
        }
        HashSet::new()
    }

    /// 已建立的端到端会话密钥（用于上层断言 / 调试，不落盘）。
    /// A0：字段已收进 `Mutex`，故返回拥有值副本（`SessionKey: Clone`）。
    pub fn session_key(&self) -> Option<SessionKey> {
        self.session_key.lock().unwrap().clone()
    }

    /// 当前 P2P 连接状态（New / Connecting / Connected / Disconnected / Failed / Closed）。
    ///
    /// 供上层安全指示器 / 横幅反映真实连接健康度：即使两条数据通道已 open，也可能因网络
    /// 切换进入 `Disconnected` 再恢复，或彻底 `Failed`；生产 UI 应订阅此状态做重连 / 告警。
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.peer.lock().unwrap().connection_state()
    }

    /// 对端是否已离开（供 Host 侧做断线检测以触发重连）。
    ///
    /// `Disconnected` / `Failed` / `Closed` 视为对端已走；其余（`New` / `Connecting` /
    /// `Connected`）视为仍在或尚未连上。配合 Host 的「接受 Viewer → 等断开 → 重连」循环使用。
    pub fn peer_gone(&self) -> bool {
        matches!(
            self.peer.lock().unwrap().connection_state(),
            RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed
        )
    }

    /// Host 会话存续期间阻塞等待：「新 Viewer 重扫」或「当前对端掉线」，先到先返回。
    ///
    /// 背景：Viewer（尤其 iOS）被杀/退后台后，ICE 掉线检测需要数十秒甚至更久才触发；
    /// 若 Host 只在确认旧对端死透后才重新读信令，用户在窗口期内重扫同一二维码会让
    /// Offer 无人接收，卡在「等待对端确认」。本方法让 Host 会话期间就监听信令：
    /// 新 Viewer 的 `PeerHello` / `Offer` 一到立即返回 [`HostWaitOutcome::Rescan`]，
    /// 上层随即 `reconnect` 抢占接入，重扫秒级生效、二维码不变、无需重启 Host。
    ///
    /// 预读到的所有消息（含重扫方的 PeerHello/Offer）存入 `pending_sig`，由下一轮
    /// establish 的 `recv_offer` 优先消费，不丢消息。迟到的 ICE 候选等会话杂音
    /// 只入缓冲、不触发 Rescan，避免误拆健康会话。
    pub async fn wait_peer_gone_or_rescan(&self) -> HostWaitOutcome {
        let mut idle_ticks = 0u32;
        loop {
            if self.peer_gone() {
                eprintln!("[host-wait] 对端连接状态已坏（ICE）→ 按离开处理");
                return HostWaitOutcome::Gone;
            }
            tokio::select! {
                msg = self.sig.recv() => {
                    match msg {
                        Ok(Some(m)) => {
                            eprintln!("[host-wait] 收到信令消息：{}", sig_msg_kind(&m));
                            let is_rescan =
                                matches!(m, Message::PeerHello(_) | Message::Offer(_));
                            self.pending_sig.lock().unwrap().push_back(m);
                            if is_rescan {
                                return HostWaitOutcome::Rescan;
                            }
                            // 其它消息（迟到 ICE 等）：入缓冲后继续等。
                        }
                        // 信令通道关闭/错误：按对端离开处理（重连循环负责重试）。
                        other => {
                            eprintln!("[host-wait] 信令通道已关闭/出错（{other:?}）→ 按离开处理");
                            return HostWaitOutcome::Gone;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    idle_ticks += 1;
                    // 每 ~10s 打一次心跳，证明等待循环本身活着（区分「循环没跑」与「没收到」）。
                    if idle_ticks % 20 == 0 {
                        eprintln!("[host-wait] 等待中…（{}s 未收到信令）", idle_ticks / 2);
                    }
                }
            }
        }
    }

    /// 阻塞等待直到 PeerConnection 真正进入 `Connected`（ICE + DTLS 成功、数据通道 open）。
    /// `timeout` 内未连上返回 `false`。用于上层区分"已建立 E2E 加密"与"真正已连上传输"。
    pub async fn wait_connected(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.peer.lock().unwrap().connection_state() == RTCPeerConnectionState::Connected
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok()
    }

    /// 收信令上的 Offer（早于 Offer 到达的 ICE 候选也顺手加入连接，避免被丢弃；
    /// 优先消费 `wait_peer_gone_or_rescan` 预读进 `pending_sig` 的重扫消息）。
    async fn recv_offer(&self) -> Result<ConnectionOffer, AppError> {
        loop {
            match self.next_sig_message().await? {
                Some(Message::Offer(o)) => return Ok(o),
                Some(Message::Ice(i)) => self.add_remote_ice(&i).await,
                Some(Message::PeerHello(p)) => self.remember_peer_tofu(p),
                Some(_) => continue,
                None => return Err(AppError::Crypto("信令通道关闭，未收到 Offer".into())),
            }
        }
    }

    /// 收信令上的 Answer（早于 Answer 到达的 ICE 候选也顺手加入连接，避免被丢弃）。
    async fn recv_answer(&self) -> Result<ConnectionAnswer, AppError> {
        loop {
            match self.next_sig_message().await? {
                Some(Message::Answer(a)) => return Ok(a),
                Some(Message::Ice(i)) => self.add_remote_ice(&i).await,
                Some(Message::PeerHello(p)) => self.remember_peer_tofu(p),
                Some(_) => continue,
                None => return Err(AppError::Crypto("信令通道关闭，未收到 Answer".into())),
            }
        }
    }

    /// 取下一条信令消息：优先弹 `pending_sig` 预读缓冲，空了才阻塞读信令通道。
    async fn next_sig_message(&self) -> Result<Option<Message>, AppError> {
        if let Some(m) = self.pending_sig.lock().unwrap().pop_front() {
            return Ok(Some(m));
        }
        Ok(self.sig.recv().await?)
    }

    /// TOFU 记住对端身份：仅当该 DeviceId 尚未被记住时采纳；已记住则忽略，
    /// 防止已配对设备的公钥被会话内消息替换（防握手后密钥降级/替换攻击）。
    fn remember_peer_tofu(&self, peer: PeerIdentity) {
        let mut store = self.store.lock().unwrap();
        if store.lookup(&peer.id).is_none() {
            store.remember(peer);
        }
    }

    /// 把对端经信令发来的 ICE 候选加入连接。remote description 尚未就绪时会由
    /// `WebRtcPeer` 内部缓冲，待 `accept_offer`/`accept_answer` 设置后自动刷入。
    async fn add_remote_ice(&self, i: &IceCandidate) {
        if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&i.candidate) {
            let peer = self.peer.lock().unwrap().clone();
            let _ = peer.add_ice_candidate(init).await;
        }
    }

    /// 本端能力（编码 / 分辨率 / 输入类型 / 剪贴板开关），纳入 Offer/Answer 签名。
    /// fps 为「可提供的帧率上限」声明值（无消费方门控，实际发送率由 Host `--fps` 决定，
    /// 默认 60，见 rdcore-desktop `DEFAULT_FPS`）。
    fn capabilities() -> Capabilities {
        Capabilities {
            video_codecs: vec![VideoCodec::H264, VideoCodec::Raw],
            max_width: 1920,
            max_height: 1080,
            fps: 60,
            clipboard: true,
            input: InputCaps {
                mouse: true,
                keyboard: true,
                wheel: true,
            },
        }
    }
}

/// ICE 中继循环：把本地收集的候选经信令发给对端，并接收对端候选加入连接（trickle ICE）。
///
/// 候选用 JSON 整段序列化进 `IceCandidate.candidate`，以保留 `username_fragment` 等全部字段，
/// 对端再 `serde_json` 还原为 `RTCIceCandidateInit`，确保回环 / host 候选完整无损。
async fn relay_ice(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    from: [u8; 16],
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        for c in peer.drain_ice_candidates().await {
            let msg = Message::Ice(IceCandidate {
                session_id: session,
                from,
                candidate: serde_json::to_string(&c).expect("序列化 ICE 候选"),
                sdp_mid: None,
                sdp_mline_index: None,
            });
            let _ = sig.send(&msg).await;
        }
        match recv_sig_timeout(&sig).await {
            Some(Message::Ice(i)) => {
                if let Ok(init) = serde_json::from_str::<RTCIceCandidateInit>(&i.candidate) {
                    // 连接建立后到达的迟到 / 重复候选，webrtc 可能拒绝，忽略即可。
                    let _ = peer.add_ice_candidate(init).await;
                }
            }
            Some(_) => {}
            None => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 带超时的信令 recv：超时（让上层继续 drain 本地候选）或信道关闭都返回 `None`。
async fn recv_sig_timeout(sig: &SignalingClient) -> Option<Message> {
    match tokio::time::timeout(Duration::from_millis(50), sig.recv()).await {
        Ok(Ok(m)) => m,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::{aead_open, aead_seal, Ed25519CryptoProvider, SessionKey};

    #[test]
    fn app_message_seal_open_roundtrip() {
        let key = SessionKey([5u8; 32]);
        let msg = AppMessage::Heartbeat(Heartbeat {
            seq: 9,
            timestamp_ms: 1,
        });
        let bytes = postcard::to_stdvec(&msg).unwrap();
        let ct = aead_seal(&key, &bytes);
        let back = aead_open(&key, &ct).expect("解密应成功");
        let got = postcard::from_bytes::<AppMessage>(&back).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn consent_decision_is_serializable() {
        // 确保 Host→Viewer 下发的授权决定能经 postcard 往返（含 Grant 的范围集合）。
        let d = ConsentDecision::Grant {
            scopes: [ConsentScope::View, ConsentScope::Input]
                .into_iter()
                .collect(),
            duration: None,
        };
        let bytes = postcard::to_stdvec(&d).expect("ConsentDecision 必须可序列化");
        let got = postcard::from_bytes::<ConsentDecision>(&bytes).unwrap();
        assert_eq!(got, d);
        let _ = Ed25519CryptoProvider; // 仅为引用，避免未使用告警
    }

    #[test]
    fn create_pairing_returns_16_byte_session_id_and_64_hex_token() {
        // A0 回归：配对邀请格式必须钉死（见计划 §5 协调点1）。
        let p = Connection::create_pairing();
        // session_id 内部为 [u8;16]，hex 展示串 = 32 字符。
        assert_eq!(
            hex::encode(p.session_id.0).len(),
            32,
            "session_id 应展示为 32 十六进制字符"
        );
        assert_eq!(p.token.len(), 64, "一次性 token 应为 64 十六进制字符");
        assert!(
            p.token.chars().all(|c| c.is_ascii_hexdigit()),
            "token 应仅含十六进制字符"
        );
        // 两次调用应不同（密码学随机，避免可预测）。
        let p2 = Connection::create_pairing();
        assert_ne!(p.session_id, p2.session_id, "两次 session_id 应不同");
        assert_ne!(p.token, p2.token, "两次 token 应不同");
    }
}
