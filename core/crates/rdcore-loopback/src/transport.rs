//! 假传输：同一进程内、内存里把 Host 与 Viewer 两端连起来的"网络"。
//!
//! 两条 lane：
//! - **ctrl lane**：承载 `rdcore_proto::Message`（postcard 编解码），模拟信令 + 输入 +
//!   剪贴板 + 心跳等"控制面"流量（对应 WebRTC 的 WebSocket 信令与 DataChannel）。
//! - **media lane**：承载 `MediaFrame`，模拟屏幕视频流（对应 WebRTC 的 RTP video）。
//!
//! 真实网络在 P2 / P3 替换；这里的 `Endpoint` 接口（send / recv ctrl & media）与那时
//! 的传输接口保持一致，方便后续无缝替换。

use crate::traits::LoopbackError;
use crate::MediaFrame;
use rdcore_proto::{decode_limited, encode, Message, MAX_SIGNALING_MESSAGE_LEN};
use std::sync::mpsc::{channel, Receiver, Sender};

/// 回环的一端（Host 或 Viewer）。拥有发 / 收两条 lane 的能力。
pub struct Endpoint {
    ctrl_tx: Sender<Vec<u8>>,
    ctrl_rx: Receiver<Vec<u8>>,
    media_tx: Sender<MediaFrame>,
    media_rx: Receiver<MediaFrame>,
}

impl Endpoint {
    /// 发送一条控制消息（经 postcard 编码为字节）。
    pub fn send_ctrl(&self, msg: &Message) -> Result<(), LoopbackError> {
        let bytes = encode(msg)?;
        self.ctrl_tx
            .send(bytes)
            .map_err(|_| LoopbackError::Transport)
    }

    /// 接收一条控制消息（字节经 postcard 解码回 `Message`）。
    pub fn recv_ctrl(&self) -> Result<Message, LoopbackError> {
        let bytes = self.ctrl_rx.recv().map_err(|_| LoopbackError::Transport)?;
        // 接入 P0 的 F3 护栏：解码前先按 MAX_SIGNALING_MESSAGE_LEN 限长，防分配炸弹。
        Ok(decode_limited(&bytes, MAX_SIGNALING_MESSAGE_LEN)?)
    }

    /// 发送一帧编码后的视频。
    pub fn send_media(&self, frame: MediaFrame) -> Result<(), LoopbackError> {
        self.media_tx
            .send(frame)
            .map_err(|_| LoopbackError::Transport)
    }

    /// 接收一帧编码后的视频。
    pub fn recv_media(&self) -> Result<MediaFrame, LoopbackError> {
        self.media_rx.recv().map_err(|_| LoopbackError::Transport)
    }
}

/// 创建一对互联的端点：`(Host 端点, Viewer 端点)`。
///
/// 两条 lane 都交叉连接：一端 `send_*` 的东西会从另一端 `recv_*` 出来，
/// 就像一条真正的双向链路。
pub fn loopback_pair() -> (Endpoint, Endpoint) {
    let (host_ctrl_tx, viewer_ctrl_rx) = channel();
    let (viewer_ctrl_tx, host_ctrl_rx) = channel();
    let (host_media_tx, viewer_media_rx) = channel();
    let (viewer_media_tx, host_media_rx) = channel();

    let host = Endpoint {
        ctrl_tx: host_ctrl_tx,
        ctrl_rx: host_ctrl_rx,
        media_tx: host_media_tx,
        media_rx: host_media_rx,
    };
    let viewer = Endpoint {
        ctrl_tx: viewer_ctrl_tx,
        ctrl_rx: viewer_ctrl_rx,
        media_tx: viewer_media_tx,
        media_rx: viewer_media_rx,
    };
    (host, viewer)
}
