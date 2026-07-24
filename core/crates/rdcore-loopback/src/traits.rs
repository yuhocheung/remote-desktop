//! 管线各环节的 trait 边界 —— 与 P3 真实后端对接的契约。
//!
//! P1 只给"假实现"（见 `fake.rs`）。后续 P3 用真实平台 / 编解码后端替换实现时，
//! 只要满足这里的 trait，管线和传输层一行都不用改。

use rdcore_proto::{InputEvent, MediaFrame};
use std::fmt;

/// 一帧未编码的屏幕画面（RGBA8888，逐行紧密排列）。
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// 像素数据，长度应为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl Frame {
    /// 缓冲区应有的字节数（宽 × 高 × 4）。
    pub fn byte_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// 捕获源：持续产出屏幕帧。返回 `None` 表示没有更多帧（源结束）。
pub trait FrameSource {
    fn next_frame(&mut self) -> Option<Frame>;
}

/// 编码器：把 `Frame` 变成可传输的 `MediaFrame`。
pub trait FrameEncoder {
    fn encode(&self, frame: &Frame) -> Result<MediaFrame, LoopbackError>;
}

/// 解码器：把 `MediaFrame` 还原成 `Frame`。
pub trait FrameDecoder {
    fn decode(&self, media: &MediaFrame) -> Result<Frame, LoopbackError>;
}

/// 渲染落点：收到解码后的帧并"呈现"（P1 里只是存起来供断言）。
pub trait FrameSink {
    fn present(&mut self, frame: &Frame);
}

/// 输入源：持续产出输入事件（Viewer 侧捕获键鼠）。
pub trait InputSource {
    fn next_input(&mut self) -> Option<InputEvent>;
}

/// 输入注入器：在 Host 侧把收到的输入事件"注入"系统（P1 里只是记录下来）。
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent);
}

/// P1 回环过程中可能出现的错误（传输 / 编解码 / 协议）。
#[derive(Debug, Clone, PartialEq)]
pub enum LoopbackError {
    /// 协议层错误（来自 rdcore_proto 的 encode / decode）。
    Protocol(rdcore_proto::ProtocolError),
    /// 编解码错误（如数据长度不匹配、codec 不支持）。
    Codec(String),
    /// 传输通道已断开（对端 Dropped）。
    Transport,
}

impl fmt::Display for LoopbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopbackError::Protocol(e) => write!(f, "protocol error: {}", e),
            LoopbackError::Codec(s) => write!(f, "codec error: {}", s),
            LoopbackError::Transport => write!(f, "transport closed"),
        }
    }
}

impl std::error::Error for LoopbackError {}

impl From<rdcore_proto::ProtocolError> for LoopbackError {
    fn from(e: rdcore_proto::ProtocolError) -> Self {
        LoopbackError::Protocol(e)
    }
}
