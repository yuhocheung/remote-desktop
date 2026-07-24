//! rdcore-loopback — P1 本地回环（验证整条管线 + P0 契约）。
//!
//! 在单个进程里，用内存中的"假传输"把两端（Host / Viewer）连起来，
//! 用"假后端"（合成捕获、Raw 编码、缓冲渲染、脚本输入、录制注入）跑通：
//!
//! ```text
//! Host:   FrameSource → FrameEncoder ──(media lane)──► Viewer: FrameDecoder → FrameSink
//! Viewer: InputSource ──(ctrl lane, Message)──► Host: InputInjector
//! ```
//!
//! - **ctrl lane** 承载 `rdcore_proto::Message`（postcard 编解码），模拟信令 + 输入 +
//!   剪贴板 + 心跳这类"控制面"流量（对应 WebRTC 的 WebSocket 信令与 DataChannel）。
//! - **media lane** 承载 `MediaFrame`，模拟屏幕视频流（对应 WebRTC 的 RTP video）。
//!
//! 两者都是内存通道，全程没有任何真实网络 / 平台代码。

// 管线各环节的 trait 边界（P3 用真实后端替换实现）。
mod traits;
// 假后端：让 P1 能在无平台代码下跑通。
mod fake;
// 假传输：内存里把两端连起来的"网络"。
mod transport;

// 对外暴露的 API。
pub use fake::{
    BufferFrameSink, RawDecoder, RawEncoder, RecordingInputInjector, ScriptedInputSource,
    SyntheticFrameSource,
};
pub use traits::{
    Frame, FrameDecoder, FrameEncoder, FrameSink, FrameSource, InputInjector, InputSource,
    LoopbackError,
};
// `MediaFrame` 是共享契约类型（与 P0 的 `Message` 同级），从 proto 透传，
// 避免媒体通道 crate 反向依赖本 loopback 假实现。
pub use rdcore_proto::MediaFrame;
pub use transport::{loopback_pair, Endpoint};

#[cfg(test)]
mod tests;
