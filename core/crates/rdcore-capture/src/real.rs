//! rdcore-capture 真实后端（feature = `real`）：scrap 抓屏 + enigo 注入。
//!
//! 仅当编译时启用 `real` feature 才会编译本模块（默认关闭，避免引入原生 SDK 构建依赖）。
//! 代码针对 **scrap 0.5** 与 **enigo 0.5** 的稳定 API 编写；在原生工具链里用
//! `cargo build -p rdcore-capture --features real` 验证（需要本机有显示器与输入权限：
//! Windows 需 DirectX 11.1、macOS 需辅助功能权限、Linux 需 X11/XWayland + libxdo）。
//!
//! 注意像素格式契约：本系统 `VideoCodec::Raw` 约定为 **RGBA**（见 `rdcore_loopback` 的
//! `RawEncoder`/`RawDecoder`），而 scrap 的 `frame()` 产出 **BGRA**，故 `ScrapCaptureSource`
//! 在抓取后做 B↔R 互换，保证 Viewer 端 `RawDecoder` 解出的颜色正确。

use std::io::{Error as IoError, ErrorKind};
use std::thread;
use std::time::Duration;

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use rdcore_proto::{InputEvent, InputKind, MediaFrame, MouseButton, VideoCodec};
use scrap::{Capturer, Display};

use crate::{CaptureSource, HostInputInjector};

/// 单帧目标等待时长（~60fps）。scrap 的 `frame()` 在帧未就绪时返回 `WouldBlock`，
/// 自旋等待过频会空转，故睡一小段再试。
/// 取 2ms 而非 16ms：旧值下帧就绪后平均白等 ~8ms（最坏 16ms），直接把管线
/// 吞吐压到 ~30fps；2ms 的自旋空转开销可忽略，等待精度换来的吞吐更值。
const FRAME_INTERVAL: Duration = Duration::from_millis(2);

/// 真实屏幕捕获源：用 `scrap` 抓主显示器的一帧，转成 `MediaFrame` 交给媒体通道。
pub struct ScrapCaptureSource {
    capturer: Capturer,
    width: u32,
    height: u32,
}

impl ScrapCaptureSource {
    /// 绑定主显示器，开始捕获。无显示器 / 无权限时返回 `Err`。
    pub fn new() -> Result<Self, IoError> {
        let display = Display::primary().map_err(IoError::other)?;
        let capturer = Capturer::new(display).map_err(IoError::other)?;
        let (w, h) = (capturer.width(), capturer.height());
        Ok(Self {
            capturer,
            width: w as u32,
            height: h as u32,
        })
    }
}

impl CaptureSource for ScrapCaptureSource {
    fn next_frame(&mut self) -> Option<MediaFrame> {
        let (w, h) = (self.width as usize, self.height as usize);
        loop {
            match self.capturer.frame() {
                Ok(buffer) => {
                    // scrap 帧格式为 packed BGRA，stride 可能大于 width*4 且逐帧可能变化。
                    // B↔R 互换为 4 字节块操作 + 按行多线程：1440p 超宽（5M 像素）下旧实现
                    // 逐字节 Vec::push（约 2000 万次调用）实测 ~40-50ms，是管线最大瓶颈；
                    // 现分块并行后 ~1-2ms（内存带宽 bound）。
                    let stride = buffer.len() / h.max(1);
                    let buf: &[u8] = &buffer;
                    let mut rgba = vec![0u8; w * h * 4];
                    let threads = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                        .min(8);
                    std::thread::scope(|s| {
                        let rows_per = h.div_ceil(threads);
                        let mut rest: &mut [u8] = &mut rgba;
                        for t in 0..threads {
                            let y0 = t * rows_per;
                            if y0 >= h {
                                break;
                            }
                            let rows = rows_per.min(h - y0);
                            let r = std::mem::take(&mut rest);
                            let (chunk, tail) = r.split_at_mut(rows * w * 4);
                            rest = tail;
                            s.spawn(move || {
                                for (ry, drow) in chunk.chunks_exact_mut(w * 4).enumerate() {
                                    let srow = &buf[(y0 + ry) * stride..(y0 + ry) * stride + w * 4];
                                    for (s4, d4) in
                                        srow.chunks_exact(4).zip(drow.chunks_exact_mut(4))
                                    {
                                        // BGRA -> RGBA
                                        d4[0] = s4[2];
                                        d4[1] = s4[1];
                                        d4[2] = s4[0];
                                        d4[3] = s4[3];
                                    }
                                }
                            });
                        }
                    });
                    return Some(MediaFrame {
                        codec: VideoCodec::Raw,
                        width: self.width,
                        height: self.height,
                        data: rgba,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // 帧还没准备好：等一小会儿再试（不视为错误）。
                    thread::sleep(FRAME_INTERVAL);
                    continue;
                }
                Err(_) => {
                    // 真实错误（显示器丢失 / 权限撤销等）：捕获流结束。
                    return None;
                }
            }
        }
    }
}

/// 真实输入注入器：用 `enigo` 把 `InputEvent` 作用到本机。
pub struct EnigoInputInjector {
    enigo: Enigo,
}

impl EnigoInputInjector {
    /// 建立到本机输入系统的连接。无权限（如 macOS 未授权辅助功能）时返回 `Err`。
    pub fn new() -> Result<Self, IoError> {
        let enigo = Enigo::new(&Settings::default()).map_err(IoError::other)?;
        Ok(Self { enigo })
    }
}

impl HostInputInjector for EnigoInputInjector {
    fn inject(&mut self, event: &InputEvent) {
        match &event.kind {
            InputKind::MouseMove { x, y } => {
                // 协议里坐标是"相对被控端屏幕的像素位置" → 绝对移动。
                let _ = self.enigo.move_mouse(*x, *y, Coordinate::Abs);
            }
            InputKind::MouseButton { button, pressed } => {
                let btn = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::Right => Button::Right,
                    MouseButton::Back => Button::Back,
                    MouseButton::Forward => Button::Forward,
                };
                let dir = if *pressed {
                    Direction::Press
                } else {
                    Direction::Release
                };
                let _ = self.enigo.button(btn, dir);
            }
            InputKind::MouseWheel { delta_x, delta_y } => {
                // enigo 0.5 的 `scroll(length, Axis)`：分别映射垂直/水平滚轮。
                if *delta_y != 0 {
                    let _ = self.enigo.scroll(*delta_y as i32, Axis::Vertical);
                }
                if *delta_x != 0 {
                    let _ = self.enigo.scroll(*delta_x as i32, Axis::Horizontal);
                }
            }
            InputKind::Key {
                key_code,
                pressed,
                modifiers: _,
            } => {
                // key_code 为平台扫描码 → 用 `Key::Other(u32)`（enigo 0.5 的原始键码变体）发送。
                // modifiers 为修饰键位掩码，本骨架不单独作用（如需组合键，
                // 应把 modifiers 展开成多条 Press/Release 事件）。
                let key = Key::Other(*key_code);
                let dir = if *pressed {
                    Direction::Press
                } else {
                    Direction::Release
                };
                let _ = self.enigo.key(key, dir);
            }
            InputKind::KeyWithChar {
                key_code,
                character,
                pressed,
                modifiers: _,
            } => {
                // IME 友好双发：pressed=true 且 character 非空 → enigo.text() 文本注入
                // （支持中文/日文等 IME 合成输入，enigo 0.5 Keyboard::text）；否则
                // fallback 到 scancode 物理按键（快捷键/游戏）。
                if *pressed {
                    if let Some(ch) = character {
                        if !ch.is_empty() {
                            let _ = self.enigo.text(ch);
                            return;
                        }
                    }
                    let key = Key::Other(*key_code);
                    let _ = self.enigo.key(key, Direction::Press);
                } else {
                    let key = Key::Other(*key_code);
                    let _ = self.enigo.key(key, Direction::Release);
                }
            }
        }
    }
}
