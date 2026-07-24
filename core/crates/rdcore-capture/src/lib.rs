//! rdcore-capture — 真实画面捕获 / 输入注入的边界 crate（P7）。
//!
//! 架构上 Host 端需要两件事：
//! - **捕获**：把本机屏幕变成 `MediaFrame`（对应 WebRTC 的 RTP 视频源）；
//! - **注入**：把收到的 `InputEvent` 作用到本机（对应 WebRTC DataChannel 的输入方向）。
//!
//! 这两条在 P1 用合成后端（`rdcore_loopback` 的 `SyntheticFrameSource` / `RecordingInputInjector`）
//! 跑通了管线。本 crate 把"真实后端"的**接缝**定义出来，并给出 headless 的 `Null*` 实现：
//!
//! - [`CaptureSource`]：产出 `MediaFrame` 的 trait。真实后端用 `scrap` / 系统 API 抓屏。
//! - [`HostInputInjector`]：把 `InputEvent` 作用到本机的 trait。真实后端用 `enigo` / 系统 API 注入。
//! - [`NullCaptureSource`] / [`NullInputInjector`]：headless / 测试用的无操作实现
//!   （返回合成纯色帧 / 记录但不作用到本机）。
//! - `feature = "real"`：真实后端（scrap + enigo）的接入点；**默认不启用**，以免引入原生 SDK
//!   构建依赖。启用后由 `src/real.rs` 提供 `ScrapCaptureSource`（impl [`CaptureSource`]）与
//!   `EnigoInputInjector`（impl [`HostInputInjector`]）的真实实现——这是 P7 原生落地的就绪代码。

use rdcore_proto::{InputEvent, MediaFrame, VideoCodec};

/// 画面捕获源：产出（编码前的）`MediaFrame`。
///
/// 真实后端（feature = "real"）会调用系统抓屏 API（如 `scrap`、Desktop Duplication、CGDisplay）
/// 取一帧屏幕像素，打包成 `MediaFrame`；上层（媒体通道）拿到后再 `FrameEncoder` 编码发送。
pub trait CaptureSource {
    /// 抓取一帧；无更多帧（如流结束 / 已停止）返回 `None`。
    fn next_frame(&mut self) -> Option<MediaFrame>;
}

/// 输入注入器：把 `InputEvent` 作用到本机（Host 端）。
///
/// 真实后端（feature = "real"）会用系统输入 API（如 `enigo`、SendInput、CGEvent）真正移动鼠标 /
/// 触发按键；上层（数据通道）收到 Viewer 的控制消息后调 [`HostInputInjector::inject`]。
pub trait HostInputInjector {
    /// 注入一条输入事件（鼠标移动 / 按键 / 滚轮等）。
    fn inject(&mut self, event: &InputEvent);
}

/// Headless 捕获源：返回固定尺寸的纯色合成帧（测试 / 无显示器环境）。
///
/// 不调用任何系统 API，仅用于把 `CaptureSource` 管线在 headless 下跑通。
pub struct NullCaptureSource {
    width: u32,
    height: u32,
    color: u8,
    remaining: u32,
}

impl NullCaptureSource {
    /// 构造：`width × height` 的纯色帧（`color`），共 `frames` 帧，之后返回 `None`。
    pub fn new(width: u32, height: u32, frames: u32, color: u8) -> Self {
        Self {
            width,
            height,
            color,
            remaining: frames,
        }
    }
}

impl CaptureSource for NullCaptureSource {
    fn next_frame(&mut self) -> Option<MediaFrame> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(MediaFrame {
            codec: VideoCodec::Raw,
            width: self.width,
            height: self.height,
            // Raw BGRA/RGBA：每像素 4 字节；纯色便于断言。
            data: vec![self.color; (self.width * self.height * 4) as usize],
        })
    }
}

/// Headless 输入注入器：记录所有事件但不真正作用到本机（测试用）。
pub struct NullInputInjector {
    /// 已注入（记录）的事件序列。
    pub injected: Vec<InputEvent>,
}

impl NullInputInjector {
    /// 构造一个空记录器。
    pub fn new() -> Self {
        Self {
            injected: Vec::new(),
        }
    }
}

impl Default for NullInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl HostInputInjector for NullInputInjector {
    fn inject(&mut self, event: &InputEvent) {
        self.injected.push(event.clone());
    }
}

// 真实后端（scrap 抓屏 + enigo 注入）：默认不编译（feature 关闭），启用 `real` 时由 `src/real.rs` 提供。
#[cfg(feature = "real")]
mod real;
#[cfg(feature = "real")]
pub use real::{EnigoInputInjector, ScrapCaptureSource};

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_proto::{InputEvent, InputKind, MouseButton};

    #[test]
    fn null_capture_yields_frames_then_none() {
        let mut src = NullCaptureSource::new(8, 4, 3, 0x10);
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_some());
        assert!(src.next_frame().is_none(), "帧数耗尽后应返回 None");
    }

    #[test]
    fn null_capture_frame_has_expected_size() {
        let mut src = NullCaptureSource::new(4, 2, 1, 0x22);
        let f = src.next_frame().unwrap();
        assert_eq!(f.width, 4);
        assert_eq!(f.height, 2);
        assert_eq!(f.data.len(), 4 * 2 * 4);
    }

    #[test]
    fn null_injector_records_events() {
        let mut inj = NullInputInjector::new();
        let ev = InputEvent {
            seq: 1,
            kind: InputKind::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        };
        inj.inject(&ev);
        assert_eq!(inj.injected.len(), 1);
        assert_eq!(inj.injected[0].seq, 1);
    }
}
