//! 假后端：让 P1 能在单机、无平台代码的情况下跑通整条管线。
//!
//! 这些实现只用于验证"管线结构 + P0 契约"。真实平台 / 编解码后端在 P3 替换，
//! 只要仍满足 `traits` 里的对应 trait 即可。

use crate::traits::*;
use crate::MediaFrame;
use rdcore_proto::{InputEvent, VideoCodec};

/// 合成捕获源：按帧序号生成确定性渐变图案。
///
/// 用确定性图案的好处：管线另一端解码后，可以反推"这是第几帧"，
/// 从而断言 capture→encode→transport→decode→render 全程无损坏（无损往返）。
pub struct SyntheticFrameSource {
    width: u32,
    height: u32,
    index: u32,
    max_frames: u32,
}

impl SyntheticFrameSource {
    pub fn new(width: u32, height: u32, max_frames: u32) -> Self {
        Self {
            width,
            height,
            index: 0,
            max_frames,
        }
    }

    /// 第 `i` 帧的像素：用 `(x + y + i)` 做 RGB 渐变，便于区分帧与帧。
    fn render(&self, i: u32) -> Frame {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let v = ((x + y + i as usize) % 256) as u8;
                rgba.push(v); // R
                rgba.push(255 - v); // G
                rgba.push((i % 256) as u8); // B（用帧序号，区分不同帧）
                rgba.push(255); // A
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}

impl FrameSource for SyntheticFrameSource {
    fn next_frame(&mut self) -> Option<Frame> {
        if self.index >= self.max_frames {
            return None;
        }
        let i = self.index;
        self.index += 1;
        Some(self.render(i))
    }
}

/// Raw 编码器：直接把 RGBA 像素原样塞进 `MediaFrame`（对应 P0 的 `VideoCodec::Raw`）。
///
/// P1 不做真实视频压缩，目的是验证"编码 → 传输 → 解码"这条链路本身是通的；
/// 真实 H.264 / H.265 编码器在 P3 替换，只要仍满足 `FrameEncoder` trait。
pub struct RawEncoder;

impl FrameEncoder for RawEncoder {
    fn encode(&self, frame: &Frame) -> Result<MediaFrame, LoopbackError> {
        if frame.rgba.len() != frame.byte_len() {
            return Err(LoopbackError::Codec(format!(
                "frame rgba len {} != expected {}",
                frame.rgba.len(),
                frame.byte_len()
            )));
        }
        Ok(MediaFrame {
            codec: VideoCodec::Raw,
            width: frame.width,
            height: frame.height,
            data: frame.rgba.clone(),
        })
    }
}

/// Raw 解码器：把 `MediaFrame(Raw)` 的字节还原成 `Frame`。
pub struct RawDecoder;

impl FrameDecoder for RawDecoder {
    fn decode(&self, media: &MediaFrame) -> Result<Frame, LoopbackError> {
        if media.codec != VideoCodec::Raw {
            return Err(LoopbackError::Codec(format!(
                "unsupported codec in P1 loopback: {:?}",
                media.codec
            )));
        }
        let expected = media.width as usize * media.height as usize * 4;
        if media.data.len() != expected {
            return Err(LoopbackError::Codec(format!(
                "media data len {} != expected {}",
                media.data.len(),
                expected
            )));
        }
        Ok(Frame {
            width: media.width,
            height: media.height,
            rgba: media.data.clone(),
        })
    }
}

/// 缓冲渲染落点：记住最后一帧和已呈现次数，供测试断言。
#[derive(Default)]
pub struct BufferFrameSink {
    pub last: Option<Frame>,
    pub presented: u64,
}

impl FrameSink for BufferFrameSink {
    fn present(&mut self, frame: &Frame) {
        self.last = Some(frame.clone());
        self.presented += 1;
    }
}

/// 脚本输入源：依次吐出预设的输入事件，吐完返回 `None`。
pub struct ScriptedInputSource {
    events: Vec<InputEvent>,
    cursor: usize,
}

impl ScriptedInputSource {
    pub fn new(events: Vec<InputEvent>) -> Self {
        Self { events, cursor: 0 }
    }
}

impl InputSource for ScriptedInputSource {
    fn next_input(&mut self) -> Option<InputEvent> {
        if self.cursor >= self.events.len() {
            return None;
        }
        let e = self.events[self.cursor].clone();
        self.cursor += 1;
        Some(e)
    }
}

/// 录制注入器：把收到的输入事件存下来，供测试断言"输入确实送达 Host"。
#[derive(Default)]
pub struct RecordingInputInjector {
    pub received: Vec<InputEvent>,
}

impl InputInjector for RecordingInputInjector {
    fn inject(&mut self, event: &InputEvent) {
        self.received.push(event.clone());
    }
}
