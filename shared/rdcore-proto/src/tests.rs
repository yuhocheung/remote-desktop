//! 线协议的 round-trip 测试。

use crate::frame::{Capabilities, FrameMetadata, InputCaps, VideoCodec};
use crate::message::{
    ClipboardAction, ClipboardEvent, ConnectionAnswer, ConnectionOffer, FileTransferAction,
    FileTransferEvent, Heartbeat, IceCandidate, InputEvent, InputKind, Message, MouseButton,
};
use crate::{
    canonical_signing_bytes, decode, decode_limited, encode, DeviceId, ProtocolError, SessionId,
    Signature, MAX_CLIPBOARD_SIZE, MAX_FILE_CHUNK_SIZE, MAX_SIGNALING_MESSAGE_LEN,
};
use rdcore_crypto::PublicKey;

/// 造一个设备 ID：16 个相同字节（测试用，便于肉眼区分）。
fn id(b: u8) -> DeviceId {
    [b; 16]
}

/// 造一个会话 ID。
fn sid(b: u8) -> SessionId {
    SessionId([b; 16])
}

/// 造一个 64 字节全相同的签名（测试用）。
fn sig(b: u8) -> Signature {
    Signature([b; 64])
}

/// 造一份 1080p/60fps/H264 的画面元数据。
fn frame() -> FrameMetadata {
    FrameMetadata {
        width: 1920,
        height: 1080,
        fps: 60,
        codec: VideoCodec::H264,
    }
}

/// 造一份典型能力集（含三种编解码器、4K、全输入）。
fn caps() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Vp8],
        max_width: 3840,
        max_height: 2160,
        fps: 60,
        clipboard: true,
        input: InputCaps {
            mouse: true,
            keyboard: true,
            wheel: true,
        },
    }
}

/// 一组合覆盖全部变体的样例消息，供 round-trip 测试复用。
fn sample_messages() -> Vec<Message> {
    vec![
        Message::Offer(ConnectionOffer {
            session_id: sid(1),
            from: id(1),
            sdp: "v=0...".into(),
            capabilities: caps(),
            frame: Some(frame()),
            signature: Some(sig(1)),
        }),
        Message::Answer(ConnectionAnswer {
            session_id: sid(1),
            from: id(2),
            sdp: "v=0...".into(),
            capabilities: caps(),
            frame: Some(frame()),
            signature: Some(sig(2)),
        }),
        Message::Ice(IceCandidate {
            session_id: sid(1),
            from: id(1),
            candidate: "candidate:1 1 UDP ...".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        }),
        Message::InputEvent(InputEvent {
            seq: 42,
            kind: InputKind::MouseMove { x: 100, y: 200 },
        }),
        Message::InputEvent(InputEvent {
            seq: 43,
            kind: InputKind::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        }),
        Message::InputEvent(InputEvent {
            seq: 44,
            kind: InputKind::Key {
                key_code: 65,
                pressed: true,
                modifiers: 0,
            },
        }),
        Message::Clipboard(ClipboardEvent {
            seq: 7,
            action: ClipboardAction::Data(b"hello".to_vec()),
        }),
        Message::Clipboard(ClipboardEvent {
            seq: 8,
            action: ClipboardAction::Request,
        }),
        Message::Heartbeat(Heartbeat {
            seq: 99,
            timestamp_ms: 1_700_000_000_000,
        }),
        Message::FileTransfer(FileTransferEvent {
            transfer_id: 7,
            action: FileTransferAction::Offer {
                name: "report.pdf".into(),
                size: 1024,
            },
        }),
        Message::FileTransfer(FileTransferEvent {
            transfer_id: 7,
            action: FileTransferAction::Chunk {
                seq: 0,
                data: vec![0xAB; 64],
            },
        }),
        Message::FileTransfer(FileTransferEvent {
            transfer_id: 7,
            action: FileTransferAction::Done { chunks: 1 },
        }),
    ]
}

#[test]
fn roundtrip_all_variants() {
    // 对每条消息：编码→解码，结果应完全一致（契约层最基本的正确性保证）。
    for msg in sample_messages() {
        let bytes = encode(&msg).expect("encode");
        let back = decode(&bytes).expect("decode");
        assert_eq!(msg, back, "round-trip mismatch for {:?}", msg);
    }
}

#[test]
fn compact_encoding() {
    // 一个小小的心跳在线上的体积应当非常小。
    let hb = Message::Heartbeat(Heartbeat {
        seq: 1,
        timestamp_ms: 123,
    });
    let bytes = encode(&hb).unwrap();
    assert!(
        bytes.len() <= 24,
        "heartbeat too big: {} bytes",
        bytes.len()
    );

    // Offer 含 SDP 字符串，体积更大，但仍应有界（防止协议本身膨胀失控）。
    let offer = sample_messages().into_iter().next().unwrap();
    let bytes = encode(&offer).unwrap();
    assert!(
        bytes.len() < 4096,
        "offer unexpectedly large: {} bytes",
        bytes.len()
    );
}

#[test]
fn decode_garbage_fails() {
    // 垃圾数据必须解码失败，且错误码应为 DecodeError。
    let r = decode(&[0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(r.is_err());
    assert_eq!(r.unwrap_err(), ProtocolError::DecodeError);
}

#[test]
fn embedded_types_roundtrip() {
    // 嵌在消息中的 identity/crypto 类型（PublicKey）能正常 round-trip。
    let pk = PublicKey([7u8; 32]);
    let bytes = postcard::to_stdvec(&pk).unwrap();
    let back: PublicKey = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(pk, back);
}

#[test]
fn signing_payload_is_signature_independent() {
    // 两条 Offer 只在 `signature` 上不同，其规范字节必须完全一致——
    // 证明签名从不覆盖自身（否则 F2 的"循环签名"约束就破了）。
    let mut a = sample_messages()
        .into_iter()
        .find(|m| matches!(m, Message::Offer(_)))
        .expect("has an offer");
    let mut b = a.clone();
    if let Message::Offer(o) = &mut a {
        o.signature = None;
    }
    if let Message::Offer(o) = &mut b {
        o.signature = Some(sig(9));
    }
    let pa = match &a {
        Message::Offer(o) => o.signing_payload(),
        _ => unreachable!(),
    };
    let pb = match &b {
        Message::Offer(o) => o.signing_payload(),
        _ => unreachable!(),
    };
    let ba = canonical_signing_bytes(&pa).unwrap();
    let bb = canonical_signing_bytes(&pb).unwrap();
    assert_eq!(ba, bb, "canonical signing bytes changed with signature");
}

#[test]
fn decode_limited_rejects_oversize() {
    let max = MAX_SIGNALING_MESSAGE_LEN;
    // 超过上限的 buffer 一律拒绝（不论内容如何）。
    let r = decode_limited(&vec![0u8; max + 1], max);
    assert_eq!(r.unwrap_err(), ProtocolError::PayloadTooLarge);

    // 小且合法的消息在限内仍可正常解码。
    let hb = Message::Heartbeat(Heartbeat {
        seq: 1,
        timestamp_ms: 123,
    });
    let bytes = encode(&hb).unwrap();
    assert!(bytes.len() <= max);
    let back = decode_limited(&bytes, max).unwrap();
    assert_eq!(hb, back);
}

#[test]
fn clipboard_size_is_validated() {
    // 恰好在上限内允许。
    let ok = Message::Clipboard(ClipboardEvent {
        seq: 1,
        action: ClipboardAction::Data(vec![0u8; MAX_CLIPBOARD_SIZE]),
    });
    assert!(ok.validate().is_ok());

    // 超出上限 1 字节即拒绝。
    let too_big = Message::Clipboard(ClipboardEvent {
        seq: 2,
        action: ClipboardAction::Data(vec![0u8; MAX_CLIPBOARD_SIZE + 1]),
    });
    assert_eq!(
        too_big.validate().unwrap_err(),
        ProtocolError::PayloadTooLarge
    );
}

#[test]
fn file_transfer_variant_index_is_8() {
    // 下标守护：`Message::FileTransfer` 必须是下标 8（postcard 首字节编码变体下标）。
    // 防止有人重排/插入变体导致 Rust↔Dart 下标错位（契约 §1 铁律）。
    let m = Message::FileTransfer(FileTransferEvent {
        transfer_id: 1,
        action: FileTransferAction::Accept,
    });
    let bytes = encode(&m).unwrap();
    assert_eq!(bytes[0], 8, "FileTransfer 变体下标必须是 8");
}

#[test]
fn file_transfer_chunk_size_is_validated() {
    let ok = Message::FileTransfer(FileTransferEvent {
        transfer_id: 1,
        action: FileTransferAction::Chunk {
            seq: 0,
            data: vec![0u8; MAX_FILE_CHUNK_SIZE],
        },
    });
    assert!(ok.validate().is_ok());

    let too_big = Message::FileTransfer(FileTransferEvent {
        transfer_id: 1,
        action: FileTransferAction::Chunk {
            seq: 0,
            data: vec![0u8; MAX_FILE_CHUNK_SIZE + 1],
        },
    });
    assert_eq!(
        too_big.validate().unwrap_err(),
        ProtocolError::PayloadTooLarge
    );
}
