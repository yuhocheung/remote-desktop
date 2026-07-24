//! FFI 级真实 WebRTC 端到端验证（缺口 M：Viewer Peer 落地证明）。
//!
//! 直接调用 `rdcore_ffi` 暴露的 C 接口，配合进程内 `signaling_svc`，证明：
//!   1) Host/Viewer 经真实信令完成完整握手（签名 Offer/Answer + ICE + E2E 密钥 + 同意）；
//!   2) Host 抓屏（NullCaptureSource）→ E2E 加密 → 媒体通道 → Viewer 解密/解码/渲染，
//!      拿到 RGBA 帧；
//!   3) Viewer 输入 → 控制通道（E2E 加密）→ Host 收到同一输入事件。
//!
//! 这是「Viewer 侧 WebRTC Peer 真正可用」的里程碑验收（Task #14）。

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rdcore_ffi::{
    rdcore_connection_establish, rdcore_connection_free, rdcore_connection_new_host,
    rdcore_connection_new_viewer, rdcore_connection_pull_frame, rdcore_connection_recv_input,
    rdcore_connection_send_input, rdcore_connection_start_capture, rdcore_identity_new,
    rdcore_input_event_free, rdcore_local_peer_json, rdcore_media_frame_free,
    rdcore_remember_peer_json, rdcore_string_free, RdInputEvent,
};
use rdcore_proto::SessionId;

fn cstr(s: &str) -> CString {
    CString::new(s).expect("CString::new")
}

/// FFI 句柄是裸指针，本身非 `Send`；本测试保证同一指针不会跨线程并发使用，
/// 仅在 establish 阶段由独立 OS 线程各自阻塞在自己的 runtime 上。为跨线程安全传递，
/// 把指针按 `usize`（始终 `Send`）搬运，进出线程时再转回指针——这是标准 FFI 做法。
unsafe fn ptr_to_usize<T>(p: *mut T) -> usize {
    p as usize
}
unsafe fn usize_to_ptr<T>(u: usize) -> *mut T {
    u as *mut T
}

/// 把 FFI 返回的 `*mut c_char` 错误串读出为 String 并释放；NULL 表示无错。
unsafe fn take_err(p: *mut std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    rdcore_string_free(p);
    Some(s)
}

#[test]
fn ffi_real_webrtc_host_viewer_e2e() {
    // 显式安装 rustls 默认 CryptoProvider。
    // 依赖闭包中 gateway(axum-server tls-rustls) 启用 aws-lc-rs、webrtc 启用 ring，
    // 二者并存导致 rustls 0.23 的 from_crate_features() 无法自动判定，必须手动安装其一。
    let _ = rustls::crypto::ring::default_provider().install_default();
    // ── 1. 起进程内信令服务器（默认配置：无鉴权/无 TURN，localhost 回环即可）──
    let (tx_addr, rx_addr) = mpsc::channel::<std::net::SocketAddr>();
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().unwrap();
            let _ = tx_addr.send(addr);
            let _ = signaling_svc::serve_listener(listener).await;
        });
    });
    let addr = rx_addr
        .recv_timeout(Duration::from_secs(5))
        .expect("信令服务器应在 5s 内就绪");

    // ── 2. 两个设备的长期身份 + 带外互相导入（TOFU 信任）──
    let host_local = rdcore_identity_new(cstr("host-laptop").as_ptr());
    let viewer_local = rdcore_identity_new(cstr("viewer-phone").as_ptr());
    assert!(
        !host_local.is_null() && !viewer_local.is_null(),
        "身份创建失败"
    );

    let host_peer = rdcore_local_peer_json(host_local);
    let viewer_peer = rdcore_local_peer_json(viewer_local);
    assert!(!host_peer.is_null() && !viewer_peer.is_null());
    unsafe {
        assert!(
            take_err(rdcore_remember_peer_json(viewer_local, host_peer)).is_none(),
            "viewer 导入 host 身份应成功"
        );
        assert!(
            take_err(rdcore_remember_peer_json(host_local, viewer_peer)).is_none(),
            "host 导入 viewer 身份应成功"
        );
        rdcore_string_free(host_peer);
        rdcore_string_free(viewer_peer);
    }

    // ── 3. 构造 Host / Viewer 连接（base URL 不含 session/token；FFI 自动拼路径+token）──
    let session = SessionId([7u8; 16]);
    let shex = signaling_svc::session_hex(&session);
    let base_url = format!("ws://{addr}");
    let url_c = cstr(&base_url);
    let shex_c = cstr(&shex);
    let tok_c = cstr("");
    let scopes_mask: std::os::raw::c_int = 3; // VIEW | INPUT
    let ice_c = cstr(""); // 空 ICE servers（localhost 回环 + include_loopback 即可）

    let host_conn = rdcore_connection_new_host(
        url_c.as_ptr(),
        shex_c.as_ptr(),
        tok_c.as_ptr(),
        host_local,
        1, // include_loopback
        0, // force_relay
        30000,
        scopes_mask,
        ice_c.as_ptr(), // ice_servers
    );
    let viewer_conn = rdcore_connection_new_viewer(
        url_c.as_ptr(),
        shex_c.as_ptr(),
        tok_c.as_ptr(),
        viewer_local,
        1,
        0,
        30000,
        ice_c.as_ptr(), // ice_servers
    );
    assert!(!host_conn.is_null(), "host 连接创建失败（见 last_error）");
    assert!(
        !viewer_conn.is_null(),
        "viewer 连接创建失败（见 last_error）"
    );

    // 媒体编解码器：Connection 现默认 H.264（见 rdcore-app 默认 video_codec）。Raw 1280×720
    // 帧 ≈3.7MiB 会超 WebRTC SCTP DataChannel 单消息上限（≈64KiB）导致发送失败；H.264 编码后
    // 仅数 KiB 可安全经 DataChannel 传输。此测试不显式 set_video_codec，直接验证默认值即生效
    // （即修复缺口 A1：生产 Host/Viewer 无需手动协商即可用 H.264 互通）。如需覆写可用
    // `rdcore_connection_set_video_codec(conn, 0/1)`。

    // ── 4. 并发跑握手（host 等 viewer offer，必须同时进行；45s 超时防挂起）──
    // 指针经 `usize` 跨线程搬运（裸指针非 Send，usize 始终 Send）。
    let (tx, rx) = mpsc::channel::<(&'static str, usize)>();
    let tx_h = tx.clone();
    let tx_v = tx.clone();
    let h_addr = unsafe { ptr_to_usize(host_conn) };
    let v_addr = unsafe { ptr_to_usize(viewer_conn) };
    let host_t = std::thread::spawn(move || {
        let ptr = unsafe { usize_to_ptr::<rdcore_ffi::RdConnection>(h_addr) };
        let r = rdcore_connection_establish(ptr);
        let _ = tx_h.send(("host", r as usize));
    });
    let viewer_t = std::thread::spawn(move || {
        let ptr = unsafe { usize_to_ptr::<rdcore_ffi::RdConnection>(v_addr) };
        let r = rdcore_connection_establish(ptr);
        let _ = tx_v.send(("viewer", r as usize));
    });

    let deadline = Instant::now() + Duration::from_secs(45);
    let mut errs: Vec<(&str, String)> = Vec::new();
    let mut received = 0usize;
    while received < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(("host", r_usize)) => {
                received += 1;
                let p = r_usize as *mut std::os::raw::c_char;
                if let Some(e) = unsafe { take_err(p) } {
                    errs.push(("host", e));
                }
            }
            Ok(("viewer", r_usize)) => {
                received += 1;
                let p = r_usize as *mut std::os::raw::c_char;
                if let Some(e) = unsafe { take_err(p) } {
                    errs.push(("viewer", e));
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("握手超时（45s）：真实 WebRTC 未在 localhost 完成 ICE+握手");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("establish 线程异常退出");
            }
        }
    }
    host_t.join().expect("host establish join");
    viewer_t.join().expect("viewer establish join");
    eprintln!("[TEST] 握手线程已 join，errs={errs:?}");
    assert!(errs.is_empty(), "握手应无错误：{errs:?}");

    // ── 5. 媒体面：Host 抓屏 → Viewer 拿到 RGBA 帧 ──
    eprintln!("[TEST] 启动 Host 抓屏 (start_capture)...");
    unsafe {
        assert!(
            take_err(rdcore_connection_start_capture(host_conn, 30)).is_none(),
            "host 启动抓屏应成功"
        );
    }
    eprintln!("[TEST] 抓屏已启动，Viewer 开始 pull_frame（阻塞等首帧）...");
    // pull_frame 阻塞至首帧到达（Host 已按 fps 推送）。
    let frame = rdcore_connection_pull_frame(viewer_conn);
    eprintln!("[TEST] pull_frame 返回 ptr={frame:?}");
    if frame.is_null() {
        let err = unsafe { take_err(rdcore_ffi::rdcore_last_error()) };
        eprintln!("[TEST] pull_frame 失败原因: {err:?}");
    }
    assert!(!frame.is_null(), "viewer 应拉到一帧（真实媒体通道）");
    unsafe {
        let f = &*frame;
        assert_eq!(f.width, 1280, "帧宽应=1280（NullCaptureSource）");
        assert_eq!(f.height, 720, "帧高应=720");
        assert_eq!(f.len, (1280 * 720 * 4) as usize, "RGBA 缓冲长度应=宽*高*4");
        assert!(!f.data.is_null(), "RGBA 缓冲指针不应为空");
        rdcore_media_frame_free(frame);
    }

    // ── 5b. 音频面：Host 不推音频流时，pull_audio 必须限时返回（回归：Flutter 后台
    // isolate 同线程跑「拉视频 + 拉音频」两个定时器，裸阻塞的 pull_audio 会把线程占死，
    // 视频拉帧随之饿死——iOS 上表现为「只显示首帧后画面冻结」）──
    eprintln!("[TEST] 验证 pull_audio 在无音频流时限时返回...");
    {
        let v_addr = unsafe { ptr_to_usize(viewer_conn) };
        let (tx_a, rx_a) = mpsc::channel::<usize>();
        std::thread::spawn(move || {
            let ptr = unsafe { usize_to_ptr::<rdcore_ffi::RdConnection>(v_addr) };
            let p = rdcore_ffi::rdcore_connection_pull_audio(ptr);
            let _ = tx_a.send(p as usize);
        });
        match rx_a.recv_timeout(Duration::from_secs(5)) {
            Ok(p) => assert_eq!(p, 0, "无音频流时 pull_audio 应返回 NULL"),
            Err(_) => panic!("pull_audio 在无音频流时阻塞超过 5s（应 30ms 超时返回）"),
        }
    }

    // ── 6. 输入面：Viewer 发 → Host 收（同一输入事件原样往返）──
    let ev = Box::into_raw(Box::new(RdInputEvent {
        kind: 3, // Key
        x: 0,
        y: 0,
        button: 0,
        pressed: 1,
        delta_x: 0,
        delta_y: 0,
        key_code: 65,
        modifiers: 0,
    }));
    unsafe {
        assert!(
            take_err(rdcore_connection_send_input(viewer_conn, ev)).is_none(),
            "viewer 发输入应成功"
        );
        // send_input 仅读取（const 指针），所有权仍在本测试侧，需释放。
        rdcore_input_event_free(ev);

        let got = rdcore_connection_recv_input(host_conn);
        assert!(!got.is_null(), "host 应收到 viewer 的输入事件");
        let g = &*got;
        assert_eq!(g.kind, 3, "应为 Key 事件");
        assert_eq!(g.key_code, 65, "key_code 应原样往返");
        assert_eq!(g.pressed, 1, "pressed 应原样往返");
        rdcore_input_event_free(got);
    }

    // ── 7. 清理 ──
    rdcore_connection_free(viewer_conn);
    rdcore_connection_free(host_conn);
    // 不 join 信令服务器线程：serve_listener 是无限 accept 循环、永不返回，
    // join 必然永久阻塞。测试进程退出时该线程随之终止。
    drop(server);
}
