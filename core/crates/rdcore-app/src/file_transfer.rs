//! Track B（韧性面）子模块：文件传输（B4）的分片 / 重组 / 状态机纯逻辑。
//!
//! 与 `connection_lifecycle.rs` 同级、互不依赖；**不修改** `Connection` 定义，也不依赖它——
//! 只把「文件 ↔ `FileTransferEvent` 序列」的转换做成纯函数 / 纯状态，由上层（FFI / 集成层）
//! 用任意 E2E 加密通道（如 `Connection::send_app` 或 `Message::FileTransfer`）驱动收发。
//!
//! 安全（对齐架构文档 §3 与 `rdcore-consent`）：
//! - 文件传输默认 opt-in，**每次传输需 Host 逐次同意**：[`TransferSession::host_decide`]
//!   在收到 `Offer` 后决定 `Accept`/`Reject`；未 `Accept` 前收到 `Chunk` 一律视为协议违规。
//! - 分片大小受 `MAX_FILE_CHUNK_SIZE` 约束（防分配炸弹），由 `rdcore-proto` 的
//!   `Message::validate` 在接收侧再兜一层。

use std::collections::BTreeMap;

use rdcore_proto::{FileTransferAction, FileTransferEvent, MAX_FILE_CHUNK_SIZE};

/// 传输侧错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferError {
    /// 分片超过 `MAX_FILE_CHUNK_SIZE`。
    ChunkTooLarge,
    /// 未获 Host 同意就收到数据分片（协议违规 / 潜在越权）。
    NotAccepted,
    /// 分片序号不连续（丢片 / 乱序超出可重组范围）。
    GapDetected,
    /// 收到的字节总数与 `Offer` 声明的 `size` 不符。
    SizeMismatch,
    /// 当前状态不允许该动作（如已 `Done` 又收到 `Chunk`）。
    BadState,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TransferError::ChunkTooLarge => "chunk too large",
            TransferError::NotAccepted => "chunk before accept",
            TransferError::GapDetected => "chunk sequence gap",
            TransferError::SizeMismatch => "size mismatch",
            TransferError::BadState => "bad state",
        };
        f.write_str(s)
    }
}

impl std::error::Error for TransferError {}

/// 发送侧：把一段字节切成一串 `Chunk` 事件（都≤`MAX_FILE_CHUNK_SIZE`）。
///
/// 先 `offer(name,size)`，待对端 `Accept` 后再依次发这些 `Chunk`，最后 `Done`。
pub fn make_offer(transfer_id: u64, name: &str, size: u64) -> FileTransferEvent {
    FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Offer {
            name: name.to_string(),
            size,
        },
    }
}

/// 把整段字节切成 `Chunk` 事件序列（发送侧在 `Accept` 后调用）。
pub fn chunk_bytes(
    transfer_id: u64,
    bytes: &[u8],
) -> Result<Vec<FileTransferEvent>, TransferError> {
    let mut out = Vec::new();
    for (i, piece) in bytes.chunks(MAX_FILE_CHUNK_SIZE).enumerate() {
        out.push(FileTransferEvent {
            transfer_id,
            action: FileTransferAction::Chunk {
                seq: i as u64,
                data: piece.to_vec(),
            },
        });
    }
    Ok(out)
}

/// 收尾事件（发送侧在最后一片后调用）。
pub fn make_done(transfer_id: u64, chunks: u64) -> FileTransferEvent {
    FileTransferEvent {
        transfer_id,
        action: FileTransferAction::Done { chunks },
    }
}

/// 接收侧单次传输的状态机：从 `Offer` 开始，经 `Accept`/`Chunk` 到 `Done` 重组出完整字节。
///
/// 每个 `transfer_id` 一个实例。Host 在 `Offer` 后必须显式 [`Self::accept`]（逐次同意），
/// 否则收到的 `Chunk` 报 [`TransferError::NotAccepted`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 已收 `Offer`，等待 Host 决定。
    Offered,
    /// Host 已同意，接收数据中。
    Receiving,
    /// 已收 `Done`，完成。
    Completed,
}

pub struct TransferSession {
    transfer_id: u64,
    name: String,
    expected_size: u64,
    phase: Phase,
    /// 已收分片（按 seq 索引，容忍乱序，重组时按 seq 拼接）。
    chunks: BTreeMap<u64, Vec<u8>>,
    /// 下一个期望的分片序号（用于检测缺片）。
    next_seq: u64,
}

impl TransferSession {
    /// 从一条 `Offer` 事件建立会话。
    pub fn on_offer(event: &FileTransferEvent) -> Result<Self, TransferError> {
        if let FileTransferAction::Offer { name, size } = &event.action {
            Ok(Self {
                transfer_id: event.transfer_id,
                name: name.clone(),
                expected_size: *size,
                phase: Phase::Offered,
                chunks: BTreeMap::new(),
                next_seq: 0,
            })
        } else {
            Err(TransferError::BadState)
        }
    }

    pub fn transfer_id(&self) -> u64 {
        self.transfer_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_complete(&self) -> bool {
        self.phase == Phase::Completed
    }

    /// Host 逐次同意（对应 `FileTransferAction::Accept` 的本地决定）。
    pub fn accept(&mut self) {
        if self.phase == Phase::Offered {
            self.phase = Phase::Receiving;
        }
    }

    /// Host 逐次拒绝（对应 `FileTransferAction::Reject`）。
    pub fn reject_event(transfer_id: u64, reason: &str) -> FileTransferEvent {
        FileTransferEvent {
            transfer_id,
            action: FileTransferAction::Reject {
                reason: reason.to_string(),
            },
        }
    }

    /// 收下一条事件（`Chunk`/`Done`）。返回 `Some(完整字节)` 当且仅当 `Done` 且重组校验通过。
    pub fn on_event(
        &mut self,
        event: &FileTransferEvent,
    ) -> Result<Option<Vec<u8>>, TransferError> {
        if event.transfer_id != self.transfer_id {
            return Err(TransferError::BadState);
        }
        match &event.action {
            FileTransferAction::Chunk { seq, data } => {
                if self.phase != Phase::Receiving {
                    return Err(TransferError::NotAccepted);
                }
                if data.len() > MAX_FILE_CHUNK_SIZE {
                    return Err(TransferError::ChunkTooLarge);
                }
                self.chunks.insert(*seq, data.clone());
                // 推进连续序号（允许乱序到达，只要求最终连续）。
                while self.chunks.contains_key(&self.next_seq) {
                    self.next_seq += 1;
                }
                Ok(None)
            }
            FileTransferAction::Done { chunks } => {
                if self.phase != Phase::Receiving {
                    return Err(TransferError::BadState);
                }
                // 必须收齐 0..chunks 的所有分片，且无缺口。
                if self.next_seq != *chunks {
                    return Err(TransferError::GapDetected);
                }
                let mut out = Vec::new();
                for i in 0..*chunks {
                    match self.chunks.get(&i) {
                        Some(part) => out.extend_from_slice(part),
                        None => return Err(TransferError::GapDetected),
                    }
                }
                if out.len() as u64 != self.expected_size {
                    return Err(TransferError::SizeMismatch);
                }
                self.phase = Phase::Completed;
                Ok(Some(out))
            }
            FileTransferAction::Abort => Err(TransferError::BadState),
            // Offer/Accept/Reject 不由本状态机在中途处理。
            _ => Err(TransferError::BadState),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer_event(id: u64, name: &str, size: u64) -> FileTransferEvent {
        make_offer(id, name, size)
    }

    #[test]
    fn chunk_bytes_respects_max_size() {
        let data = vec![7u8; MAX_FILE_CHUNK_SIZE * 2 + 100];
        let chunks = chunk_bytes(1, &data).unwrap();
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            if let FileTransferAction::Chunk { data, .. } = &c.action {
                assert!(data.len() <= MAX_FILE_CHUNK_SIZE);
            } else {
                panic!("应为 Chunk");
            }
        }
    }

    #[test]
    fn full_transfer_roundtrip() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let id = 9;
        let mut sess =
            TransferSession::on_offer(&offer_event(id, "a.bin", payload.len() as u64)).unwrap();
        // 未 Accept 前收 Chunk 应报 NotAccepted。
        let chunks = chunk_bytes(id, &payload).unwrap();
        assert_eq!(
            sess.on_event(&chunks[0]).unwrap_err(),
            TransferError::NotAccepted
        );
        // Host 逐次同意。
        sess.accept();
        for c in &chunks {
            assert!(sess.on_event(c).unwrap().is_none());
        }
        let total = chunks.len() as u64;
        let done_result = sess.on_event(&make_done(id, total)).unwrap();
        assert_eq!(done_result, Some(payload));
        assert!(sess.is_complete());
    }

    #[test]
    fn out_of_order_chunks_reassemble_correctly() {
        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
        let id = 3;
        let mut sess =
            TransferSession::on_offer(&offer_event(id, "b.bin", payload.len() as u64)).unwrap();
        sess.accept();
        let mut chunks = chunk_bytes(id, &payload).unwrap();
        chunks.reverse(); // 乱序投递
        for c in &chunks {
            sess.on_event(c).unwrap();
        }
        let total = chunks.len() as u64;
        let got = sess.on_event(&make_done(id, total)).unwrap();
        assert_eq!(got, Some(payload));
    }

    #[test]
    fn missing_chunk_is_gap_detected() {
        let payload: Vec<u8> = vec![1u8; MAX_FILE_CHUNK_SIZE + 10]; // 两片
        let id = 4;
        let mut sess =
            TransferSession::on_offer(&offer_event(id, "c.bin", payload.len() as u64)).unwrap();
        sess.accept();
        let chunks = chunk_bytes(id, &payload).unwrap();
        // 只发第 0 片，漏第 1 片，却声明 Done{chunks:2}。
        sess.on_event(&chunks[0]).unwrap();
        assert_eq!(
            sess.on_event(&make_done(id, 2)).unwrap_err(),
            TransferError::GapDetected
        );
    }

    #[test]
    fn size_mismatch_rejected() {
        let payload: Vec<u8> = vec![9u8; 100];
        let id = 5;
        // Offer 声称 200，实际发 100。
        let mut sess = TransferSession::on_offer(&offer_event(id, "d.bin", 200)).unwrap();
        sess.accept();
        for c in &chunk_bytes(id, &payload).unwrap() {
            sess.on_event(c).unwrap();
        }
        assert_eq!(
            sess.on_event(&make_done(id, 1)).unwrap_err(),
            TransferError::SizeMismatch
        );
    }

    #[test]
    fn oversize_chunk_rejected() {
        let id = 6;
        let mut sess = TransferSession::on_offer(&offer_event(id, "e.bin", 10)).unwrap();
        sess.accept();
        let big = FileTransferEvent {
            transfer_id: id,
            action: FileTransferAction::Chunk {
                seq: 0,
                data: vec![0u8; MAX_FILE_CHUNK_SIZE + 1],
            },
        };
        assert_eq!(
            sess.on_event(&big).unwrap_err(),
            TransferError::ChunkTooLarge
        );
    }
}
