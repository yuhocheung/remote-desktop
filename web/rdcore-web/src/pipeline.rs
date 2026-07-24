//! 帧管线：SCTP 分片重组 + `[4 字节小端长度][postcard]` 帧格式 + E2E 加解密。
//!
//! 镜像三处现有实现（以源码为准）：
//! - 分片：`rdcore-rtc::real` 的 `WebRtcDataChannelTransport`——每条 SCTP 消息第 1 字节
//!   为标签（0=整包 1=首片 2=中片 3=末片），大消息按 16 KiB 切片，通道 `ordered: true`
//!   保证按序到达；`START` 到达即重置重组缓冲（前一条未完成消息被丢弃）。
//! - 帧格式：`rdcore-media` 的 `frame_encode` / `frame_decode`——4 字节小端长度前缀 +
//!   postcard 负载，长度必须恰好匹配（防越界读 / 分配炸弹）。
//! - 加解密：`rdcore-app` 的 `send_media` / `recv_media`（媒体帧 `data` 为 postcard 编码的
//!   `Ciphertext`）与 `send_app` / `recv_app`（`AppMessage` 整条 AEAD 后以
//!   `Message::Encrypted` 承载）。
//!
//! [`Pipeline`] 为纯 Rust 核心（返回 [`WebError`]，便于原生单测）；[`FramePipeline`]
//! 是 wasm-bindgen 薄封装（错误映射为 `JsError`——`JsError::new` 在非 wasm 目标会 panic，
//! 因此原生测试一律走核心层）。

use rdcore_crypto::{aead_open, aead_seal, SessionKey};
use rdcore_proto::{
    AudioCodec, AudioFrame, Ciphertext, ClipboardAction, ClipboardEvent, FileTransferAction,
    FileTransferEvent, Heartbeat, InputEvent, InputKind, MediaFrame, Message, MouseButton,
    VideoCodec, MAX_CLIPBOARD_SIZE, MAX_FILE_CHUNK_SIZE,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

use crate::{hex_encode, WebError};

/// 与 `rdcore-rtc` 一致：单条 SCTP 消息最大载荷（含 1 字节分片标签）。
pub const DC_CHUNK_SIZE: usize = 16 * 1024;
/// 分片标签：未分片整包。
pub const TAG_WHOLE: u8 = 0;
/// 分片标签：首片。
pub const TAG_START: u8 = 1;
/// 分片标签：中间片。
pub const TAG_MIDDLE: u8 = 2;
/// 分片标签：末片。
pub const TAG_END: u8 = 3;

/// 媒体通道帧长上限（与 `rdcore-media::MAX_MEDIA_FRAME_LEN` 一致）。
pub const MAX_MEDIA_FRAME_LEN: usize = 64 * 1024 * 1024;
/// 控制通道帧长上限（与 `rdcore-media::MAX_DATA_FRAME_LEN` 一致）。
pub const MAX_DATA_FRAME_LEN: usize = 8 * 1024 * 1024;
/// 音频通道帧长上限（与 `rdcore-media::MAX_AUDIO_FRAME_LEN` 一致）。
pub const MAX_AUDIO_FRAME_LEN: usize = 256 * 1024;

/// 应用层控制消息：与 `rdcore-app::AppMessage` **逐字节同构**
/// （postcard 按变体下标编码，变体顺序严禁重排；新增只能追加）。
///
/// `rdcore-app` 在 `core/` 下依赖 WebRTC 等重依赖无法进 WASM，故在此镜像；
/// `Consent` 负载直接复用 `rdcore_consent::ConsentDecision` 保证布局一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AppMessage {
    /// 心跳存活探针。
    Heartbeat(Heartbeat),
    /// 远程输入事件（鼠标 / 键盘 / 滚轮）。
    Input(InputEvent),
    /// 剪贴板同步事件。
    Clipboard(ClipboardEvent),
    /// Host 给 Viewer 的授权决定。
    Consent(rdcore_consent::ConsentDecision),
    /// Host 撤销连接。
    Revoke,
    /// Viewer 请求 Host 下一帧输出关键帧（IDR）——P 帧流丢帧/花屏/积压恢复。
    /// 与 rdcore-app 线格式一致：只能追加在枚举末尾，严禁重排。
    RequestKeyframe,
}

/// SCTP 分片重组器（镜像 `WebRtcDataChannelTransport::recv_bytes` 的重组语义）。
#[derive(Default)]
pub struct Reassembly {
    partial: Vec<u8>,
}

impl Reassembly {
    /// 喂入一条 SCTP 消息；凑齐一整帧时返回完整帧字节，否则返回 `None`。
    ///
    /// 语义与 Host 侧一致：空消息与未知标签丢弃；`START` 重置重组缓冲
    /// （`ordered: true` 通道上不存在真正的乱序交叉，`START` 到达即视为前一条作废）。
    pub fn push(&mut self, msg: &[u8]) -> Option<Vec<u8>> {
        let (&tag, payload) = msg.split_first()?;
        match tag {
            TAG_WHOLE => Some(payload.to_vec()),
            TAG_START => {
                self.partial.clear();
                self.partial.extend_from_slice(payload);
                None
            }
            TAG_MIDDLE => {
                self.partial.extend_from_slice(payload);
                None
            }
            TAG_END => {
                self.partial.extend_from_slice(payload);
                Some(std::mem::take(&mut self.partial))
            }
            _ => None,
        }
    }
}

/// 把已帧化（`[4 字节长度][负载]`）的字节切成带标签的 SCTP 消息（镜像发送侧 `send_bytes`）。
pub fn sctp_chunks(framed: &[u8]) -> Vec<Vec<u8>> {
    if framed.len() < DC_CHUNK_SIZE {
        return vec![[&[TAG_WHOLE], framed].concat()];
    }
    let payload = DC_CHUNK_SIZE - 1;
    let n = framed.len().div_ceil(payload);
    framed
        .chunks(payload)
        .enumerate()
        .map(|(i, c)| {
            let tag = if i == 0 {
                TAG_START
            } else if i + 1 == n {
                TAG_END
            } else {
                TAG_MIDDLE
            };
            [&[tag], c].concat()
        })
        .collect()
}

/// 加 4 字节小端长度前缀（镜像 `rdcore-media::frame_encode` 的前缀部分）。
pub fn frame_wrap(payload: &[u8], max_len: usize) -> Result<Vec<u8>, WebError> {
    if payload.len() > max_len || payload.len() > u32::MAX as usize {
        return Err(WebError::Protocol("负载超过帧长上限".into()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// 去 4 字节小端长度前缀（镜像 `frame_decode` 的长度校验：长度必须恰好匹配）。
pub fn frame_unwrap(framed: &[u8], max_len: usize) -> Result<Vec<u8>, WebError> {
    if framed.len() < 4 {
        return Err(WebError::Protocol("帧过短（不足 4 字节长度前缀）".into()));
    }
    let len = u32::from_le_bytes(framed[0..4].try_into().unwrap()) as usize;
    if len > max_len || framed.len() != 4 + len {
        return Err(WebError::Protocol("帧长度前缀与负载不符".into()));
    }
    Ok(framed[4..].to_vec())
}

/// 纯 Rust 帧管线核心（每条 DataChannel 一个实例：媒体 / 控制 / 音频各自独立）。
pub struct Pipeline {
    reasm: Reassembly,
    max_len: usize,
    session_key: Option<SessionKey>,
}

impl Pipeline {
    /// 以帧长上限构造（媒体通道 `MAX_MEDIA_FRAME_LEN`，控制通道 `MAX_DATA_FRAME_LEN`）。
    pub fn new(max_len: usize) -> Self {
        Self {
            reasm: Reassembly::default(),
            max_len,
            session_key: None,
        }
    }

    /// 设置 32 字节会话密钥。
    pub fn set_session_key(&mut self, key: [u8; 32]) {
        self.session_key = Some(SessionKey(key));
    }

    /// 会话密钥是否已设置。
    pub fn has_session_key(&self) -> bool {
        self.session_key.is_some()
    }

    /// 喂入一条 SCTP 消息：分片重组 → 去 4 字节长度前缀 → 返回完整 postcard 负载。
    pub fn push_sctp_message(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, WebError> {
        match self.reasm.push(bytes) {
            Some(framed) => Ok(Some(frame_unwrap(&framed, self.max_len)?)),
            None => Ok(None),
        }
    }

    /// 解析并（在已设会话密钥时）解密媒体帧，返回 JSON `{codec, width, height, data_hex}`
    /// （镜像 `rdcore-app::recv_media`：已建钥时 `data` 为 postcard 编码的 `Ciphertext`）。
    pub fn decrypt_media_frame(&self, payload: &[u8]) -> Result<String, WebError> {
        let frame: MediaFrame =
            postcard::from_bytes(payload).map_err(|e| WebError::Protocol(e.to_string()))?;
        let data = self.decrypt_frame_data(&frame.data)?;
        #[derive(Serialize)]
        struct Out {
            codec: VideoCodec,
            width: u32,
            height: u32,
            data_hex: String,
        }
        Ok(serde_json::to_string(&Out {
            codec: frame.codec,
            width: frame.width,
            height: frame.height,
            data_hex: hex_encode(&data),
        })?)
    }

    /// `decrypt_media_frame` 的二进制版（生产路径）：返回
    /// `[codec: u8][width: u16 LE][height: u16 LE][data...]` 的单块字节，
    /// 避免 JSON + hex 字符串在每帧上的双倍分配与 JS 侧 parseInt 逐字节解码（卡顿根源）。
    /// codec 映射：0=H264 1=H265 2=Vp8 3=Vp9 4=Av1 5=Raw。
    pub fn decrypt_media_frame_bytes(&self, payload: &[u8]) -> Result<Vec<u8>, WebError> {
        let frame: MediaFrame =
            postcard::from_bytes(payload).map_err(|e| WebError::Protocol(e.to_string()))?;
        let data = self.decrypt_frame_data(&frame.data)?;
        let codec: u8 = match frame.codec {
            VideoCodec::H264 => 0,
            VideoCodec::H265 => 1,
            VideoCodec::Vp8 => 2,
            VideoCodec::Vp9 => 3,
            VideoCodec::Av1 => 4,
            VideoCodec::Raw => 5,
        };
        let mut out = Vec::with_capacity(5 + data.len());
        out.push(codec);
        out.extend_from_slice(&(frame.width as u16).to_le_bytes());
        out.extend_from_slice(&(frame.height as u16).to_le_bytes());
        out.extend_from_slice(&data);
        Ok(out)
    }

    /// 帧 `data` 段解密：已建钥按 postcard(Ciphertext) 解 AEAD，未建钥明文透传。
    fn decrypt_frame_data(&self, data: &[u8]) -> Result<Vec<u8>, WebError> {
        match &self.session_key {
            Some(key) => {
                let ct: Ciphertext = postcard::from_bytes(data)
                    .map_err(|e| WebError::Protocol(format!("Ciphertext 解码失败: {e}")))?;
                aead_open(key, &ct)
                    .ok_or_else(|| WebError::Crypto("媒体/音频帧解密失败（篡改 / 密钥不匹配）".into()))
            }
            // 未建钥：按明文透传（仅调试 / 回环场景；生产流程密钥先于媒体帧建立）。
            None => Ok(data.to_vec()),
        }
    }

    /// 解析并（在已设会话密钥时）解密音频帧，返回 JSON
    /// `{codec, channels, sample_rate, data_hex}`（镜像 `rdcore-app::recv_audio`：
    /// 已建钥时 `data` 为 postcard 编码的 `Ciphertext`，仅 `data` 字节加密，
    /// `channels`/`sample_rate` 留明文便于播放器预分配缓冲）。
    ///
    /// `codec = Raw` 时 `data` 即 16-bit 交错 PCM（小端），可直接喂 WebAudio；
    /// `Opus` 需另行解码（M3 探针恒发 Raw）。
    pub fn decrypt_audio_frame(&self, payload: &[u8]) -> Result<String, WebError> {
        let frame: AudioFrame =
            postcard::from_bytes(payload).map_err(|e| WebError::Protocol(e.to_string()))?;
        let data = self.decrypt_frame_data(&frame.data)?;
        #[derive(Serialize)]
        struct Out {
            codec: AudioCodec,
            channels: u16,
            sample_rate: u32,
            data_hex: String,
        }
        Ok(serde_json::to_string(&Out {
            codec: frame.codec,
            channels: frame.channels,
            sample_rate: frame.sample_rate,
            data_hex: hex_encode(&data),
        })?)
    }

    /// `decrypt_audio_frame` 的二进制版（生产路径）：返回
    /// `[codec: u8][channels: u16 LE][sample_rate: u32 LE][data...]` 的单块字节。
    /// codec 映射：0=Opus 1=Raw。
    pub fn decrypt_audio_frame_bytes(&self, payload: &[u8]) -> Result<Vec<u8>, WebError> {
        let frame: AudioFrame =
            postcard::from_bytes(payload).map_err(|e| WebError::Protocol(e.to_string()))?;
        let data = self.decrypt_frame_data(&frame.data)?;
        let codec: u8 = match frame.codec {
            AudioCodec::Opus => 0,
            AudioCodec::Raw => 1,
        };
        let mut out = Vec::with_capacity(7 + data.len());
        out.push(codec);
        out.extend_from_slice(&frame.channels.to_le_bytes());
        out.extend_from_slice(&frame.sample_rate.to_le_bytes());
        out.extend_from_slice(&data);
        Ok(out)
    }

    /// 加密一条应用层控制消息：postcard(`AppMessage`) → AEAD →
    /// postcard(`Message::Encrypted`)（镜像 `rdcore-app::send_app`）。
    /// 发送前还需 `frame_wrap` + 分片（`sctp_chunks`）。
    pub fn encrypt_control_message(&self, plaintext_postcard: &[u8]) -> Result<Vec<u8>, WebError> {
        let key = self.session_key.as_ref().ok_or(WebError::NoSessionKey)?;
        let ct = aead_seal(key, plaintext_postcard);
        Ok(rdcore_proto::encode(&Message::Encrypted(ct))?)
    }

    /// 解密控制消息：postcard(`Message::Encrypted`) → postcard(`AppMessage`) 明文
    /// （镜像 `rdcore-app::recv_app` 的解密段）。
    pub fn decrypt_control_message(&self, bytes: &[u8]) -> Result<Vec<u8>, WebError> {
        let msg = rdcore_proto::decode_limited(bytes, MAX_DATA_FRAME_LEN)?;
        let Message::Encrypted(ct) = msg else {
            return Err(WebError::Protocol("期望 Message::Encrypted".into()));
        };
        let key = self.session_key.as_ref().ok_or(WebError::NoSessionKey)?;
        aead_open(key, &ct)
            .ok_or_else(|| WebError::Crypto("控制消息解密失败（篡改 / 密钥不匹配）".into()))
    }
}

/// 解析 postcard(`AppMessage`) 为 JSON：`{kind, detail}`（`detail` 为完整 serde 展开）。
pub fn app_message_json(bytes: &[u8]) -> Result<String, WebError> {
    let m: AppMessage =
        postcard::from_bytes(bytes).map_err(|e| WebError::Protocol(e.to_string()))?;
    let kind = match &m {
        AppMessage::Heartbeat(_) => "heartbeat",
        AppMessage::Input(_) => "input",
        AppMessage::Clipboard(_) => "clipboard",
        AppMessage::Consent(_) => "consent",
        AppMessage::Revoke => "revoke",
        AppMessage::RequestKeyframe => "request-keyframe",
    };
    Ok(serde_json::json!({
        "kind": kind,
        "detail": serde_json::to_value(&m)?,
    })
    .to_string())
}

// ───────────────────── 发送侧构造（返回 postcard(AppMessage) 明文，纯 Rust 核心） ─────────────────────

fn encode_app(msg: &AppMessage) -> Result<Vec<u8>, WebError> {
    postcard::to_stdvec(msg).map_err(|e| WebError::Protocol(e.to_string()))
}

/// 构造鼠标移动输入事件。
pub fn app_input_mouse_move(seq: u64, x: i32, y: i32) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Input(InputEvent {
        seq,
        kind: InputKind::MouseMove { x, y },
    }))
}

/// 构造鼠标按键输入事件。`button`：0=左 1=中 2=右 3=侧前（后退）4=侧后（前进）。
pub fn app_input_mouse_button(seq: u64, button: u8, pressed: bool) -> Result<Vec<u8>, WebError> {
    let button = match button {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        v => return Err(WebError::BadInput(format!("未知鼠标按键: {v}"))),
    };
    encode_app(&AppMessage::Input(InputEvent {
        seq,
        kind: InputKind::MouseButton { button, pressed },
    }))
}

/// 构造鼠标滚轮输入事件。
pub fn app_input_mouse_wheel(seq: u64, delta_x: i16, delta_y: i16) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Input(InputEvent {
        seq,
        kind: InputKind::MouseWheel { delta_x, delta_y },
    }))
}

/// 构造原始按键输入事件（`key_code` 平台扫描码，`modifiers` 修饰键位掩码）。
pub fn app_input_key(
    seq: u64,
    key_code: u32,
    pressed: bool,
    modifiers: u16,
) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Input(InputEvent {
        seq,
        kind: InputKind::Key {
            key_code,
            pressed,
            modifiers,
        },
    }))
}

/// 构造带字符的按键输入事件（IME 友好；`character` 为空时 Host 回退物理按键）。
pub fn app_input_key_with_char(
    seq: u64,
    key_code: u32,
    character: Option<String>,
    pressed: bool,
    modifiers: u16,
) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Input(InputEvent {
        seq,
        kind: InputKind::KeyWithChar {
            key_code,
            character,
            pressed,
            modifiers,
        },
    }))
}

/// 构造心跳。
pub fn app_heartbeat(seq: u64, timestamp_ms: u64) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Heartbeat(Heartbeat { seq, timestamp_ms }))
}

/// 构造剪贴板请求（请 Host 回传当前剪贴板内容）。
pub fn app_clipboard_request(seq: u64) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Clipboard(ClipboardEvent {
        seq,
        action: ClipboardAction::Request,
    }))
}

/// 构造剪贴板数据（发送方已做清洗；受 `MAX_CLIPBOARD_SIZE` = 5 MiB 限制）。
pub fn app_clipboard_data(seq: u64, bytes: &[u8]) -> Result<Vec<u8>, WebError> {
    if bytes.len() > MAX_CLIPBOARD_SIZE {
        return Err(WebError::Protocol(format!(
            "剪贴板数据 {} 字节超过上限 {} 字节",
            bytes.len(),
            MAX_CLIPBOARD_SIZE
        )));
    }
    encode_app(&AppMessage::Clipboard(ClipboardEvent {
        seq,
        action: ClipboardAction::Data(bytes.to_vec()),
    }))
}

/// 构造剪贴板清除（本地剪贴板已变更，请对端清除镜像副本）。
pub fn app_clipboard_clear(seq: u64) -> Result<Vec<u8>, WebError> {
    encode_app(&AppMessage::Clipboard(ClipboardEvent {
        seq,
        action: ClipboardAction::Clear,
    }))
}

// ─────────── 文件传输（M3-B）：内层明文 = postcard(rdcore_proto::Message::FileTransfer) ───────────
//
// 与剪贴板/输入不同：真实控制通道的应用层枚举 `AppMessage` **没有** FileTransfer 变体；
// 仓库内唯一的文件传输线格式在 rdcore-ffi Track B（`seal_file_event` / `open_file_event`，
// headless RdSession seam）：`encode(Message::FileTransfer(ev))` → AEAD → `Message::Encrypted`。
// 以下构造器与解析器与该格式逐字节对齐（M3 探针 Host 用同一格式应答）。
// 加密仍走 `encrypt_control_message`（它对任意明文 AEAD，不挑内层格式）。

fn encode_file_event(ev: &FileTransferEvent) -> Result<Vec<u8>, WebError> {
    Ok(rdcore_proto::encode(&Message::FileTransfer(ev.clone()))?)
}

/// 构造文件传输提议（Offer：文件名 + 总字节数；等对端 Accept 后才发数据）。
pub fn file_offer(transfer_id: u64, name: &str, size: u64) -> Result<Vec<u8>, WebError> {
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Offer {
            name: name.to_string(),
            size,
        },
    })
}

/// 构造接受（逐次授权）。
pub fn file_accept(transfer_id: u64) -> Result<Vec<u8>, WebError> {
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Accept,
    })
}

/// 构造拒绝（含原因）。
pub fn file_reject(transfer_id: u64, reason: &str) -> Result<Vec<u8>, WebError> {
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Reject {
            reason: reason.to_string(),
        },
    })
}

/// 构造一个数据分片（`seq` 单调递增；单片 ≤ `MAX_FILE_CHUNK_SIZE` = 1 MiB）。
pub fn file_chunk(transfer_id: u64, seq: u64, bytes: &[u8]) -> Result<Vec<u8>, WebError> {
    if bytes.len() > MAX_FILE_CHUNK_SIZE {
        return Err(WebError::Protocol(format!(
            "文件分片 {} 字节超过上限 {} 字节",
            bytes.len(),
            MAX_FILE_CHUNK_SIZE
        )));
    }
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Chunk {
            seq,
            data: bytes.to_vec(),
        },
    })
}

/// 构造收尾事件（Done，含总分片数）。
pub fn file_done(transfer_id: u64, chunks: u64) -> Result<Vec<u8>, WebError> {
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Done { chunks },
    })
}

/// 构造中止事件。
pub fn file_abort(transfer_id: u64) -> Result<Vec<u8>, WebError> {
    encode_file_event(&FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Abort,
    })
}

/// 解析 postcard(`Message`) 中的 `FileTransfer` 变体为 JSON：
/// `{kind:"file", transfer_id, detail:{action, ...}}`；`Chunk` 的 `data` 以 `data_hex` 承载
/// （避免 JSON 数字数组承载百万级字节）。非 FileTransfer 变体报错。
pub fn file_message_json(bytes: &[u8]) -> Result<String, WebError> {
    let msg = rdcore_proto::decode_limited(bytes, MAX_DATA_FRAME_LEN)?;
    let Message::FileTransfer(ev) = msg else {
        return Err(WebError::Protocol("期望 Message::FileTransfer".into()));
    };
    let detail = match &ev.action {
        FileTransferAction::Offer { name, size } => serde_json::json!({
            "action": "Offer", "name": name, "size": size,
        }),
        FileTransferAction::Accept => serde_json::json!({ "action": "Accept" }),
        FileTransferAction::Reject { reason } => serde_json::json!({
            "action": "Reject", "reason": reason,
        }),
        FileTransferAction::Chunk { seq, data } => serde_json::json!({
            "action": "Chunk", "seq": seq, "data_hex": hex_encode(data),
        }),
        FileTransferAction::Done { chunks } => serde_json::json!({
            "action": "Done", "chunks": chunks,
        }),
        FileTransferAction::Abort => serde_json::json!({ "action": "Abort" }),
    };
    Ok(serde_json::json!({
        "kind": "file",
        "transfer_id": ev.transfer_id,
        "detail": detail,
    })
    .to_string())
}

// ───────────────────────────── wasm-bindgen facade ─────────────────────────────

/// 帧管线（每条 DataChannel 一个实例：媒体 / 控制 / 音频各自独立的重组缓冲与上限）。
#[wasm_bindgen]
pub struct FramePipeline {
    inner: Pipeline,
}

#[wasm_bindgen]
impl FramePipeline {
    /// 默认构造（媒体通道上限 64 MiB）。
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::with_max_len(MAX_MEDIA_FRAME_LEN as u32)
    }

    /// 指定帧长上限构造（控制通道建议 `MAX_DATA_FRAME_LEN` = 8 MiB）。
    pub fn with_max_len(max_len: u32) -> Self {
        Self {
            inner: Pipeline::new(max_len as usize),
        }
    }

    /// 设置 32 字节会话密钥（从 `WebHandshake.session_key_bytes()` 取）。
    pub fn set_session_key(&mut self, key_bytes: &[u8]) -> Result<(), JsError> {
        let key: [u8; 32] = key_bytes.try_into().map_err(|_| {
            WebError::BadInput(format!("会话密钥应为 32 字节，实际 {}", key_bytes.len()))
        })?;
        self.inner.set_session_key(key);
        Ok(())
    }

    /// 会话密钥是否已设置。
    pub fn has_session_key(&self) -> bool {
        self.inner.has_session_key()
    }

    /// 喂入一条 SCTP 消息：分片重组 → 去 4 字节长度前缀 → 返回完整 postcard 负载；
    /// 未凑齐返回空（JS 侧为 `undefined`）。
    pub fn push_sctp_message(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, JsError> {
        Ok(self.inner.push_sctp_message(bytes)?)
    }

    /// 解析并（在已设会话密钥时）解密媒体帧，返回 JSON `{codec, width, height, data_hex}`。
    pub fn decrypt_media_frame(&self, payload: &[u8]) -> Result<String, JsError> {
        Ok(self.inner.decrypt_media_frame(payload)?)
    }

    /// `decrypt_media_frame` 的二进制版（生产路径）：
    /// `[codec: u8][width: u16 LE][height: u16 LE][data...]`。
    pub fn decrypt_media_frame_bytes(&self, payload: &[u8]) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.decrypt_media_frame_bytes(payload)?)
    }

    /// 解析并（在已设会话密钥时）解密音频帧，返回 JSON
    /// `{codec, channels, sample_rate, data_hex}`（镜像 `rdcore-app::recv_audio`）。
    pub fn decrypt_audio_frame(&self, payload: &[u8]) -> Result<String, JsError> {
        Ok(self.inner.decrypt_audio_frame(payload)?)
    }

    /// `decrypt_audio_frame` 的二进制版（生产路径）：
    /// `[codec: u8][channels: u16 LE][sample_rate: u32 LE][data...]`。
    pub fn decrypt_audio_frame_bytes(&self, payload: &[u8]) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.decrypt_audio_frame_bytes(payload)?)
    }

    /// 加密一条应用层控制消息为 postcard(`Message::Encrypted`)。
    pub fn encrypt_control_message(&self, plaintext_postcard: &[u8]) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.encrypt_control_message(plaintext_postcard)?)
    }

    /// 解密控制消息为 postcard(`AppMessage`) 明文。
    pub fn decrypt_control_message(&self, bytes: &[u8]) -> Result<Vec<u8>, JsError> {
        Ok(self.inner.decrypt_control_message(bytes)?)
    }
}

impl Default for FramePipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析 postcard(`AppMessage`) 为 JSON：`{kind, detail}`。
#[wasm_bindgen]
pub fn app_message_to_json(bytes: &[u8]) -> Result<String, JsError> {
    Ok(app_message_json(bytes)?)
}

/// 构造鼠标移动输入事件（产物交给 `encrypt_control_message` 加密后发送）。
#[wasm_bindgen]
pub fn build_input_mouse_move(seq: u64, x: i32, y: i32) -> Result<Vec<u8>, JsError> {
    Ok(app_input_mouse_move(seq, x, y)?)
}

/// 构造鼠标按键输入事件。`button`：0=左 1=中 2=右 3=侧前（后退）4=侧后（前进）。
#[wasm_bindgen]
pub fn build_input_mouse_button(seq: u64, button: u8, pressed: bool) -> Result<Vec<u8>, JsError> {
    Ok(app_input_mouse_button(seq, button, pressed)?)
}

/// 构造鼠标滚轮输入事件。
#[wasm_bindgen]
pub fn build_input_mouse_wheel(seq: u64, delta_x: i16, delta_y: i16) -> Result<Vec<u8>, JsError> {
    Ok(app_input_mouse_wheel(seq, delta_x, delta_y)?)
}

/// 构造原始按键输入事件（`key_code` 平台扫描码，`modifiers` 修饰键位掩码）。
#[wasm_bindgen]
pub fn build_input_key(
    seq: u64,
    key_code: u32,
    pressed: bool,
    modifiers: u16,
) -> Result<Vec<u8>, JsError> {
    Ok(app_input_key(seq, key_code, pressed, modifiers)?)
}

/// 构造带字符的按键输入事件（IME 友好；`character` 为空时 Host 回退物理按键）。
#[wasm_bindgen]
pub fn build_input_key_with_char(
    seq: u64,
    key_code: u32,
    character: Option<String>,
    pressed: bool,
    modifiers: u16,
) -> Result<Vec<u8>, JsError> {
    Ok(app_input_key_with_char(
        seq, key_code, character, pressed, modifiers,
    )?)
}

/// 构造心跳。
#[wasm_bindgen]
pub fn build_heartbeat(seq: u64, timestamp_ms: u64) -> Result<Vec<u8>, JsError> {
    Ok(app_heartbeat(seq, timestamp_ms)?)
}

/// 构造关键帧请求（P 帧流丢帧/花屏/解码积压后的快速恢复；产物交给
/// `encrypt_control_message` 加密后经控制通道发送）。
#[wasm_bindgen]
pub fn build_request_keyframe() -> Result<Vec<u8>, JsError> {
    Ok(encode_app(&AppMessage::RequestKeyframe)?)
}

/// 构造剪贴板请求（请 Host 回传当前剪贴板内容）。
#[wasm_bindgen]
pub fn build_clipboard_request(seq: u64) -> Result<Vec<u8>, JsError> {
    Ok(app_clipboard_request(seq)?)
}

/// 构造剪贴板数据（≤ 5 MiB；产物交给 `encrypt_control_message` 加密后发送）。
#[wasm_bindgen]
pub fn build_clipboard_data(seq: u64, bytes: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(app_clipboard_data(seq, bytes)?)
}

/// 构造剪贴板清除。
#[wasm_bindgen]
pub fn build_clipboard_clear(seq: u64) -> Result<Vec<u8>, JsError> {
    Ok(app_clipboard_clear(seq)?)
}

/// 构造文件传输提议（产物为 postcard(Message::FileTransfer) 明文，交给
/// `encrypt_control_message` 加密后发送；内层格式与 rdcore-ffi Track B 对齐）。
#[wasm_bindgen]
pub fn build_file_offer(transfer_id: u64, name: &str, size: u64) -> Result<Vec<u8>, JsError> {
    Ok(file_offer(transfer_id, name, size)?)
}

/// 构造文件传输接受。
#[wasm_bindgen]
pub fn build_file_accept(transfer_id: u64) -> Result<Vec<u8>, JsError> {
    Ok(file_accept(transfer_id)?)
}

/// 构造文件传输拒绝。
#[wasm_bindgen]
pub fn build_file_reject(transfer_id: u64, reason: &str) -> Result<Vec<u8>, JsError> {
    Ok(file_reject(transfer_id, reason)?)
}

/// 构造一个文件数据分片（≤ 1 MiB）。
#[wasm_bindgen]
pub fn build_file_chunk(transfer_id: u64, seq: u64, bytes: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(file_chunk(transfer_id, seq, bytes)?)
}

/// 构造文件传输收尾（含总分片数）。
#[wasm_bindgen]
pub fn build_file_done(transfer_id: u64, chunks: u64) -> Result<Vec<u8>, JsError> {
    Ok(file_done(transfer_id, chunks)?)
}

/// 构造文件传输中止。
#[wasm_bindgen]
pub fn build_file_abort(transfer_id: u64) -> Result<Vec<u8>, JsError> {
    Ok(file_abort(transfer_id)?)
}

/// 解析 postcard(`Message`) 中的 `FileTransfer` 变体为 JSON
/// （`{kind:"file", transfer_id, detail}`；`Chunk.data` 以 `data_hex` 承载）。
#[wasm_bindgen]
pub fn file_message_to_json(bytes: &[u8]) -> Result<String, JsError> {
    Ok(file_message_json(bytes)?)
}

/// 加 4 字节小端长度前缀（发送侧：`encrypt_control_message` 产物 → 本函数 → `sctp_chunk`）。
#[wasm_bindgen(js_name = frame_wrap)]
pub fn frame_wrap_js(bytes: &[u8], max_len: u32) -> Result<Vec<u8>, JsError> {
    Ok(frame_wrap(bytes, max_len as usize)?)
}

/// 已帧化字节需要的 SCTP 消息片数。
#[wasm_bindgen]
pub fn sctp_chunk_count(framed_len: u32) -> u32 {
    let len = framed_len as usize;
    if len < DC_CHUNK_SIZE {
        1
    } else {
        len.div_ceil(DC_CHUNK_SIZE - 1) as u32
    }
}

/// 取已帧化字节的第 `index` 片 SCTP 消息（含 1 字节标签，可直接 `dc.send`）。
#[wasm_bindgen]
pub fn sctp_chunk(framed: &[u8], index: u32) -> Result<Vec<u8>, JsError> {
    let chunks = sctp_chunks(framed);
    chunks
        .get(index as usize)
        .cloned()
        .ok_or_else(|| WebError::BadInput(format!("分片下标越界: {index}")))
        .map_err(Into::into)
}

/// 单条 SCTP 消息最大载荷（含 1 字节分片标签），供 TS 侧参考。
#[wasm_bindgen]
pub fn dc_chunk_size() -> u32 {
    DC_CHUNK_SIZE as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdcore_crypto::SessionKey;

    fn framed_payload(len: usize) -> Vec<u8> {
        // 造一个带长度前缀的假负载：内容可预测（递增字节）。
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        frame_wrap(&payload, MAX_MEDIA_FRAME_LEN).unwrap()
    }

    #[test]
    fn chunk_and_reassemble_whole() {
        let framed = framed_payload(100);
        let chunks = sctp_chunks(&framed);
        assert_eq!(chunks.len(), 1);
        let mut r = Reassembly::default();
        assert_eq!(r.push(&chunks[0]), Some(framed));
    }

    #[test]
    fn chunk_and_reassemble_multi() {
        // 40000 字节负载 → 3 片（16383 + 16383 + 7238）。
        let framed = framed_payload(40000);
        let chunks = sctp_chunks(&framed);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0][0], TAG_START);
        assert_eq!(chunks[1][0], TAG_MIDDLE);
        assert_eq!(chunks[2][0], TAG_END);
        assert!(chunks[..2].iter().all(|c| c.len() == DC_CHUNK_SIZE));
        let mut r = Reassembly::default();
        assert_eq!(r.push(&chunks[0]), None);
        assert_eq!(r.push(&chunks[1]), None);
        assert_eq!(r.push(&chunks[2]), Some(framed));
    }

    #[test]
    fn start_abandons_previous_partial() {
        // 与 Host 侧 recv_bytes 语义一致：新的 START 到达即丢弃前一条未完成消息。
        let framed_a = framed_payload(20000);
        let framed_b = framed_payload(20000);
        let chunks_a = sctp_chunks(&framed_a);
        let chunks_b = sctp_chunks(&framed_b);
        let mut r = Reassembly::default();
        assert_eq!(r.push(&chunks_a[0]), None); // A 首片
        assert_eq!(r.push(&chunks_b[0]), None); // B 首片到达 → A 作废
        assert_eq!(r.push(&chunks_b[1]), Some(framed_b), "应还原完整的 B");
    }

    #[test]
    fn unknown_tag_and_empty_message_dropped() {
        let mut r = Reassembly::default();
        assert_eq!(r.push(&[]), None, "空消息忽略");
        assert_eq!(r.push(&[9, 1, 2, 3]), None, "未知标签忽略");
    }

    #[test]
    fn frame_unwrap_rejects_bad_prefix() {
        assert!(frame_unwrap(&[1, 2], 1024).is_err(), "过短应拒绝");
        let mut b = vec![0u8; 14];
        b[0] = 100; // 声称 100 字节，实际只有 10
        assert!(frame_unwrap(&b, 1024).is_err(), "长度不符应拒绝");
        let oversized = (u32::MAX).to_le_bytes().to_vec();
        assert!(frame_unwrap(&oversized, 1024).is_err(), "超上限应拒绝");
    }

    #[test]
    fn control_encrypt_decrypt_roundtrip() {
        let mut p = Pipeline::new(MAX_DATA_FRAME_LEN);
        let plaintext = app_input_mouse_move(7, 100, 200).unwrap();
        // 未建钥 → NoSessionKey。
        assert_eq!(
            p.encrypt_control_message(&plaintext),
            Err(WebError::NoSessionKey)
        );
        p.set_session_key([0x5Au8; 32]);
        let wire = p.encrypt_control_message(&plaintext).unwrap();
        // wire 是 postcard(Message::Encrypted)；再 frame_wrap + 分片 + 重组走一遍全链路。
        let framed = frame_wrap(&wire, MAX_DATA_FRAME_LEN).unwrap();
        let chunks = sctp_chunks(&framed);
        let mut back = Pipeline::new(MAX_DATA_FRAME_LEN);
        back.set_session_key([0x5Au8; 32]);
        let mut payload = None;
        for c in &chunks {
            payload = back.push_sctp_message(c).unwrap().or(payload);
        }
        let opened = back.decrypt_control_message(&payload.unwrap()).unwrap();
        assert_eq!(opened, plaintext);
        let json = app_message_json(&opened).unwrap();
        assert!(
            json.contains("\"kind\":\"input\""),
            "应解析为 input: {json}"
        );
        assert!(
            json.contains("\"MouseMove\":{\"x\":100,\"y\":200}"),
            "{json}"
        );
    }

    #[test]
    fn media_frame_decrypt_with_key() {
        // 造一帧加密媒体：data = postcard(Ciphertext{nonce, AEAD(像素)})。
        let key = SessionKey([0x33u8; 32]);
        let pixels = vec![0xABu8; 4096];
        let ct = aead_seal(&key, &pixels);
        let sealed = MediaFrame {
            codec: VideoCodec::H264,
            width: 64,
            height: 48,
            data: postcard::to_stdvec(&ct).unwrap(),
        };
        let payload = postcard::to_stdvec(&sealed).unwrap();

        let mut p = Pipeline::new(MAX_MEDIA_FRAME_LEN);
        p.set_session_key([0x33u8; 32]);
        let json = p.decrypt_media_frame(&payload).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["codec"], "H264");
        assert_eq!(v["width"], 64);
        assert_eq!(v["height"], 48);
        assert_eq!(v["data_hex"], hex_encode(&pixels));
    }

    #[test]
    fn media_frame_passthrough_without_key() {
        let raw = MediaFrame {
            codec: VideoCodec::Raw,
            width: 2,
            height: 2,
            data: vec![1u8, 2, 3, 4],
        };
        let payload = postcard::to_stdvec(&raw).unwrap();
        let p = Pipeline::new(MAX_MEDIA_FRAME_LEN);
        let json = p.decrypt_media_frame(&payload).unwrap();
        assert!(json.contains("\"data_hex\":\"01020304\""), "{json}");
    }

    #[test]
    fn media_frame_rejects_tampered_ciphertext() {
        let key = SessionKey([0x33u8; 32]);
        let mut ct = aead_seal(&key, b"pixels");
        if let Some(b) = ct.data.last_mut() {
            *b ^= 0xFF;
        }
        let sealed = MediaFrame {
            codec: VideoCodec::H264,
            width: 64,
            height: 48,
            data: postcard::to_stdvec(&ct).unwrap(),
        };
        let payload = postcard::to_stdvec(&sealed).unwrap();
        let mut p = Pipeline::new(MAX_MEDIA_FRAME_LEN);
        p.set_session_key([0x33u8; 32]);
        assert!(
            p.decrypt_media_frame(&payload).is_err(),
            "篡改密文必须解密失败"
        );
    }

    #[test]
    fn app_builders_encode_expected_variants() {
        let json = app_message_json(&app_input_mouse_button(1, 2, true).unwrap()).unwrap();
        assert!(
            json.contains("\"MouseButton\":{\"button\":\"Right\",\"pressed\":true}"),
            "{json}"
        );
        let json = app_message_json(&app_input_mouse_wheel(2, -3, 4).unwrap()).unwrap();
        assert!(
            json.contains("\"MouseWheel\":{\"delta_x\":-3,\"delta_y\":4}"),
            "{json}"
        );
        let json = app_message_json(&app_input_key(3, 0x1E, true, 0b0010).unwrap()).unwrap();
        // serde_json::Value 默认按字母序排键，逐字段断言而非整段字面量。
        assert!(json.contains("\"Key\":{"), "{json}");
        assert!(json.contains("\"key_code\":30"), "{json}");
        assert!(json.contains("\"pressed\":true"), "{json}");
        assert!(json.contains("\"modifiers\":2"), "{json}");
        let json =
            app_message_json(&app_input_key_with_char(4, 0, Some("你".into()), true, 0).unwrap())
                .unwrap();
        assert!(json.contains("\"KeyWithChar\""), "{json}");
        let json = app_message_json(&app_heartbeat(5, 123).unwrap()).unwrap();
        assert!(json.contains("\"kind\":\"heartbeat\""), "{json}");
        let json = app_message_json(&app_clipboard_request(6).unwrap()).unwrap();
        assert!(json.contains("\"kind\":\"clipboard\""), "{json}");
        assert!(
            app_input_mouse_button(1, 9, true).is_err(),
            "未知按键应拒绝"
        );
    }

    #[test]
    fn clipboard_data_clear_builders_and_limits() {
        // Data / Clear 构造器：app_message_json 可还原，且变体正确。
        let json = app_message_json(&app_clipboard_data(7, b"hello").unwrap()).unwrap();
        assert!(json.contains("\"kind\":\"clipboard\""), "{json}");
        assert!(json.contains("\"Data\":[104,101,108,108,111]"), "{json}");
        let json = app_message_json(&app_clipboard_clear(8).unwrap()).unwrap();
        assert!(json.contains("\"Clear\""), "{json}");
        // 超 MAX_CLIPBOARD_SIZE 必须拒绝（5 MiB 上限）。
        let oversized = vec![0u8; MAX_CLIPBOARD_SIZE + 1];
        assert!(app_clipboard_data(9, &oversized).is_err());
        let at_limit = vec![1u8; MAX_CLIPBOARD_SIZE];
        assert!(app_clipboard_data(9, &at_limit).is_ok());
    }

    #[test]
    fn file_event_builders_roundtrip_and_format() {
        // 构造 → file_message_json 解析还原（格式 = postcard(Message::FileTransfer)）。
        let offer = file_offer(42, "a.bin", 1500).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&file_message_json(&offer).unwrap()).unwrap();
        assert_eq!(v["kind"], "file");
        assert_eq!(v["transfer_id"], 42);
        assert_eq!(v["detail"]["action"], "Offer");
        assert_eq!(v["detail"]["name"], "a.bin");
        assert_eq!(v["detail"]["size"], 1500);
        // 与 AppMessage 互不相容：FileTransfer 明文按 AppMessage 解析必须失败（变体下标 8 越界）。
        assert!(app_message_json(&offer).is_err());

        let chunk = file_chunk(42, 3, b"chunk-data").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&file_message_json(&chunk).unwrap()).unwrap();
        assert_eq!(v["detail"]["action"], "Chunk");
        assert_eq!(v["detail"]["seq"], 3);
        assert_eq!(v["detail"]["data_hex"], hex_encode(b"chunk-data"));

        let done = file_done(42, 2).unwrap();
        assert!(file_message_json(&done).unwrap().contains("\"chunks\":2"));
        let accept = file_accept(42).unwrap();
        assert!(file_message_json(&accept).unwrap().contains("\"Accept\""));
        let reject = file_reject(42, "拒绝原因").unwrap();
        assert!(file_message_json(&reject).unwrap().contains("拒绝原因"));
        let abort = file_abort(42).unwrap();
        assert!(file_message_json(&abort).unwrap().contains("\"Abort\""));

        // 超 MAX_FILE_CHUNK_SIZE 必须拒绝（1 MiB 上限）。
        let oversized = vec![0u8; MAX_FILE_CHUNK_SIZE + 1];
        assert!(file_chunk(42, 0, &oversized).is_err());
        // AppMessage 明文按 file_message_json 解析必须失败（非 FileTransfer 变体）。
        let app = app_clipboard_request(1).unwrap();
        assert!(file_message_json(&app).is_err());
    }

    #[test]
    fn file_event_encrypted_control_roundtrip() {
        // 全链路：构造 → 加密 → frame_wrap → 分片 → 重组 → 解密 → 解析。
        let mut send = Pipeline::new(MAX_DATA_FRAME_LEN);
        send.set_session_key([0x77u8; 32]);
        let plain = file_chunk(9, 1, b"payload").unwrap();
        let wire = send.encrypt_control_message(&plain).unwrap();
        let framed = frame_wrap(&wire, MAX_DATA_FRAME_LEN).unwrap();
        let chunks = sctp_chunks(&framed);
        let mut recv = Pipeline::new(MAX_DATA_FRAME_LEN);
        recv.set_session_key([0x77u8; 32]);
        let mut payload = None;
        for c in &chunks {
            payload = recv.push_sctp_message(c).unwrap().or(payload);
        }
        let opened = recv.decrypt_control_message(&payload.unwrap()).unwrap();
        assert_eq!(opened, plain);
        let json = file_message_json(&opened).unwrap();
        assert!(json.contains(&hex_encode(b"payload")), "{json}");
    }

    #[test]
    fn audio_frame_decrypt_with_key() {
        // 造一帧加密音频：data = postcard(Ciphertext{nonce, AEAD(PCM)})。
        let key = SessionKey([0x66u8; 32]);
        let pcm: Vec<u8> = (0..1920u32).flat_map(|i| (i as i16).to_le_bytes()).collect();
        let ct = aead_seal(&key, &pcm);
        let sealed = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 48_000,
            data: postcard::to_stdvec(&ct).unwrap(),
        };
        let payload = postcard::to_stdvec(&sealed).unwrap();

        let mut p = Pipeline::new(MAX_AUDIO_FRAME_LEN);
        p.set_session_key([0x66u8; 32]);
        let v: serde_json::Value =
            serde_json::from_str(&p.decrypt_audio_frame(&payload).unwrap()).unwrap();
        assert_eq!(v["codec"], "Raw");
        assert_eq!(v["channels"], 2);
        assert_eq!(v["sample_rate"], 48_000);
        assert_eq!(v["data_hex"], hex_encode(&pcm));

        // 篡改密文必须解密失败。
        let mut bad = ct;
        if let Some(b) = bad.data.last_mut() {
            *b ^= 0xFF;
        }
        let sealed = AudioFrame {
            codec: AudioCodec::Raw,
            channels: 2,
            sample_rate: 48_000,
            data: postcard::to_stdvec(&bad).unwrap(),
        };
        let payload = postcard::to_stdvec(&sealed).unwrap();
        assert!(p.decrypt_audio_frame(&payload).is_err());
    }
}
