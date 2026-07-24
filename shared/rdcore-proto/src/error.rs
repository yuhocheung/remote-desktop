//! 协议错误码与线编解码（encode/decode）。

use crate::message::{Message, SigningPayload};
use crate::{DeviceId, SessionId};
use serde::{Deserialize, Serialize};

/// 协议层错误码。
///
/// 自身也可序列化（按紧凑变体下标），以便在需要时把错误码随信令一并携带。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolError {
    /// 编码失败。
    EncodeError,
    /// 解码失败（数据损坏或不兼容）。
    DecodeError,
    /// 消息不合法（语义层）。
    InvalidMessage,
    /// 未知会话。
    UnknownSession,
    /// 未授权。
    Unauthorized,
    /// 不支持的编解码器。
    UnsupportedCodec,
    /// 需要用户同意（P5 同意流）。
    ConsentRequired,
    /// 被限流。
    RateLimited,
    /// 内部错误。
    Internal,
    /// 消息或负载超过大小上限（传输/校验护栏）。
    PayloadTooLarge,
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            ProtocolError::EncodeError => "encode error",
            ProtocolError::DecodeError => "decode error",
            ProtocolError::InvalidMessage => "invalid message",
            ProtocolError::UnknownSession => "unknown session",
            ProtocolError::Unauthorized => "unauthorized",
            ProtocolError::UnsupportedCodec => "unsupported codec",
            ProtocolError::ConsentRequired => "consent required",
            ProtocolError::RateLimited => "rate limited",
            ProtocolError::Internal => "internal error",
            ProtocolError::PayloadTooLarge => "payload too large",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ProtocolError {}

/// 信令/控制消息的最大字节数。
///
/// 传输层在调用 [`decode`] 之前，必须拒绝任何大于此值的 buffer，
/// 因为 `postcard` 会按长度前缀预分配 `Vec`/`String`——一个声称很大的前缀
/// 即使真实负载很小也会触发大分配（分配炸弹）。在每个传输边界都优先用
/// [`decode_limited`]。
pub const MAX_SIGNALING_MESSAGE_LEN: usize = 64 * 1024;

/// 剪贴板负载的最大字节数。
///
/// 剪贴板 `Data` 既是已知的数据外泄面，也是 DoS 面。传输层（用自己的 buffer 上限）
/// 与 [`Message::validate`] 都会强制此限制。
pub const MAX_CLIPBOARD_SIZE: usize = 5 * 1024 * 1024;

/// 文件传输单个分片（`FileTransferAction::Chunk`）的最大字节数。
///
/// 与剪贴板同理：Chunk 的 `data` 是 DoS/分配炸弹面，传输层限长 + [`Message::validate`] 双保险。
/// 取 1 MiB：够大以保持吞吐，又限制单帧预分配。
pub const MAX_FILE_CHUNK_SIZE: usize = 1024 * 1024;

/// 把 [`Message`] 编码为紧凑的 postcard 字节。
pub fn encode(msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(msg).map_err(|_| ProtocolError::EncodeError)
}

/// 把紧凑 postcard 字节解码回 [`Message`]。
pub fn decode(buf: &[u8]) -> Result<Message, ProtocolError> {
    postcard::from_bytes(buf).map_err(|_| ProtocolError::DecodeError)
}

/// 先拒绝超过 `max_len` 的 buffer、再解码，以约束预分配大小。
///
/// 在每个传输边界都优先用它而不是 [`decode`]。信令路径用
/// [`MAX_SIGNALING_MESSAGE_LEN`]；数据通道路径（可能携带剪贴板 `Data`）
/// 用 `>=` [`MAX_CLIPBOARD_SIZE`] 的上限。
pub fn decode_limited(buf: &[u8], max_len: usize) -> Result<Message, ProtocolError> {
    if buf.len() > max_len {
        return Err(ProtocolError::PayloadTooLarge);
    }
    decode(buf)
}

/// 把规范签名负载序列化为确定性的 postcard 字节。
///
/// 请对这些字节签名，永远不要对整条 `Message` 签名（那会包含 `signature`
/// 字段本身，导致循环）。见 [`crate::SigningPayload`]。
pub fn canonical_signing_bytes(payload: &SigningPayload) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(payload).map_err(|_| ProtocolError::EncodeError)
}

/// 把端到端加密握手的规范负载（会话 ID || from || 临时公钥）序列化为确定性 postcard 字节。
///
/// 签名应覆盖这些字节，且**绝不**覆盖 `signature` 字段本身。见 [`crate::SessionKeyExchange`]。
pub fn canonical_ephemeral_bytes(
    session_id: SessionId,
    from: DeviceId,
    ephemeral: [u8; 32],
) -> Result<Vec<u8>, ProtocolError> {
    postcard::to_stdvec(&(session_id, from, ephemeral)).map_err(|_| ProtocolError::EncodeError)
}
