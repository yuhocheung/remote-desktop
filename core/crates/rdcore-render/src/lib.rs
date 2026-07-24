//! rdcore-render — 解码帧的渲染落点（媒体面 / Track A）。
//!
//! 在完整系统里，这一步会把 `DecodedFrame` 的 RGBA 上传到 GPU 纹理 / Flutter 的
//! `Texture` widget 供显示。本 crate 提供与平台无关的 **CPU 渲染契约**：校验尺寸并
//! 产出可直接 blit 的 `RenderedFrame`。GUI 侧（`RemoteScreen`）消费 `RenderedFrame`，
//! 负责把 RGBA 真正绘制到屏幕（或上传到 Flutter `Texture`）。

use rdcore_decode::DecodedFrame;
use std::fmt;

/// 一帧已可显示的画面（RGBA8888），由渲染层交付给 GUI。
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedFrame {
    /// 宽（像素）。
    pub width: u32,
    /// 高（像素）。
    pub height: u32,
    /// RGBA8888 像素，长度应为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl RenderedFrame {
    /// 缓冲区应有的字节数（宽 × 高 × 4）。
    pub fn byte_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// 渲染错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// RGBA 缓冲长度与 `width * height * 4` 不符（帧损坏 / 越界）。
    InvalidBuffer { expected: usize, got: usize },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::InvalidBuffer { expected, got } => {
                write!(f, "渲染缓冲长度不符: 期望 {expected}, 实际 {got}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// 把解码后的帧渲染成可显示缓冲（CPU 路径；真实 GPU 上传由 GUI 层完成）。
///
/// 当前为尺寸校验 + 数据拷贝，保证交付给 GUI 的 `RenderedFrame` 一定是合法 RGBA；
/// 后续可在这里接入色彩空间转换 / 缩放 / 叠加 HUD 等。
pub fn render(frame: &DecodedFrame) -> Result<RenderedFrame, RenderError> {
    let expected = (frame.width as usize) * (frame.height as usize) * 4;
    if frame.rgba.len() != expected {
        return Err(RenderError::InvalidBuffer {
            expected,
            got: frame.rgba.len(),
        });
    }
    Ok(RenderedFrame {
        width: frame.width,
        height: frame.height,
        rgba: frame.rgba.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_decode::DecodedFrame;

    #[test]
    fn render_produces_displayable() {
        let f = DecodedFrame {
            width: 8,
            height: 6,
            rgba: vec![0x7F; 8 * 6 * 4],
        };
        let r = render(&f).unwrap();
        assert_eq!(r.width, 8);
        assert_eq!(r.height, 6);
        assert_eq!(r.rgba.len(), 8 * 6 * 4);
        assert_eq!(r.rgba[0], 0x7F);
    }

    #[test]
    fn render_rejects_bad_buffer() {
        let f = DecodedFrame {
            width: 2,
            height: 2,
            rgba: vec![0; 3],
        };
        assert!(matches!(render(&f), Err(RenderError::InvalidBuffer { .. })));
    }
}
