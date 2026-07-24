//! rdcore-proto — 远程桌面二进制线协议（契约层）。
//!
//! 纯 Rust 实现，不含任何网络或平台相关代码。消息使用 [`postcard`]
//! 编码为紧凑的二进制格式（类比前端里 `JSON.stringify`，但 postcard 更省空间、
//! 更快，且不携带字段名）。编解码入口见 [`encode`] / [`decode`]。
//!
//! # 线格式稳定性（契约不能随便改）
//! 枚举变体按"位置下标"编码：postcard 序列化的是变体序号，而不是名字。
//! 因此 **绝对不能给已有变体换位置 / 重排**；新增变体只能追加到末尾，
//! 否则旧版本解析新数据会错位（就像后端给前端返回的字段顺序变了，但前端
//! 仍按下标取值一样会出乱子）。
//!
//! # 传输层安全
//! `postcard` 解码时会先读长度前缀再去预分配 `Vec`/`String`。如果有人发来一个
//! "声称很长"的包，就会触发大规模内存预分配（分配炸弹）。所以传输层在 decode
//! 之前必须先限制 buffer 大小（见 `MAX_SIGNALING_MESSAGE_LEN` 与 `decode_limited`），
//! decode 之后还要做语义校验（见 `Message::validate`）。

// 声明三个子模块：错误码与编解码、帧元数据与能力协商、线消息。
pub mod error;
pub mod frame;
pub mod message;

// 把常用的类型/函数重新导出，调用方 `use rdcore_proto::*` 即可拿到大部分 API。
pub use error::{
    canonical_ephemeral_bytes, canonical_signing_bytes, decode, decode_limited, encode,
    ProtocolError, MAX_CLIPBOARD_SIZE, MAX_FILE_CHUNK_SIZE, MAX_SIGNALING_MESSAGE_LEN,
};
pub use frame::{AudioCodec, AudioFrame, Capabilities, FrameMetadata, InputCaps, MediaFrame, VideoCodec};
pub use message::{
    ClipboardAction, ClipboardEvent, ConnectionAnswer, ConnectionOffer, FileTransferAction,
    FileTransferEvent, Heartbeat, IceCandidate, InputEvent, InputKind, Message, MouseButton,
    SessionKeyExchange, SigningPayload,
};
// 顺手重新导出协议所携带的 identity/crypto 类型。
// 注意：私钥（SecretKey）故意不在这里导出 —— 私钥永远不应被序列化进线。
pub use rdcore_crypto::{Ciphertext, Fingerprint, PublicKey, SessionKey, Signature};
pub use rdcore_identity::{DeviceId, PeerIdentity};

use serde::{Deserialize, Serialize};

/// 会话 ID（16 字节）。每一条信令消息都携带它，
/// 这样云端控制面（信令服务器）可以据此把 Offer/Answer/ICE 关联起来，
/// 而云端全程不需要看到任何媒体流内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

#[cfg(test)]
mod tests;
