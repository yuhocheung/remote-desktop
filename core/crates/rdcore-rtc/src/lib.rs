//! rdcore-rtc — 真实 WebRTC PeerConnection 后端（P7）。
//!
//! 建立一条**真实**的 P2P WebRTC 连接（ICE / NAT 穿透 + DTLS-SRTP 加密），复用 P2 已建立的
//! 信令通道（仅传 SDP/ICE）来交换握手材料。媒体与控制流量都走 P2P，云端控制面看不到任何内容。
//!
//! # 与 `rdcore-media` 的接缝
//! P7 已在 `rdcore-media` 定义 `ByteTransport` 缝（`TcpTransport` 证明其可用）：任何字节传输
//! 后端都能驱动 `MediaChannel` / `DataChannel`。本后端把每条 WebRTC `RTCDataChannel` 包装成
//! `ByteTransport`：
//! - **`media` DataChannel** → 承载 `MediaChannel`（视频帧，沿用 `[4 字节长度][postcard]` 帧格式）。
//! - **`control` DataChannel** → 承载 `DataChannel`（输入 / 剪贴板 / 心跳）。
//!
//! 这与架构文档「媒体走 MediaChannel、控制走 DataChannel、信令只传 SDP/ICE」完全一致，且云端不可见。
//! 注：视频生产路径已迁到 RTP 轨道（见 [`video_rtp`] 模块文档）；`media` DataChannel
//! 保留为旧端（Web Viewer 等，Offer 无视频 m-line）的回退通道。
//!
//! # 握手时序（配合 `rdcore-signaling` 的 `SignalingClient`）
//! 1. Viewer：`WebRtcPeer::new` → `create_offer` 拿到 `(offer_sdp, media, control)`，
//!    把 `offer_sdp` 经信令发 Host；同时反复 `drain_ice_candidates` 把每个 ICE 经信令发 Host，
//!    直到 `ice_gathering_complete()`。
//! 2. Host：收到 `offer_sdp` → `accept_offer` 拿到 `(answer_sdp, media, control)`，把 `answer_sdp`
//!    经信令发 Viewer；同样反复 drain 自己的 ICE 候选发 Viewer。
//! 3. Viewer：收到 `answer_sdp` → `accept_answer`；双方收到对方 ICE → `add_ice_candidate`。
//! 4. 两端 `wait_data_channels_open` 等待两条通道 open，即可经 `media`/`control` 收发帧与消息。

#[cfg(not(feature = "real"))]
pub fn placeholder() {
    // 未启用 `real` feature 时仅保留占位符号，不引入 webrtc 依赖。
}

#[cfg(feature = "real")]
mod real;
#[cfg(feature = "real")]
pub use real::*;

/// 视频帧的 RTP 分片打包与抗丢包重组（gap J 生产路径）：整帧（已 AEAD + postcard 的
/// 不透明字节）经 `RtpFramePacketizer` 分片发出、`RtpFrameReassembler` 抗丢包重组，
/// 丢包只丢当前帧（编码器每帧 IDR，下一帧即恢复），消除 DataChannel 队头阻塞。
#[cfg(feature = "real")]
mod video_rtp;
#[cfg(feature = "real")]
pub use video_rtp::*;

#[cfg(feature = "real")]
pub use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

/// WebRTC 后端错误。
#[cfg(feature = "real")]
#[derive(Debug)]
pub enum RtcError {
    /// 来自 `webrtc-rs` 的底层错误（SDP 协商、ICE、数据通道等）。
    WebRtc(webrtc::Error),
    /// ICE 候选收集 / 连接建立相关的逻辑错误（预留）。
    #[allow(dead_code)]
    Ice(String),
}

#[cfg(feature = "real")]
impl std::fmt::Display for RtcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtcError::WebRtc(e) => write!(f, "webrtc error: {e}"),
            RtcError::Ice(s) => write!(f, "ice error: {s}"),
        }
    }
}

#[cfg(feature = "real")]
impl std::error::Error for RtcError {}

#[cfg(feature = "real")]
impl From<webrtc::Error> for RtcError {
    fn from(e: webrtc::Error) -> Self {
        RtcError::WebRtc(e)
    }
}
