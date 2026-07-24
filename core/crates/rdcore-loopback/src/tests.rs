//! P1 端到端本地回环测试：把 P0 契约在一条"假网络"里真正跑起来。

use crate::*;
use rdcore_proto::{
    Capabilities, ClipboardAction, ClipboardEvent, ConnectionAnswer, ConnectionOffer, DeviceId,
    FrameMetadata, Heartbeat, IceCandidate, InputEvent, InputKind, Message, MouseButton,
    ProtocolError, SessionId, VideoCodec,
};

/// 构造一个测试用的能力集（仅声明支持 Raw，P1 不碰真实编解码）。
fn caps() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::Raw],
        max_width: 1920,
        max_height: 1080,
        fps: 30,
        clipboard: true,
        input: rdcore_proto::InputCaps {
            mouse: true,
            keyboard: true,
            wheel: true,
        },
    }
}

fn session_id() -> SessionId {
    SessionId([7u8; 16])
}

fn device_id() -> DeviceId {
    [9u8; 16]
}

#[test]
fn local_loopback_end_to_end() {
    // 创建一对互联端点：host = 被控端，viewer = 控制端。
    let (host, viewer) = loopback_pair();

    // ---- 1) 信令握手（Offer / Answer / Ice），全部走 P0 的 Message ----
    // 注意 lane 是交叉的：viewer 发出的控制消息由 host 收到，反之亦然。
    let offer = ConnectionOffer {
        session_id: session_id(),
        from: device_id(),
        sdp: "v=0...".to_string(),
        capabilities: caps(),
        frame: Some(FrameMetadata {
            width: 64,
            height: 48,
            fps: 30,
            codec: VideoCodec::Raw,
        }),
        signature: None, // identity 尚未接入（P4 / P5）
    };
    viewer.send_ctrl(&Message::Offer(offer.clone())).unwrap();
    let got_offer = host.recv_ctrl().unwrap();
    assert!(matches!(got_offer, Message::Offer(_)));

    let answer = ConnectionAnswer {
        session_id: session_id(),
        from: device_id(),
        sdp: "v=0...answer".to_string(),
        capabilities: caps(),
        frame: Some(FrameMetadata {
            width: 64,
            height: 48,
            fps: 30,
            codec: VideoCodec::Raw,
        }),
        signature: None,
    };
    host.send_ctrl(&Message::Answer(answer.clone())).unwrap();
    let got_answer = viewer.recv_ctrl().unwrap();
    assert!(matches!(got_answer, Message::Answer(_)));

    let ice = IceCandidate {
        session_id: session_id(),
        from: device_id(),
        candidate: "candidate:1 1 UDP 1234 192.0.2.1 5000 typ host".to_string(),
        sdp_mid: Some("0".to_string()),
        sdp_mline_index: Some(0),
    };
    host.send_ctrl(&Message::Ice(ice.clone())).unwrap();
    let got_ice = viewer.recv_ctrl().unwrap();
    assert!(matches!(got_ice, Message::Ice(_)));

    // ---- 2) 屏幕管线：Host 捕获→编码→media lane→Viewer 解码→渲染 ----
    let width = 64u32;
    let height = 48u32;
    let frames = 5u32;
    let encoder = RawEncoder;
    let decoder = RawDecoder;
    let mut sink = BufferFrameSink::default();

    // 合成源是确定性的：先用一个独立实例预生成"期望帧序列"，再与实际回环结果逐帧比对。
    let expected_frames: Vec<Frame> = {
        let mut s = SyntheticFrameSource::new(width, height, frames);
        std::iter::from_fn(|| s.next_frame()).collect()
    };

    let mut source = SyntheticFrameSource::new(width, height, frames);
    let mut actual_frames: Vec<Frame> = Vec::with_capacity(frames as usize);
    while let Some(frame) = source.next_frame() {
        let media = encoder.encode(&frame).unwrap();
        host.send_media(media).unwrap();
        let media_in = viewer.recv_media().unwrap();
        let decoded = decoder.decode(&media_in).unwrap();
        actual_frames.push(decoded.clone());
        sink.present(&decoded);
    }
    assert_eq!(sink.presented, frames as u64, "应渲染 5 帧");
    assert_eq!(
        actual_frames, expected_frames,
        "每一帧都应无损往返（capture→encode→传输→decode→render）"
    );
    assert_eq!(
        sink.last.as_ref().unwrap().rgba.len(),
        (width * height * 4) as usize
    );

    // ---- 3) 输入管线：Viewer 捕获键鼠→ctrl lane→Host 注入 ----
    let scripted = vec![
        InputEvent {
            seq: 1,
            kind: InputKind::MouseMove { x: 10, y: 20 },
        },
        InputEvent {
            seq: 2,
            kind: InputKind::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
        },
        InputEvent {
            seq: 3,
            kind: InputKind::Key {
                key_code: 65,
                pressed: true,
                modifiers: 0,
            },
        },
    ];
    let mut input_src = ScriptedInputSource::new(scripted.clone());
    let mut injector = RecordingInputInjector::default();
    while let Some(ev) = input_src.next_input() {
        viewer.send_ctrl(&Message::InputEvent(ev)).unwrap();
        let m = host.recv_ctrl().unwrap();
        match m {
            Message::InputEvent(received) => injector.inject(&received),
            other => panic!("期望收到 InputEvent，实际 {:?}", other),
        }
    }
    assert_eq!(injector.received.len(), 3, "3 个输入事件应全部送达 Host");
    assert_eq!(
        injector.received, scripted,
        "输入事件应原样往返（postcard 编解码无损）"
    );

    // ---- 4) 剪贴板：Viewer 请求→Host 回数据（受 MAX_CLIPBOARD_SIZE 约束）----
    viewer
        .send_ctrl(&Message::Clipboard(ClipboardEvent {
            seq: 1,
            action: ClipboardAction::Request,
        }))
        .unwrap();
    let req = host.recv_ctrl().unwrap();
    assert!(matches!(
        req,
        Message::Clipboard(ClipboardEvent {
            action: ClipboardAction::Request,
            ..
        })
    ));

    let clip_data = vec![0xABu8; 128]; // 远小于 5 MiB 上限
    host.send_ctrl(&Message::Clipboard(ClipboardEvent {
        seq: 2,
        action: ClipboardAction::Data(clip_data.clone()),
    }))
    .unwrap();
    let resp = viewer.recv_ctrl().unwrap();
    match resp {
        Message::Clipboard(ClipboardEvent {
            action: ClipboardAction::Data(got),
            ..
        }) => assert_eq!(got, clip_data, "剪贴板数据应原样往返"),
        other => panic!("期望收到 Clipboard Data，实际 {:?}", other),
    }

    // ---- 5) 心跳：双向存活探针，验证 Message::validate 通过 ----
    let hb = Heartbeat {
        seq: 1,
        timestamp_ms: 1_700_000_000_000,
    };
    host.send_ctrl(&Message::Heartbeat(hb.clone())).unwrap();
    let got_hb = viewer.recv_ctrl().unwrap();
    match &got_hb {
        Message::Heartbeat(h) => {
            assert_eq!(h, &hb);
            // 心跳不应触发 PayloadTooLarge（validate 只拦剪贴板超限）。
            got_hb.validate().expect("心跳不应触发 PayloadTooLarge");
        }
        other => panic!("期望收到 Heartbeat，实际 {:?}", other),
    }

    // ---- 6) 验证 P0 的剪贴板护栏：超量数据 decode 后 validate 必须失败 ----
    let huge = vec![0u8; rdcore_proto::MAX_CLIPBOARD_SIZE + 1];
    let oversize = Message::Clipboard(ClipboardEvent {
        seq: 3,
        action: ClipboardAction::Data(huge),
    });
    assert!(
        oversize.validate().is_err(),
        "超量剪贴板应在 validate 被拒（来自 F4）"
    );
}

/// 验证 P0 的 F3 护栏在传输层真正生效：超过 MAX_SIGNALING_MESSAGE_LEN 的控制消息
/// 在接收侧 `decode_limited` 被拒绝（P1 的 `Endpoint::recv_ctrl` 已接入该护栏）。
#[test]
fn ctrl_lane_enforces_max_message_len() {
    let (host, viewer) = loopback_pair();

    // 构造一个编码后超过 64 KiB 上限的控制消息（用超长剪贴板数据）。
    let huge = vec![0u8; rdcore_proto::MAX_SIGNALING_MESSAGE_LEN + 1];
    let msg = Message::Clipboard(ClipboardEvent {
        seq: 1,
        action: ClipboardAction::Data(huge),
    });

    // 编码侧不拦截，消息能发出去。
    viewer.send_ctrl(&msg).unwrap();

    // 接收侧走 decode_limited，应拒绝（PayloadTooLarge 经 LoopbackError::Protocol 透出）。
    let err = host.recv_ctrl().unwrap_err();
    assert!(
        matches!(err, LoopbackError::Protocol(ProtocolError::PayloadTooLarge)),
        "超长控制消息应被传输层拒绝，实际: {:?}",
        err
    );
}
