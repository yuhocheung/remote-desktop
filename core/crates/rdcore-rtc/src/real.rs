//! 真实 WebRTC PeerConnection 后端（P7）。
//!
//! 本模块把一条真实 P2P WebRTC 连接（`webrtc-rs` 0.12）包装成 `rdcore-media` 的 `ByteTransport`
//! 缝：每条 WebRTC `RTCDataChannel` 经 `WebRtcDataChannelTransport` 成为一条"收发已帧化字节"的
//! 传输层，从而无需任何改动即可驱动上层 `SocketMediaChannel` / `SocketDataChannel`。
//!
//! 通道规划（与架构文档 §1/§5 一致）：
//! - `media` 协商通道（negotiated id=0）→ 承载 `MediaChannel`（屏幕视频帧）。
//! - `audio` 协商通道（negotiated id=2）→ 承载 `AudioChannel`（设备音频帧；与视频独立、互不阻塞）。
//! - `control` 协商通道（negotiated id=1）→ 承载 `DataChannel`（输入 / 剪贴板 / 心跳）。
//!
//! 两条都用 `negotiated` 通道：建连双方各自 `create_data_channel` 同一 `(label, id)`，无需
//! `on_data_channel` 协商回调，规避"谁先谁后"的竞态，且云端控制面永远看不到媒体或输入内容
//! （信令只传 SDP/ICE）。
//!
//! 注：`media` 也可走 DataChannel（可靠、有序、消息定界）。上层一帧字节若超过 SCTP 单条
//! 消息上限（64KiB），由 `WebRtcDataChannelTransport` 自动分片发送、对端透明重组（1 字节标签），
//! 对上层 `SocketMediaChannel` 的 `[4 字节长度][postcard]` 帧格式完全不可见。
//!
//! **视频生产路径已迁到 RTP 轨道（gap J）**：`setup_video_track` 挂载
//! `TrackLocalStaticRTP`，整帧（已 AEAD 加密、postcard 序列化的不透明字节）经
//! [`crate::video_rtp`] 分片发送、抗丢包重组——丢包只丢当前帧（编码器每帧 IDR，
//! 下一帧即恢复），不再有可靠有序 SCTP 的队头阻塞。`media` DataChannel 保留为
//! 旧端（如 Web Viewer，Offer 无视频 m-line）的回退通道。

use crate::RtcError;
use bytes::Bytes;
use rdcore_media::{SocketAudioChannel, SocketDataChannel, SocketMediaChannel, TransportError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
pub use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};

use crate::video_rtp::{
    timestamp_90khz, LatestFrameQueue, RtpFramePacketizer, RtpFrameReassembler,
};

/// 媒体通道在 SCTP 上的 negotiated id（两端必须一致）。
pub const MEDIA_CHANNEL_ID: u16 = 0;
/// 控制通道在 SCTP 上的 negotiated id（两端必须一致）。
pub const CONTROL_CHANNEL_ID: u16 = 1;
/// 音频通道在 SCTP 上的 negotiated id（两端必须一致）。
///
/// 与媒体/控制平行、互不阻塞：音频抖动或丢帧不影响视频流畅度（对应架构 §1/§5 的三独立通道）。
pub const AUDIO_CHANNEL_ID: u16 = 2;

/// 单条 SCTP 出站消息的最大载荷（含 1 字节分片标签）。
///
/// webrtc-sctp 默认 `max_message_size` 为 64KiB，超限的 `send` 直接报
/// 「outbound packet larger than maximum message size」（实测 3440x1440 的 H.264 IDR 约
/// 190KB，远超上限）。本层把大消息拆成 ≤ 16KiB 的片发送，接收端按标签重组。
const DC_CHUNK_SIZE: usize = 16 * 1024;

/// 分片标签（每条 SCTP 消息的第 1 字节，其余为载荷）：
/// 未分片整包 / 首片 / 中间片 / 末片。通道 `ordered: true` 保证按序到达。
const TAG_WHOLE: u8 = 0;
const TAG_START: u8 = 1;
const TAG_MIDDLE: u8 = 2;
const TAG_END: u8 = 3;

/// 默认 STUN 服务器（仅用于 ICE 候选收集；不传输任何媒体/输入内容）。
pub const DEFAULT_ICE_SERVERS: &[&str] = &["stun:stun.l.google.com:19302"];

/// 一个 ICE 服务器（STUN 或 TURN）。
///
/// - STUN 只需 `urls`（收集公网反射候选，穿透锥形 NAT）。
/// - TURN 还需 `username` + `credential`（对称 NAT / 严格防火墙下的中继兜底；
///   此时媒体经 TURN 服务器转发，但**仍由端到端密钥加密**，TURN 只能看到密文）。
///
/// 简写：可用 `IceServer::from("stun:host:port")`，或 `RtcConfig { ice_servers: vec!["stun:..."] }`
/// （借由 `From<&str>` / `From<String>` 自动转换）。带鉴权的 TURN 用 [`IceServer::turn`]。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IceServer {
    /// 服务器 URL 列表（同一服务器可多 transport，如 `udp` 与 `tcp`）。
    /// 形如 `stun:host:port` 或 `turn:host:port?transport=udp`。
    pub urls: Vec<String>,
    /// TURN 用户名（STUN 留 `None`）。
    pub username: Option<String>,
    /// TURN 凭据（密码或长效密钥；STUN 留 `None`）。
    pub credential: Option<String>,
}

impl IceServer {
    /// 构造一个带鉴权的 TURN 服务器（对称 NAT / 企业防火墙中继兜底）。
    pub fn turn(
        urls: impl IntoIterator<Item = impl Into<String>>,
        username: impl Into<String>,
        credential: impl Into<String>,
    ) -> Self {
        Self {
            urls: urls.into_iter().map(|u| u.into()).collect(),
            username: Some(username.into()),
            credential: Some(credential.into()),
        }
    }
}

impl From<&str> for IceServer {
    fn from(url: &str) -> Self {
        Self {
            urls: vec![url.to_string()],
            username: None,
            credential: None,
        }
    }
}

impl From<String> for IceServer {
    fn from(url: String) -> Self {
        Self {
            urls: vec![url],
            username: None,
            credential: None,
        }
    }
}

/// WebRTC 后端配置。
#[derive(Clone, Debug)]
pub struct RtcConfig {
    /// ICE 服务器（STUN/TURN）。空数组表示纯 host 候选（仅同网段/localhost 可用）。
    pub ice_servers: Vec<IceServer>,
    /// 数据通道内部缓冲大小（用于把 SCTP 消息投递给 `recv_bytes`）。
    pub channel_buffer: usize,
    /// 是否把回环网络（127.0.0.1 / ::1）纳入 ICE 候选。默认 false。
    /// 仅用于同机回环联调（CI / 本机 P2P 测试）；真实跨机连接应留 false（避免无意义的回环候选）。
    pub include_loopback: bool,
    /// 强制中继（force relay）：丢弃所有 host / srflx 本地候选，仅保留 TURN 中继候选。
    ///
    /// 用途：对称型 NAT / 严格企业防火墙下直连必然失败，必须走中继；或出于隐私/可审计
    /// 诉求希望所有媒体都经中继服务器（TURN 只见密文）。中继候选由 TURN 客户端生成、
    /// 不经本地接口采集，因此不受下方 `set_ip_filter` 影响——过滤器只丢本地接口候选。
    ///
    /// 注意：启用后**必须**在 `ice_servers` 里配置可用的 TURN，否则无候选、连接必然失败。
    pub force_relay: bool,
}

impl Default for RtcConfig {
    fn default() -> Self {
        Self {
            ice_servers: DEFAULT_ICE_SERVERS
                .iter()
                .map(|s| IceServer::from(*s))
                .collect(),
            channel_buffer: 64,
            include_loopback: false,
            force_relay: false,
        }
    }
}

impl RtcConfig {
    /// 从环境变量装配 ICE 服务器（B3 韧性面：TURN 可配置，不硬编码）。
    ///
    /// - `RDCORE_STUN`：单个 STUN URL（如 `stun:stun.example.com:3478`）。
    /// - `RDCORE_TURN_URL` / `RDCORE_TURN_USER` / `RDCORE_TURN_PASS`：TURN 中继（三者齐才启用）。
    ///   对称 NAT / 严格防火墙下，媒体经 TURN 转发但仍由端到端密钥加密，TURN 只见密文。
    ///
    /// 都未设置时回退到 [`RtcConfig::default`]（默认公共 STUN）。
    /// 生产环境 TURN 应自建（架构文档 §1），把地址/凭据注入这三个变量即可。
    pub fn from_env() -> Self {
        Self::from_env_lookup(|k| std::env::var(k).ok())
    }

    /// 与 [`RtcConfig::from_env`] 同逻辑，但来源可注入（便于单测，不依赖真实环境变量）。
    pub fn from_env_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let mut cfg = RtcConfig::default();
        let stun = get("RDCORE_STUN").filter(|s| !s.trim().is_empty());
        if let Some(url) = stun {
            cfg.ice_servers = vec![IceServer::from(url)];
        }
        let turn = (
            get("RDCORE_TURN_URL").filter(|s| !s.trim().is_empty()),
            get("RDCORE_TURN_USER"),
            get("RDCORE_TURN_PASS"),
        );
        if let (Some(url), Some(user), Some(pass)) = turn {
            cfg.ice_servers.push(IceServer::turn([url], user, pass));
        }
        cfg
    }
}

/// 把 WebRTC `RTCDataChannel` 包装成 `ByteTransport`。
///
/// - `send_bytes`：把已经过 `[4 字节长度][postcard]` 帧化的字节整体作为一条 SCTP 消息发出
///   （DataChannel 是消息定界的，所以单条消息即一帧，无需额外分片）。
/// - `recv_bytes`：由 `on_message` 回调把收到的每条 SCTP 消息投入 mpsc，供上层取出。
///
/// 通道在 `wait_data_channels_open` 之前不会真正收发；`send_bytes` 在通道未 open 时会被
/// webrtc 拒绝，本实现统一映射为 `TransportError::Closed`。
#[derive(Clone)]
pub struct WebRtcDataChannelTransport {
    dc: Arc<RTCDataChannel>,
    rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    /// 分片重组缓冲（跨 `recv_bytes` 调用累积 START..END 之间的 MIDDLE 片）。
    /// 克隆体共享同一缓冲，与 `rx` 共享语义一致（单消费者）。
    partial: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl WebRtcDataChannelTransport {
    /// 用一条已创建的 `RTCDataChannel` 构造传输层，并安装 `on_message` 收包回调。
    fn new(dc: Arc<RTCDataChannel>, buffer: usize) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(buffer.max(1));
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let tx = tx.clone();
            Box::pin(async move {
                // 无论 is_string 与否，媒体/控制帧都是二进制字节，直接透传 data。
                let _ = tx.send(msg.data.to_vec()).await;
            })
        }));
        Self {
            dc,
            rx: Arc::new(Mutex::new(rx)),
            partial: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 单条 SCTP 消息的发送上限：对端死透（iOS 被杀 / 断网）后 SCTP 发送缓冲永不排空，
    /// `dc.send` 会无限阻塞——曾把 Host 媒体泵楔死在 `pump.stop()`、导致无法再接受新
    /// Viewer（必须重启 Host）。超时按通道关闭上报，让上层丢帧/收束，保证停止与重连可达。
    const SEND_TIMEOUT: Duration = Duration::from_secs(5);

    /// 发一条带分片标签的 SCTP 消息（≤ DC_CHUNK_SIZE）。
    async fn send_tagged(&self, tag: u8, payload: &[u8]) -> Result<(), TransportError> {
        let mut msg = Vec::with_capacity(payload.len() + 1);
        msg.push(tag);
        msg.extend_from_slice(payload);
        let len = msg.len();
        let msg = Bytes::from(msg);
        match tokio::time::timeout(Self::SEND_TIMEOUT, self.dc.send(&msg)).await {
            Ok(r) => r.map(|_| ()).map_err(|e| {
                eprintln!(
                    "[rtc] data channel send failed ({len} bytes, state={:?}): {e}",
                    self.dc.ready_state()
                );
                TransportError::Closed
            }),
            Err(_) => {
                eprintln!(
                    "[rtc] data channel send timeout ({}s, {len} bytes, state={:?})",
                    Self::SEND_TIMEOUT.as_secs(),
                    self.dc.ready_state()
                );
                Err(TransportError::Closed)
            }
        }
    }
}

impl rdcore_media::ByteTransport for WebRtcDataChannelTransport {
    async fn send_bytes(&self, data: Vec<u8>) -> Result<(), TransportError> {
        // 小于单片上限：一条消息发完；否则拆成 START..MIDDLE..END 多片（SCTP 消息定界、有序）。
        if data.len() < DC_CHUNK_SIZE {
            return self.send_tagged(TAG_WHOLE, &data).await;
        }
        let payload = DC_CHUNK_SIZE - 1;
        let n = data.len().div_ceil(payload);
        for (i, chunk) in data.chunks(payload).enumerate() {
            let tag = if i == 0 {
                TAG_START
            } else if i + 1 == n {
                TAG_END
            } else {
                TAG_MIDDLE
            };
            self.send_tagged(tag, chunk).await?;
        }
        Ok(())
    }

    async fn recv_bytes(&self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut rx = self.rx.lock().await;
        loop {
            let Some(msg) = rx.recv().await else {
                return Ok(None);
            };
            let Some((&tag, payload)) = msg.split_first() else {
                continue; // 空消息无标签，忽略
            };
            match tag {
                TAG_WHOLE => return Ok(Some(payload.to_vec())),
                TAG_START => {
                    let mut p = self.partial.lock().unwrap();
                    p.clear();
                    p.extend_from_slice(payload);
                }
                TAG_MIDDLE => self.partial.lock().unwrap().extend_from_slice(payload),
                TAG_END => {
                    let mut p = self.partial.lock().unwrap();
                    p.extend_from_slice(payload);
                    return Ok(Some(std::mem::take(&mut *p)));
                }
                _ => {} // 未知标签：丢弃，等下一条
            }
        }
    }
}

/// RTP 视频轨道的接收端句柄（克隆安全）。
///
/// 由 [`WebRtcPeer::video_receiver`] 取得；`recv` 异步取出远端经 RTP 轨道推送、并由本端
/// 抗丢包重组后的**整帧字节**（postcard 序列化的 `MediaFrame`，其像素字段已端到端
/// AEAD 加密——本层不感知内容，解密与反序列化在上层 `rdcore-app`）。多个句柄共享同一个
/// 底层最新帧队列（但同一时刻只应有一个消费者在 `recv`，否则帧会被分散到不同消费者）。
#[derive(Clone)]
pub struct VideoReceiver {
    rx: Arc<LatestFrameQueue>,
}

impl VideoReceiver {
    /// 取出下一帧完整重组的字节；轨道关闭 / 连接断开后返回 `None`。
    ///
    /// 注意：丢包导致的残缺帧在本层已被静默丢弃（不会产出半帧），`recv` 只在
    /// 拼出完整帧时返回——调用方看到的「帧间隔」天然吸收网络抖动。队列为直播
    /// 语义（满载丢最旧），消费慢时自动跳过中间陈帧、优先拿到最新画面。
    pub async fn recv(&self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

/// 一条真实 WebRTC PeerConnection 的句柄。
///
/// 持有 `RTCPeerConnection` 与两条 negotiated 数据通道，并提供"信令无关"的握手原语
/// （`create_offer` / `accept_offer` / `accept_answer` / `add_ice_candidate` / 候选收集 /
/// 通道 open 等待）。握手材料（SDP/ICE）经由你自己的信令通道传输；本结构本身不接触网络之外的任何东西。
///
/// 视频走 H.264 **RTP 轨道**（gap J 生产路径）：`setup_video_track` 注册
/// `TrackLocalStaticRTP` 并安装 `on_track` 重组；整帧字节（上层已 AEAD + postcard 的
/// 不透明载荷）经 [`RtpFramePacketizer`] 分片发出、对端 [`RtpFrameReassembler`] 抗丢包
/// 重组。该轨道默认不创建（保持向后兼容：`channels()` 的 `media` DataChannel 仍可用作
/// 旧端回退），需显式 `setup_video_track` 才启用；两端是否启用由 Offer/Answer SDP 中的
/// 视频 m-line 决定（见 [`crate::sdp_has_active_video`]）。
pub struct WebRtcPeer {
    pc: Arc<RTCPeerConnection>,
    media_dc: Arc<RTCDataChannel>,
    control_dc: Arc<RTCDataChannel>,
    audio_dc: Arc<RTCDataChannel>,
    ice_rx: Arc<Mutex<mpsc::Receiver<RTCIceCandidateInit>>>,
    /// 早于 remote description 到达、暂不能加入连接的 ICE 候选缓冲（trickle ICE 常见时序）。
    /// 在 `accept_offer`/`accept_answer` 设置完 remote description 后由 `flush_pending_candidates` 刷入。
    pending_candidates: Arc<Mutex<Vec<RTCIceCandidateInit>>>,
    open_count: Arc<AtomicUsize>,
    open_notify: Arc<Notify>,
    media_transport: WebRtcDataChannelTransport,
    control_transport: WebRtcDataChannelTransport,
    audio_transport: WebRtcDataChannelTransport,
    // ---- gap J：RTP 视频轨道（可选，setup_video_track 前均为 None）----
    /// 本端向对端推送视频用的 `TrackLocalStaticRTP`（逐包写入已分片的 RTP）。
    video_track: Mutex<Option<Arc<TrackLocalStaticRTP>>>,
    /// 发送侧分片器（序列号跨帧连续；`Mutex` 提供 `&self` 可变）。
    video_packetizer: Mutex<RtpFramePacketizer>,
    /// 远端轨道重组出的整帧字节队列（on_track 任务生产、`VideoReceiver` 消费；
    /// 直播语义：满载丢最旧帧）。
    video_q: Mutex<Option<Arc<LatestFrameQueue>>>,
    /// `setup_video_track` 是否已执行（幂等守卫）。
    video_setup: AtomicBool,
    /// 关键帧请求标志（PLI/FIR 的本地承接钩子；见 `request_keyframe`）。
    need_keyframe: AtomicBool,
}

impl WebRtcPeer {
    /// 用默认配置（`RtcConfig::default`）创建一条 PeerConnection 与两条 negotiated 数据通道。
    pub async fn new() -> Result<Self, RtcError> {
        Self::with_config(RtcConfig::default()).await
    }

    /// 用自定义配置创建 PeerConnection 与两条 negotiated 数据通道，并安装
    /// ICE 候选与 open 回调。此时尚未交换任何 SDP，可安全在 `create_offer`/`accept_offer` 前调用。
    pub async fn with_config(cfg: RtcConfig) -> Result<Self, RtcError> {
        // 进程级 rustls CryptoProvider 锁定为 ring：workspace 内 aws-lc-rs（gateway）与
        // ring（webrtc）可能并存，0.23 无法自动判定时 DTLS 建连直接 panic。此处为全部
        // 下游（desktop / FFI / viewer-cli）统一的安装点；已安装时 install_default 返回
        // Err，属正常，忽略即可。
        let _ = rustls::crypto::ring::default_provider().install_default();
        // 媒体引擎 + 默认拦截器（即使只用数据通道，webrtc-rs 也要求注册默认拦截器栈）。
        let mut me = MediaEngine::default();
        // gap J：注册 H.264 视频编码（payload type 96, 90kHz），供 `setup_video_track` 的
        // RTP 视频轨道使用。仅注册编解码器不会自动新增 m-line；只有 `add_track` 后 SDP 才含视频段。
        let h264_params = RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_string(),
                clock_rate: 90_000,
                channels: 0,
                // packetization-mode=1（单一 NALU / FU-A，非 STAP-A 聚合）；profile-level-id
                // 取基线 42e01f（与 openh264 默认产出兼容）。
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 96,
            stats_id: String::new(),
        };
        me.register_codec(h264_params, RTPCodecType::Video)
            .map_err(|e| RtcError::Ice(format!("注册 H.264 编解码器失败: {e}")))?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut me)?;

        let mut api_builder = APIBuilder::new()
            .with_media_engine(me)
            .with_interceptor_registry(registry);

        // 回环联调：让两对等端同机时仅靠 host 候选即可在 localhost 上完成真实 P2P（无需 STUN/TURN）。
        // 强制中继：丢弃本地 host/srflx 候选，仅留 TURN relay 候选（详见 RtcConfig::force_relay）。
        if cfg.include_loopback || cfg.force_relay {
            use webrtc::api::setting_engine::SettingEngine;
            use webrtc::ice::mdns::MulticastDnsMode;
            let mut se = SettingEngine::default();
            if cfg.include_loopback {
                // 关键：localhost 联调必须关掉 mDNS，否则候选是 `.local` 主机名，同机无法直连。
                se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
                // 把 127.0.0.1 / ::1 回环候选也纳入 ICE。
                se.set_include_loopback_candidate(true);
            }
            if cfg.force_relay {
                // 丢弃所有 host/srflx 本地候选（其地址必为回环/私网），仅保留 TURN 中继候选。
                // 中继候选由 TURN 客户端生成、不经本地接口采集，故不受此过滤器影响。
                // `IpFilterFn` 是 `Box<dyn Fn(IpAddr) -> bool + Send + Sync>`；返回 false = 丢弃。
                // `is_private` 只在 `Ipv4Addr` 上，`IpAddr` 需分派；IPv6 用 ULA(fc00::/7) 近似私网。
                se.set_ip_filter(Box::new(|ip: std::net::IpAddr| {
                    let is_private = match ip {
                        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
                        std::net::IpAddr::V6(v6) => {
                            // ULA fc00::/7 或 link-local fe80::/10 视为私网。
                            (v6.segments()[0] & 0xfe00) == 0xfc00
                                || (v6.segments()[0] & 0xffc0) == 0xfe80
                        }
                    };
                    !(ip.is_loopback() || is_private)
                }));
            }
            api_builder = api_builder.with_setting_engine(se);
        }

        let api = api_builder.build();

        let ice_servers = cfg
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();
        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };
        let pc = Arc::new(api.new_peer_connection(config).await?);

        // ICE 候选收集：回调把每个候选转成 `RTCIceCandidateInit` 投入 mpsc，供信令层 drain。
        let (ice_tx, ice_rx) = mpsc::channel::<RTCIceCandidateInit>(32);
        {
            let ice_tx = ice_tx.clone();
            pc.on_ice_candidate(Box::new(
                move |c: Option<webrtc::ice_transport::ice_candidate::RTCIceCandidate>| {
                    let ice_tx = ice_tx.clone();
                    Box::pin(async move {
                        if let Some(c) = c {
                            if let Ok(init) = c.to_json() {
                                let _ = ice_tx.send(init).await;
                            }
                        }
                    })
                },
            ));
        }

        // 两条 negotiated 数据通道：两端各自 create_data_channel 同一 (label, id)。
        let media_dc = pc
            .create_data_channel(
                "media",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    // 对 negotiated 通道，`negotiated` 字段直接承载通道 id（无独立 `id` 字段）；
                    // 两端各自 create_data_channel 同一 (label, id)，无需 on_data_channel 协商。
                    negotiated: Some(MEDIA_CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await?;
        let control_dc = pc
            .create_data_channel(
                "control",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    negotiated: Some(CONTROL_CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await?;
        // 音频通道（negotiated id=2）：与媒体/控制平行、互不阻塞。
        let audio_dc = pc
            .create_data_channel(
                "audio",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    negotiated: Some(AUDIO_CHANNEL_ID),
                    ..Default::default()
                }),
            )
            .await?;

        // open 计数 + 通知：三条通道都 open 后 `wait_data_channels_open` 才返回。
        let open_count = Arc::new(AtomicUsize::new(0));
        let open_notify = Arc::new(Notify::new());
        for dc in [&media_dc, &control_dc, &audio_dc] {
            let open_count = open_count.clone();
            let open_notify = open_notify.clone();
            dc.on_open(Box::new(move || {
                open_count.fetch_add(1, Ordering::SeqCst);
                open_notify.notify_one();
                Box::pin(async {})
            }));
        }

        let buffer = cfg.channel_buffer;
        let media_transport = WebRtcDataChannelTransport::new(media_dc.clone(), buffer);
        let control_transport = WebRtcDataChannelTransport::new(control_dc.clone(), buffer);
        let audio_transport = WebRtcDataChannelTransport::new(audio_dc.clone(), buffer);

        Ok(Self {
            pc,
            media_dc,
            control_dc,
            audio_dc,
            ice_rx: Arc::new(Mutex::new(ice_rx)),
            pending_candidates: Arc::new(Mutex::new(Vec::new())),
            open_count,
            open_notify,
            media_transport,
            control_transport,
            audio_transport,
            video_track: Mutex::new(None),
            video_packetizer: Mutex::new(RtpFramePacketizer::new()),
            video_q: Mutex::new(None),
            video_setup: AtomicBool::new(false),
            need_keyframe: AtomicBool::new(false),
        })
    }

    /// Viewer 侧：生成本地 Offer 并设为 local description，返回 SDP 文本（经信令发对端）。
    ///
    /// 调用后应开始 drain `drain_ice_candidates` 把 ICE 候选经信令发对端。
    pub async fn create_offer(&self) -> Result<String, RtcError> {
        let offer = self.pc.create_offer(None).await?;
        let sdp = offer.sdp.clone();
        self.pc.set_local_description(offer).await?;
        Ok(sdp)
    }

    /// Host 侧：收到对端 Offer，设为 remote description，生成本地 Answer 并设为 local description，
    /// 返回 Answer 的 SDP 文本（经信令发回 Viewer）。
    ///
    /// 调用后应开始 drain `drain_ice_candidates` 把 ICE 候选经信令发对端。
    pub async fn accept_offer(&self, sdp: String) -> Result<String, RtcError> {
        let offer = RTCSessionDescription::offer(sdp)?;
        self.pc.set_remote_description(offer).await?;
        let answer = self.pc.create_answer(None).await?;
        let sdp = answer.sdp.clone();
        self.pc.set_local_description(answer).await?;
        // remote description 已就绪，刷入早于它到达而缓冲的候选。
        self.flush_pending_candidates().await;
        Ok(sdp)
    }

    /// Viewer 侧：收到对端 Answer，设为 remote description。之后只需继续交换 ICE 候选。
    pub async fn accept_answer(&self, sdp: String) -> Result<(), RtcError> {
        let answer = RTCSessionDescription::answer(sdp)?;
        self.pc.set_remote_description(answer).await?;
        // remote description 已就绪，刷入早于它到达而缓冲的候选。
        self.flush_pending_candidates().await;
        Ok(())
    }

    /// 收到对端的一个 ICE 候选，加入连接。
    ///
    /// 若 remote description 尚未 set（trickle ICE 中候选早于 SDP 到达的常见情况），
    /// webrtc-rs 会报错；本方法不报错，而是把候选**缓冲**起来，待远端描述就绪后由
    /// `accept_offer`/`accept_answer` 自动刷入。这样无论信令层以何种顺序投递 SDP/ICE，
    /// 连接都能正确建立。
    pub async fn add_ice_candidate(&self, cand: RTCIceCandidateInit) -> Result<(), RtcError> {
        match self.pc.add_ice_candidate(cand.clone()).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // remote description 未就绪，缓冲后等待 flush。
                self.pending_candidates.lock().await.push(cand);
                Ok(())
            }
        }
    }

    /// 把缓冲的 ICE 候选（早于 remote description 到达的）加入连接。
    /// 仅在 `accept_offer`/`accept_answer` 设置完 remote description 后调用。
    async fn flush_pending_candidates(&self) {
        let mut pending = self.pending_candidates.lock().await;
        if pending.is_empty() {
            return;
        }
        let buffered = std::mem::take(&mut *pending);
        drop(pending);
        for c in buffered {
            // 刷入时 remote description 已就绪；仍失败的（重复/迟到候选）直接忽略。
            let _ = self.pc.add_ice_candidate(c).await;
        }
    }

    /// 非阻塞 drain 当前已收集到的全部 ICE 候选（trickle ICE 用）。
    ///
    /// 信令层应在 `create_offer`/`accept_offer` 后反复调用本方法，直到
    /// `ice_gathering_complete()` 为真，把所有候选经信令发对端。
    pub async fn drain_ice_candidates(&self) -> Vec<RTCIceCandidateInit> {
        let mut rx = self.ice_rx.lock().await;
        let mut out = Vec::new();
        while let Ok(c) = rx.try_recv() {
            out.push(c);
        }
        out
    }

    /// ICE 候选收集是否已结束（gathering state == Complete）。
    pub fn ice_gathering_complete(&self) -> bool {
        self.pc.ice_gathering_state() == RTCIceGatheringState::Complete
    }

    /// 等待两条 negotiated 数据通道都 open（ICE+DTLS 成功后才 open）。
    /// open 前 `send_bytes` 会被 webrtc 拒绝。
    pub async fn wait_data_channels_open(&self) {
        // 媒体 + 控制 + 音频 三条 negotiated 数据通道都 open 后才算就绪。
        while self.open_count.load(Ordering::SeqCst) < 3 {
            self.open_notify.notified().await;
        }
    }

    /// 取媒体通道（视频帧）。建议先 `wait_data_channels_open` 再收发。
    pub fn media_channel(&self) -> SocketMediaChannel<WebRtcDataChannelTransport> {
        SocketMediaChannel::new(self.media_transport.clone())
    }

    /// 取控制通道（输入 / 剪贴板 / 心跳）。建议先 `wait_data_channels_open` 再收发。
    pub fn data_channel(&self) -> SocketDataChannel<WebRtcDataChannelTransport> {
        SocketDataChannel::new(self.control_transport.clone())
    }

    /// 音频通道：承载设备音频帧（`AudioChannel`）。
    ///
    /// 与 `media_channel` / `data_channel` 平行、互不阻塞；经第三条 negotiated DataChannel
    /// （id=2）传输。音频丢帧/抖动不影响视频流畅度。
    pub fn audio_channel(&self) -> SocketAudioChannel<WebRtcDataChannelTransport> {
        SocketAudioChannel::new(self.audio_transport.clone())
    }

    /// 一次性取出媒体 + 控制两条通道（推荐入口）。
    pub fn channels(
        &self,
    ) -> (
        SocketMediaChannel<WebRtcDataChannelTransport>,
        SocketDataChannel<WebRtcDataChannelTransport>,
    ) {
        (self.media_channel(), self.data_channel())
    }

    /// 暴露底层 `RTCPeerConnection`，便于上层查询连接状态（如 `connection_state`）。
    pub fn peer_connection(&self) -> &Arc<RTCPeerConnection> {
        &self.pc
    }

    /// 当前 PeerConnection 连接状态
    ///（`New` / `Connecting` / `Connected` / `Disconnected` / `Failed` / `Closed`）。
    /// 供上层安全指示器 / 横幅反映"是否已真正建立 P2P 连接"。
    ///
    /// 注意：`wait_data_channels_open` 返回时通常已 `Connected`，但 `Connected` 之后仍可能因
    /// 网络切换进入 `Disconnected` 再恢复；生产 UI 应订阅此状态做重连提示。
    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.pc.connection_state()
    }

    /// 暴露底层媒体数据通道（高级用途：直接读取 SCTP 统计等）。
    pub fn media_data_channel(&self) -> &Arc<RTCDataChannel> {
        &self.media_dc
    }

    // ===================== gap J：H.264 RTP 视频轨道 =====================
    //
    // 把视频从「可靠有序 DataChannel 消息」升级为真正的 RTP 视频轨道：发送端把整帧
    // （上层已 AEAD + postcard 的不透明字节）经 [`RtpFramePacketizer`] 切成连续 RTP 包，
    // 逐包 `TrackLocalStaticRTP::write_rtp` 发出；接收端在 `on_track` 里用
    // [`RtpFrameReassembler`] 抗丢包重组为整帧字节。载荷不透明（不解析 NAL），因此
    // 应用层端到端加密与 RTP 传输互不干扰。丢包只丢当前帧——编码器每帧强制 IDR，
    // 下一帧即恢复，无需可靠传输，也消除了 SCTP 队头阻塞导致的画面卡顿/延迟累积。
    // 本轨道默认不创建，需显式 `setup_video_track`，以兼容旧端（Web Viewer 等）
    // 的 `channels()`（DataChannel 媒体）回退路径。

    /// 注册 H.264 RTP 视频轨道。必须在 `create_offer` / `accept_offer` **之前**调用，
    /// 且连接两端都应调用（Host 调用后 `push_video_frame` 才有效；Viewer 调用后才能经
    /// `video_receiver` 收帧）。幂等。
    ///
    /// 不需要分辨率参数：整帧字节内含 postcard `MediaFrame` 元数据（宽/高/编码），
    /// 由上层反序列化取得——RTP 层只搬字节，不感知内容。
    pub async fn setup_video_track(&self) -> Result<(), RtcError> {
        if self.video_setup.load(Ordering::SeqCst) {
            return Ok(());
        }
        let cap = RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_string(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_string(),
            rtcp_feedback: vec![],
        };
        let track = Arc::new(TrackLocalStaticRTP::new(
            cap,
            "rdcore-video".to_string(),
            "rdcore".to_string(),
        ));
        // 加入轨道 → SDP offer/answer 将含一条 H.264 视频 m-line。
        let track_for_add: Arc<dyn TrackLocal + Send + Sync> = track.clone();
        self.pc.add_track(track_for_add).await?;

        // 容量取小：视频是实时流，积压 = 延迟。8 帧在 60fps 下约 133ms 缓冲上限；
        // 队列满载时丢**最旧**帧（LatestFrameQueue 直播语义），消费者永远先拿最新画面。
        let q = Arc::new(LatestFrameQueue::new(8));

        // 接收端：远端轨道的逐包 RTP → RtpFrameReassembler 抗丢包重组 → 投进最新帧队列。
        // 重组器对缺口/乱序/迟到包一律丢帧处理，绝不产出半帧、绝不阻塞后续帧。
        let q_producer = q.clone();
        self.pc.on_track(Box::new(move |track, _rcvr, _trans| {
            let mut reasm = RtpFrameReassembler::new();
            let q = q_producer.clone();
            let mut evicted = 0u64;
            Box::pin(async move {
                while let Ok((pkt, _)) = track.read_rtp().await {
                    if let Some(frame_bytes) = reasm.push(&pkt) {
                        if q.push(frame_bytes) {
                            evicted += 1;
                            if evicted == 1 || evicted % 100 == 0 {
                                eprintln!(
                                    "[rtc] video rx queue full, evicted {evicted} stale frames（消费端跟不上，丢旧保新）"
                                );
                            }
                        }
                    }
                }
                // 轨道结束（连接断开 / 远端停发）：关闭队列，让所有 recv 立即返回 None。
                q.close();
            })
        }));

        *self.video_track.lock().await = Some(track);
        *self.video_q.lock().await = Some(q);
        self.video_setup.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// 取本端视频轨道（推送用）。未 `setup_video_track` 时返回 `None`。
    pub async fn video_track(&self) -> Option<Arc<TrackLocalStaticRTP>> {
        self.video_track.lock().await.clone()
    }

    /// 取视频接收端句柄（收帧用）。未 `setup_video_track` 时返回 `None`。
    pub async fn video_receiver(&self) -> Option<VideoReceiver> {
        self.video_q
            .lock()
            .await
            .clone()
            .map(|rx| VideoReceiver { rx })
    }

    /// 推送一帧完整字节（postcard `MediaFrame`，像素已端到端 AEAD）到 RTP 视频轨道
    ///（Host→Viewer）。内部按 [`RtpFramePacketizer`] 分片逐包发出；须先 `setup_video_track`。
    ///
    /// RTP 传输不保证送达：丢包由对端重组器丢帧处理。本方法把整帧交给内核即返回，
    /// 不会像 DataChannel 那样因发送缓冲满而阻塞——这正是消除画面延迟累积的关键。
    pub async fn push_video_frame(&self, payload: &[u8]) -> Result<(), RtcError> {
        let track = self.video_track.lock().await.clone();
        let track = match track {
            Some(t) => t,
            None => {
                return Err(RtcError::Ice(
                    "video track 未初始化：请先调用 setup_video_track".into(),
                ))
            }
        };
        // PLI/FIR 钩子：若被请求关键帧，消费标志位。本系统编码器默认每帧强制 IDR，
        // 关键帧天然满足；此处仅清标志，未来若改为按需 IDR 则在此触发编码器。
        if self.need_keyframe.load(Ordering::SeqCst) {
            self.need_keyframe.store(false, Ordering::SeqCst);
        }
        let ts = timestamp_90khz();
        let packets = self.video_packetizer.lock().await.packetize(payload, ts);
        for pkt in packets {
            track.write_rtp(&pkt).await.map_err(RtcError::WebRtc)?;
        }
        Ok(())
    }

    /// 请求下一帧为关键帧（IDR），对应标准 PLI / FIR 反馈。
    ///
    /// webrtc-rs 0.12 的 `RTCRtpSender` 未公开 `on_rtcp`，故由上层在检测到需重传关键帧
    /// （如新 Viewer 加入、长丢包后）时显式调用；本系统编码器已默认每帧强制 IDR，
    /// 调用即满足（见 [`WebRtcPeer::push_video_frame`]）。BWE→码率（REMB/TWCC）同理，
    /// 待 webrtc-rs 暴露 RTCP 反馈钩子后在 `push_video_frame` 旁路接入编码器。
    pub fn request_keyframe(&self) {
        self.need_keyframe.store(true, Ordering::SeqCst);
    }

    /// 暴露底层控制数据通道。
    pub fn control_data_channel(&self) -> &Arc<RTCDataChannel> {
        &self.control_dc
    }

    /// 暴露底层音频数据通道。
    pub fn audio_data_channel(&self) -> &Arc<RTCDataChannel> {
        &self.audio_dc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_satisfies_bytetransport_bound() {
        // 编译期校验：WebRtcDataChannelTransport 必须实现 ByteTransport（Send + Sync）。
        fn assert_send_sync<T: rdcore_media::ByteTransport + Send + Sync>() {}
        assert_send_sync::<WebRtcDataChannelTransport>();
    }

    #[test]
    fn default_config_has_stun() {
        let cfg = RtcConfig::default();
        assert_eq!(
            cfg.ice_servers,
            vec![IceServer::from("stun:stun.l.google.com:19302")]
        );
        assert!(cfg.channel_buffer >= 1);
    }

    #[test]
    fn turn_server_carries_credentials() {
        let s = IceServer::turn(
            ["turn:turn.example.com:3478?transport=udp"],
            "alice",
            "s3cr3t",
        );
        assert_eq!(
            s.urls,
            vec!["turn:turn.example.com:3478?transport=udp".to_string()]
        );
        assert_eq!(s.username.as_deref(), Some("alice"));
        assert_eq!(s.credential.as_deref(), Some("s3cr3t"));
        // STUN 简写仍走 From<&str>，无凭据。
        let stun = IceServer::from("stun:stun.l.google.com:19302");
        assert!(stun.username.is_none());
        assert!(stun.credential.is_none());
    }

    #[test]
    fn channel_ids_are_distinct() {
        assert_ne!(MEDIA_CHANNEL_ID, CONTROL_CHANNEL_ID);
    }

    #[test]
    fn from_env_defaults_to_public_stun_when_unset() {
        let cfg = RtcConfig::from_env_lookup(|_| None);
        assert_eq!(
            cfg.ice_servers,
            vec![IceServer::from("stun:stun.l.google.com:19302")]
        );
    }

    #[test]
    fn from_env_adds_turn_when_all_three_set() {
        let get = |k: &str| match k {
            "RDCORE_TURN_URL" => Some("turn:turn.example.com:3478?transport=udp".to_string()),
            "RDCORE_TURN_USER" => Some("alice".to_string()),
            "RDCORE_TURN_PASS" => Some("s3cr3t".to_string()),
            _ => None,
        };
        let cfg = RtcConfig::from_env_lookup(get);
        // 默认 STUN 保留，TURN 追加为兜底中继。
        assert!(cfg
            .ice_servers
            .contains(&IceServer::from("stun:stun.l.google.com:19302")));
        let turn = cfg
            .ice_servers
            .iter()
            .find(|s| s.username.is_some())
            .expect("应包含带凭据的 TURN");
        assert_eq!(turn.username.as_deref(), Some("alice"));
        assert_eq!(turn.credential.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn from_env_stun_override_replaces_default() {
        let get = |k: &str| match k {
            "RDCORE_STUN" => Some("stun:stun.example.com:3478".to_string()),
            _ => None,
        };
        let cfg = RtcConfig::from_env_lookup(get);
        assert_eq!(
            cfg.ice_servers,
            vec![IceServer::from("stun:stun.example.com:3478")]
        );
    }

    #[test]
    fn from_env_ignores_incomplete_turn() {
        // 只给 URL 不给凭据：不启用 TURN（避免半配置静默失效）。
        let get = |k: &str| match k {
            "RDCORE_TURN_URL" => Some("turn:turn.example.com:3478".to_string()),
            _ => None,
        };
        let cfg = RtcConfig::from_env_lookup(get);
        assert!(cfg.ice_servers.iter().all(|s| s.username.is_none()));
    }

    #[test]
    fn video_track_and_receiver_are_send_sync() {
        // 编译期校验：视频轨道与接收端句柄可跨线程使用（在 on_track 任务 / 跨 await 中使用）。
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<TrackLocalStaticRTP>>();
        assert_send_sync::<VideoReceiver>();
    }

    #[tokio::test]
    async fn setup_video_track_registers_and_is_idempotent() {
        // 无需网络：add_track / on_track 均为本地操作，验证轨道接线可运行且不破坏 PC。
        let peer = WebRtcPeer::with_config(RtcConfig::default())
            .await
            .expect("创建 PeerConnection");
        peer.setup_video_track().await.expect("setup 视频轨道");
        assert!(
            peer.video_track().await.is_some(),
            "setup 后 video_track 应可用"
        );
        assert!(
            peer.video_receiver().await.is_some(),
            "setup 后 video_receiver 应可用"
        );
        // 二次调用应幂等（不报错、不产生第二条轨道导致 SDP 异常）。
        peer.setup_video_track().await.expect("重复 setup 应幂等");
    }

    #[tokio::test]
    async fn push_video_frame_rejects_without_setup() {
        // 未 setup 时 push 应报错，而非静默丢帧。
        let peer = WebRtcPeer::with_config(RtcConfig::default())
            .await
            .expect("创建 PeerConnection");
        let err = peer.push_video_frame(&[0x65, 0x01, 0x02, 0x03]).await;
        assert!(err.is_err(), "未 setup 时 push_video_frame 应返回 Err");
    }
}
