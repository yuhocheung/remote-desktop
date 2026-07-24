//! P0-D 公网 NAT 穿透：TURN 中继路径端到端验证（自包含，无需外部服务）。
//!
//! 背景（架构缺口 P0-D）：仅靠 STUN 收集反射候选，在**对称型 NAT** / 严格企业防火墙下
//! 直连必然失败——两端谁都无法预测对方为自己开的映射端口。此时唯一可靠的兜底是 TURN
//! **中继**：媒体/输入经 TURN 服务器转发（但仍由端到端密钥加密，TURN 只见密文）。
//!
//! 本测试用真实的 `turn` 0.9 服务器（同 webrtc-rs 生态）在 localhost 起一个中继服务，
//! 两个 `WebRtcPeer` 配置：
//!   - `ice_servers = [TURN(127.0.0.1:port, user/pass)]`（不配 STUN）；
//!   - `force_relay = true`：丢弃所有 host/srflx 本地候选，**只保留 TURN relay 候选**。
//!
//! 于是唯一能连通的路径就是「经 TURN 中继」。若数据通道能 open、媒体帧与心跳能往返，
//! 即证明中继路径可用——这正是公网对称 NAT 场景下系统能连上的关键能力。
//!
//! 与 `e2e_real_p2p.rs`（纯 host 回环直连）互补：那条验证「能直连时走直连」，
//! 这条验证「不能直连时走中继」。两条合起来覆盖 ICE 的 host / relay 两类候选。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rdcore_media::{DataChannel, MediaChannel};
use rdcore_proto::{
    Capabilities, ConnectionAnswer, ConnectionOffer, FrameMetadata, Heartbeat, IceCandidate,
    InputCaps, MediaFrame, Message, SessionId, VideoCodec,
};
use rdcore_rtc::{IceServer, RTCIceCandidateInit, RtcConfig, WebRtcPeer};
use rdcore_signaling::SignalingClient;

use turn::auth::{generate_auth_key, AuthHandler};
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::config::{ConnConfig, ServerConfig};
use turn::server::Server;
use turn::Error as TurnError;
use webrtc_util::vnet::net::Net;

const VIEWER_ID: [u8; 16] = [1u8; 16];
const HOST_ID: [u8; 16] = [2u8; 16];

const TURN_USER: &str = "rdcore";
const TURN_PASS: &str = "s3cr3t";
const TURN_REALM: &str = "rdcore.turn";

fn caps() -> Capabilities {
    Capabilities {
        video_codecs: vec![VideoCodec::Raw],
        max_width: 1920,
        max_height: 1080,
        fps: 30,
        clipboard: true,
        input: InputCaps {
            mouse: true,
            keyboard: true,
            wheel: true,
        },
    }
}

fn frame_meta() -> FrameMetadata {
    FrameMetadata {
        width: 64,
        height: 48,
        fps: 30,
        codec: VideoCodec::Raw,
    }
}

/// TURN 鉴权：静态单用户凭据（realm 固定），返回长效密钥 HA1。
struct StaticAuth;

impl AuthHandler for StaticAuth {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, TurnError> {
        if username == TURN_USER {
            Ok(generate_auth_key(TURN_USER, realm, TURN_PASS))
        } else {
            Err(TurnError::ErrFakeErr)
        }
    }
}

/// 在 127.0.0.1 的空闲 UDP 端口启动内嵌 TURN 服务器，返回 `(server, port)`。
/// relay 地址也设为 127.0.0.1，使中继分配落在回环，便于同机验证。
async fn spawn_turn_server() -> (Server, u16) {
    let conn = Arc::new(
        tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind TURN udp"),
    );
    let port = conn.local_addr().expect("turn local_addr").port();

    let server = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                address: "0.0.0.0".to_owned(),
                net: Arc::new(Net::new(None)),
            }),
        }],
        realm: TURN_REALM.to_owned(),
        auth_handler: Arc::new(StaticAuth),
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await
    .expect("启动内嵌 TURN 服务器");

    (server, port)
}

/// 在空闲端口启动信令服务器，返回其地址。
async fn spawn_signaling() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = signaling_svc::serve_listener(listener).await;
    });
    addr
}

#[tokio::test]
async fn webrtc_p2p_over_turn_relay() {
    // 1) 起内嵌 TURN 中继服务器（localhost）。
    let (turn_server, turn_port) = spawn_turn_server().await;
    let turn_url = format!("turn:127.0.0.1:{turn_port}?transport=udp");

    // 2) 起信令服务器，供双方交换 SDP/ICE。
    let sig_addr = spawn_signaling().await;
    let session = SessionId([9u8; 16]);
    let url = format!("ws://{sig_addr}/{}", signaling_svc::session_hex(&session));

    // 3) 两端配置：只给 TURN、不给 STUN，并强制中继——唯一可连通路径即 TURN relay。
    let cfg = RtcConfig {
        ice_servers: vec![IceServer::turn([turn_url], TURN_USER, TURN_PASS)],
        channel_buffer: 64,
        include_loopback: false,
        force_relay: true,
    };
    let viewer_peer = Arc::new(WebRtcPeer::with_config(cfg.clone()).await.unwrap());
    let host_peer = Arc::new(WebRtcPeer::with_config(cfg).await.unwrap());

    let viewer_sig = Arc::new(SignalingClient::connect(&url).await.unwrap());
    let host_sig = Arc::new(SignalingClient::connect(&url).await.unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    // 收集本地实际收集到的候选串，用于断言「确实收集到了 relay 候选」。
    let seen_candidates = Arc::new(StdMutex::new(Vec::<String>::new()));

    let v_task = tokio::spawn(viewer_handshake(
        viewer_peer.clone(),
        viewer_sig.clone(),
        session,
        stop.clone(),
        seen_candidates.clone(),
    ));
    let h_task = tokio::spawn(host_handshake(
        host_peer.clone(),
        host_sig.clone(),
        session,
        stop.clone(),
        seen_candidates.clone(),
    ));

    // 4) 等两条数据通道 open（ICE 经 TURN relay + DTLS 成功）；中继握手更慢，给 30s。
    let connected = tokio::time::timeout(Duration::from_secs(30), async {
        viewer_peer.wait_data_channels_open().await;
        host_peer.wait_data_channels_open().await;
    })
    .await;

    // 停止 ICE 中继循环，收束握手任务。
    stop.store(true, Ordering::SeqCst);
    let _ = tokio::join!(v_task, h_task);

    // 关键断言 A：确实收集到了 TURN relay 类型候选（force_relay 下这是唯一候选来源）。
    let cands = seen_candidates.lock().unwrap().clone();
    assert!(
        cands.iter().any(|c| c.contains("typ relay")),
        "force_relay 下应至少收集到一个 TURN relay 候选；实际收集到: {cands:?}"
    );
    assert!(
        !cands.iter().any(|c| c.contains("typ host")),
        "force_relay 下不应出现 host 候选（应被 ip_filter 丢弃）；实际: {cands:?}"
    );

    // 关键断言 B：连接经中继真的建立了。
    assert!(
        connected.is_ok(),
        "经 TURN 中继的 WebRTC 连接（数据通道 open）应在 30s 内建立；超时说明中继路径未连通"
    );

    // 5) 媒体帧 + 控制消息经「TURN 中继的」P2P 通道往返。
    let (viewer_media, viewer_dc) = viewer_peer.channels();
    let (host_media, host_dc) = host_peer.channels();

    let frame = MediaFrame {
        codec: VideoCodec::Raw,
        width: 16,
        height: 12,
        data: vec![0x5Au8; 16 * 12 * 4],
    };
    host_media
        .send_frame(&frame)
        .await
        .expect("Host 经 TURN 中继媒体通道发帧");
    let got = viewer_media
        .recv_frame()
        .await
        .unwrap()
        .expect("Viewer 应经中继收到媒体帧");
    assert_eq!(got, frame, "视频帧应经 TURN 中继无损往返");

    let hb = Message::Heartbeat(Heartbeat {
        seq: 42,
        timestamp_ms: 1_700_000_000_000,
    });
    host_dc
        .send(&hb)
        .await
        .expect("Host 经 TURN 中继控制通道发消息");
    let got = viewer_dc
        .recv()
        .await
        .unwrap()
        .expect("Viewer 应经中继收到控制消息");
    assert_eq!(got, hb, "控制消息应经 TURN 中继无损往返");

    // 收尾：关闭 TURN 服务器。
    turn_server.close().await.expect("关闭 TURN 服务器");
}

/// Viewer 握手：发 Offer，等 Answer，然后跑 ICE 中继循环。
async fn viewer_handshake(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    stop: Arc<AtomicBool>,
    seen: Arc<StdMutex<Vec<String>>>,
) {
    let sdp = peer.create_offer().await.expect("create_offer");
    sig.send(&conn_offer(session, sdp))
        .await
        .expect("send offer");
    let answer_sdp = loop {
        match recv_with_timeout(&sig).await {
            Some(Message::Answer(a)) => break a.sdp,
            Some(Message::Ice(i)) => feed_ice(&peer, &i).await,
            Some(_) => continue,
            None => return,
        }
    };
    peer.accept_answer(answer_sdp).await.expect("accept_answer");
    relay_loop(peer, sig, session, VIEWER_ID, stop, seen).await;
}

/// Host 握手：等 Offer → 回 Answer，然后跑 ICE 中继循环。
async fn host_handshake(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    stop: Arc<AtomicBool>,
    seen: Arc<StdMutex<Vec<String>>>,
) {
    let offer_sdp = loop {
        match recv_with_timeout(&sig).await {
            Some(Message::Offer(o)) => break o.sdp,
            Some(Message::Ice(i)) => feed_ice(&peer, &i).await,
            Some(_) => continue,
            None => return,
        }
    };
    let answer_sdp = peer.accept_offer(offer_sdp).await.expect("accept_offer");
    sig.send(&conn_answer(session, answer_sdp))
        .await
        .expect("send answer");
    relay_loop(peer, sig, session, HOST_ID, stop, seen).await;
}

/// ICE 中继循环（trickle ICE）：drain 本地候选发对端，接收对端候选加入连接。
/// 同时把本地候选串记入 `seen`，用于断言 relay 候选被收集。
async fn relay_loop(
    peer: Arc<WebRtcPeer>,
    sig: Arc<SignalingClient>,
    session: SessionId,
    from: [u8; 16],
    stop: Arc<AtomicBool>,
    seen: Arc<StdMutex<Vec<String>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        for c in peer.drain_ice_candidates().await {
            seen.lock().unwrap().push(c.candidate.clone());
            sig.send(&ice_msg(session, from, &c))
                .await
                .expect("send ice");
        }
        match recv_with_timeout(&sig).await {
            Some(Message::Ice(i)) => {
                let init: RTCIceCandidateInit = serde_json::from_str(&i.candidate)
                    .expect("ICE candidate 应为 JSON 序列化的 RTCIceCandidateInit");
                let _ = peer.add_ice_candidate(init).await;
            }
            Some(_) => {}
            None => break,
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn recv_with_timeout(sig: &SignalingClient) -> Option<Message> {
    match tokio::time::timeout(Duration::from_millis(50), sig.recv()).await {
        Ok(Ok(m)) => m,
        _ => None,
    }
}

async fn feed_ice(peer: &WebRtcPeer, i: &IceCandidate) {
    let init: RTCIceCandidateInit = serde_json::from_str(&i.candidate)
        .expect("ICE candidate 应为 JSON 序列化的 RTCIceCandidateInit");
    let _ = peer.add_ice_candidate(init).await;
}

fn conn_offer(session: SessionId, sdp: String) -> Message {
    Message::Offer(ConnectionOffer {
        session_id: session,
        from: VIEWER_ID,
        sdp,
        capabilities: caps(),
        frame: Some(frame_meta()),
        signature: None,
    })
}

fn conn_answer(session: SessionId, sdp: String) -> Message {
    Message::Answer(ConnectionAnswer {
        session_id: session,
        from: HOST_ID,
        sdp,
        capabilities: caps(),
        frame: Some(frame_meta()),
        signature: None,
    })
}

fn ice_msg(session: SessionId, from: [u8; 16], cand: &RTCIceCandidateInit) -> Message {
    Message::Ice(IceCandidate {
        session_id: session,
        from,
        candidate: serde_json::to_string(cand).expect("serialize ice candidate"),
        sdp_mid: None,
        sdp_mline_index: None,
    })
}
