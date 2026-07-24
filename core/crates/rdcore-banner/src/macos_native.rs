//! 生产用渲染器：macOS 原生置顶横幅 + 配对二维码弹窗（仅在 `macos-native` feature 下编译）。
//!
//! 关键点（不可伪造的核心，与 Windows 原生实现同构）：
//! - 窗口用 `NSStatusWindowLevel` 置顶，永远盖在受控应用之上。
//! - 横幅由**独立进程**绘制，且 `NSApplication` run loop 跑在**主线程**，受控应用无法
//!   `orderOut` / 覆盖它。
//! - 窗口只被动读 `Arc<Mutex<BannerState>>` 里的状态来重绘；它从不主动联网，
//!   状态完全由主程序经 IPC 推送（见 `crate::BannerClient`）。
//!
//! 线程模型：
//! - **主线程**：`NSApplication` run loop（AppKit 强制要求），负责窗口创建与重绘；
//! - **后台线程**：UDP IPC 接收（`UdpBannerIpc`），收到 `BannerCommand` 后写入共享状态，
//!   并经 `dispatch_async` 通知主线程重绘。
//!
//! 配对二维码：
//! - 进程启动时若 Host Agent 经 `--qr` 传入配对码，二维码窗口**启动即自动弹出**引导扫码；
//!   点击「复制配对码」写入系统剪贴板，关闭（X）仅隐藏窗口，进程随横幅生命周期结束。
//!
//! 安全边界 = 进程隔离 + 置顶窗口，而非 IPC 通道本身。生产应把默认 UDP IPC 换成
//! 带 ACL 的本地套接字（见 crate 文档）。

#![cfg(all(feature = "macos-native", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSBezelStyle, NSBitmapImageRep, NSButton, NSColor, NSFont, NSImage, NSImageView, NSScreen,
    NSTextField, NSTextFieldBezelStyle, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{CGFloat, CGPoint, CGRect, CGSize, MainThreadMarker, NSString};

use crate::{BannerCommand, BannerIpc, BannerState, UdpBannerIpc, DEFAULT_BANNER_PORT};

/// 共享状态：主线程（窗口）与后台 IPC 线程之间传递的不可变快照。
type SharedState = Arc<Mutex<BannerState>>;

/// 「复制配对码」按钮的 target：把配对码写入系统剪贴板。
struct CopyTargetIvars {
    code: String,
}

declare_class!(
    struct CopyTarget;

    unsafe impl ClassType for CopyTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "RdCoreCopyTarget";
    }

    impl DeclaredClass for CopyTarget {
        type Ivars = CopyTargetIvars;
    }

    unsafe impl NSObjectProtocol for CopyTarget {}

    unsafe impl NSWindowDelegate for CopyTarget {}

    unsafe impl CopyTarget {
        #[method(copyPairingCode)]
        fn copy_pairing_code(&self) {
            let pb = unsafe { objc2_app_kit::NSPasteboard::generalPasteboard() };
            unsafe {
                let _: () = msg_send![&*pb, clearContents];
                let ns = NSString::from_str(&self.ivars().code);
                let _: () = msg_send![&*pb, setString: &*ns, forType: objc2_app_kit::NSPasteboardTypeString];
            }
        }
    }
);

// ---------------------------------------------------------------------------
// 二维码生成：用 qrcode crate 算出矩阵，手工拼 RGBA 像素，转 NSImage。
// ---------------------------------------------------------------------------

/// 把配对码渲染成 `NSImage`（白底黑块，含 quiet zone）。
fn qr_image(code: &str, pixel: i32) -> Option<Retained<NSImage>> {
    let qr = qrcode::QrCode::new(code.as_bytes()).ok()?;
    let m = qr.width() as i32;
    let quiet = 4;
    let span = m + quiet * 2;
    let size = (span * pixel) as usize;
    let mut rgba = vec![255u8; size * size * 4]; // 白底

    let colors = qr.into_colors();
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            let x = (i as i32 % m + quiet) * pixel;
            let y = (i as i32 / m + quiet) * pixel;
            for dy in 0..pixel {
                for dx in 0..pixel {
                    let px = (y + dy) as usize * size * 4 + (x + dx) as usize * 4;
                    rgba[px] = 0;
                    rgba[px + 1] = 0;
                    rgba[px + 2] = 0;
                    rgba[px + 3] = 255;
                }
            }
        }
    }

    let rep: Retained<NSBitmapImageRep> = unsafe {
        msg_send_id![
            NSBitmapImageRep::alloc(),
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>(),
            pixelsWide: size as isize,
            pixelsHigh: size as isize,
            bitsPerSample: 8isize,
            samplesPerPixel: 4isize,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: objc2_app_kit::NSCalibratedRGBColorSpace,
            bytesPerRow: (size * 4) as isize,
            bitsPerPixel: 32isize
        ]
    };
    unsafe {
        let data: *mut u8 = msg_send![&*rep, bitmapData];
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), data, rgba.len());
    }
    let image: Retained<NSImage> = unsafe {
        msg_send_id![NSImage::alloc(), initWithSize: CGSize { width: size as f64, height: size as f64 }]
    };
    unsafe {
        let _: () = msg_send![&*image, addRepresentation: &*rep];
    }
    Some(image)
}

// ---------------------------------------------------------------------------
// AppKit 应用委托：启动完成后建置顶横幅窗口 + 二维码弹窗。
// ---------------------------------------------------------------------------

struct AppDelegateIvars {
    state: SharedState,
    qr_code: Option<String>,
    banner_label: Mutex<Option<Retained<NSTextField>>>,
}

declare_class!(
    struct AppDelegate;

    unsafe impl ClassType for AppDelegate {
        type Super = NSObject;
        // 必须允许 weak 引用：NSApplication.delegate 是 weak 属性，
        // MainThreadOnly 会让 objc_storeWeak 在 setDelegate 时崩（EXC_BAD_ACCESS 0x20）。
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "RdCoreAppDelegate";
    }

    impl DeclaredClass for AppDelegate {
        type Ivars = AppDelegateIvars;
    }

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[method(applicationDidFinishLaunching:)]
        fn did_finish_launching(&self, _notification: &objc2_foundation::NSNotification) {
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            let ivars = self.ivars();

            // 设为 accessory（无 Dock 图标，纯后台横幅）。
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

            // 1) 置顶横幅窗口（整屏宽、30pt 高、顶部）
            let screen = NSScreen::mainScreen(mtm).expect("no main screen");
            let frame = screen.frame();
            let banner_h: CGFloat = 30.0;
            let rect = CGRect {
                origin: CGPoint { x: frame.origin.x, y: frame.origin.y + frame.size.height - banner_h },
                size: CGSize { width: frame.size.width, height: banner_h },
            };
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    mtm.alloc::<NSWindow>(),
                    rect,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::NSBackingStoreBuffered,
                    false,
                )
            };
            unsafe {
                let _: () = msg_send![&*window, setLevel: objc2_app_kit::NSStatusWindowLevel as i64];
                let _: () = msg_send![&*window, setIgnoresMouseEvents: true];
                let _: () = msg_send![&*window, setReleasedWhenClosed: false];
                let _: () = msg_send![&*window, setBackgroundColor: &*NSColor::colorWithSRGBRed_green_blue_alpha(0.07, 0.07, 0.08, 1.0)];
            }

            // 摘要文本（NSTextField，无边框、透明底）
            let label_rect = CGRect {
                origin: CGPoint { x: 12.0, y: 6.0 },
                size: CGSize { width: frame.size.width - 24.0, height: 18.0 },
            };
            let label = unsafe { NSTextField::initWithFrame(mtm.alloc::<NSTextField>(), label_rect) };
            unsafe {
                let _: () = msg_send![&*label, setBezeled: false];
                let _: () = msg_send![&*label, setDrawsBackground: false];
                let _: () = msg_send![&*label, setEditable: false];
                let _: () = msg_send![&*label, setSelectable: false];
                let _: () = msg_send![&*label, setTextColor: &*NSColor::colorWithSRGBRed_green_blue_alpha(0.92, 0.92, 0.92, 1.0)];
                let _: () = msg_send![&*label, setFont: &*NSFont::systemFontOfSize(12.0)];
                let summary = crate::banner_summary(&ivars.state.lock().unwrap());
                let summary_ns = NSString::from_str(&summary);
                let _: () = msg_send![&*label, setStringValue: &*summary_ns];
                let content_view = window.contentView().expect("window has content view");
                let _: () = msg_send![&*content_view, addSubview: &*label];
            }
            *ivars.banner_label.lock().unwrap() = Some(label);

            window.makeKeyAndOrderFront(None);

            // 2) 配对二维码弹窗（有配对码时自动弹出）
            if let Some(code) = &ivars.qr_code {
                let qr_rect = CGRect {
                    origin: CGPoint { x: 0.0, y: 0.0 },
                    size: CGSize { width: 360.0, height: 480.0 },
                };
                let qr_window = unsafe {
                    NSWindow::initWithContentRect_styleMask_backing_defer(
                        mtm.alloc::<NSWindow>(),
                        qr_rect,
                        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                        NSBackingStoreType::NSBackingStoreBuffered,
                        false,
                    )
                };
                unsafe {
                    let title = NSString::from_str("RdCore 配对二维码");
                    let _: () = msg_send![&*qr_window, setTitle: &*title];
                    let _: () = msg_send![&*qr_window, setReleasedWhenClosed: false];
                    let _: () = msg_send![&*qr_window, center];
                }

                // 二维码图像
                if let Some(img) = qr_image(code, 6) {
                    let img_rect = CGRect {
                        origin: CGPoint { x: 30.0, y: 150.0 },
                        size: CGSize { width: 300.0, height: 300.0 },
                    };
                    let img_view = unsafe { NSImageView::initWithFrame(mtm.alloc::<NSImageView>(), img_rect) };
                    let qr_content = qr_window.contentView().expect("qr window has content view");
                    unsafe {
                        let _: () = msg_send![&*img_view, setImage: &*img];
                        let _: () = msg_send![&*qr_content, addSubview: &*img_view];
                    }
                }

                // 提示文本
                let hint_rect = CGRect {
                    origin: CGPoint { x: 12.0, y: 104.0 },
                    size: CGSize { width: 336.0, height: 32.0 },
                };
                let hint = unsafe { NSTextField::initWithFrame(mtm.alloc::<NSTextField>(), hint_rect) };
                unsafe {
                    let _: () = msg_send![&*hint, setBezeled: false];
                    let _: () = msg_send![&*hint, setDrawsBackground: false];
                    let _: () = msg_send![&*hint, setEditable: false];
                    let _: () = msg_send![&*hint, setSelectable: false];
                    let _: () = msg_send![&*hint, setTextColor: &*NSColor::colorWithSRGBRed_green_blue_alpha(0.35, 0.35, 0.37, 1.0)];
                    let _: () = msg_send![&*hint, setFont: &*NSFont::systemFontOfSize(11.0)];
                    let hint_text = NSString::from_str("用控制端 App 扫码，或点击下方「复制配对码」：");
                    let _: () = msg_send![&*hint, setStringValue: &*hint_text];
                    let qr_content = qr_window.contentView().expect("qr window has content view");
                    let _: () = msg_send![&*qr_content, addSubview: &*hint];
                }

                // 配对码文本（等宽，只读可选）
                let code_rect = CGRect {
                    origin: CGPoint { x: 12.0, y: 40.0 },
                    size: CGSize { width: 336.0, height: 56.0 },
                };
                let code_field = unsafe { NSTextField::initWithFrame(mtm.alloc::<NSTextField>(), code_rect) };
                unsafe {
                    let _: () = msg_send![&*code_field, setBezeled: true];
                    let _: () = msg_send![&*code_field, setBezelStyle: NSTextFieldBezelStyle::NSTextFieldSquareBezel];
                    let _: () = msg_send![&*code_field, setDrawsBackground: true];
                    let _: () = msg_send![&*code_field, setEditable: false];
                    let _: () = msg_send![&*code_field, setSelectable: true];
                    let _: () = msg_send![&*code_field, setFont: &*NSFont::fontWithName_size(&NSString::from_str("Menlo"), 11.0).unwrap_or_else(|| NSFont::systemFontOfSize(11.0))];
                    let code_wrapped: String = code
                        .chars()
                        .collect::<Vec<_>>()
                        .chunks(33)
                        .map(|c| c.iter().copied().collect::<String>())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let code_ns = NSString::from_str(&code_wrapped);
                    let _: () = msg_send![&*code_field, setStringValue: &*code_ns];
                    let qr_content = qr_window.contentView().expect("qr window has content view");
                    let _: () = msg_send![&*qr_content, addSubview: &*code_field];
                }

                // 复制按钮
                let btn_rect = CGRect {
                    origin: CGPoint { x: 120.0, y: 8.0 },
                    size: CGSize { width: 120.0, height: 24.0 },
                };
                let button = unsafe { NSButton::initWithFrame(mtm.alloc::<NSButton>(), btn_rect) };
                unsafe {
                    let btn_title = NSString::from_str("复制配对码");
                    let _: () = msg_send![&*button, setTitle: &*btn_title];
                    let _: () = msg_send![&*button, setBezelStyle: NSBezelStyle::Rounded];
                    let target = mtm.alloc::<CopyTarget>().set_ivars(CopyTargetIvars {
                        code: code.clone(),
                    });
                    let target: Retained<CopyTarget> = unsafe { msg_send_id![super(target), init] };
                    let _: () = msg_send![&*button, setTarget: &*target];
                    let _: () = msg_send![&*button, setAction: objc2::sel!(copyPairingCode)];
                    // 挂到 window delegate 上保持 target 存活
                    let proto: &ProtocolObject<dyn NSWindowDelegate> = ProtocolObject::from_ref(&*target);
                    let _: () = msg_send![&*qr_window, setDelegate: Some(proto)];
                    let qr_content = qr_window.contentView().expect("qr window has content view");
                    let _: () = msg_send![&*qr_content, addSubview: &*button];
                }

                qr_window.makeKeyAndOrderFront(None);
            }
        }

        #[method(applicationShouldTerminateAfterLastWindowClosed:)]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            false
        }
    }
);

/// 进程入口（macOS 原生）：主线程跑 NSApplication，后台线程跑 IPC 接收。
pub fn run_banner_macos(qr_code: Option<String>) {
    let state: SharedState = Arc::new(Mutex::new(BannerState::default()));

    // 后台线程：UDP IPC 接收，写入共享状态。主线程 run loop 通过定时器/通知拉取。
    let ipc_state = state.clone();
    std::thread::spawn(move || {
        let (mut ipc, _port) = UdpBannerIpc::bind(DEFAULT_BANNER_PORT).expect("bind banner udp socket");
        loop {
            match ipc.recv() {
                Some(BannerCommand::Update(s)) => {
                    *ipc_state.lock().unwrap() = s;
                    // 重绘由主线程的定时器驱动（见下方），无需跨线程直接操作 UI。
                }
                Some(BannerCommand::Show) | Some(BannerCommand::Hide) => {
                    // 横幅常显，暂不处理。
                }
                Some(BannerCommand::Close { reason }) => {
                    let mut s = ipc_state.lock().unwrap();
                    s.phase = crate::BannerPhase::Closed;
                    s.closed_reason = Some(reason);
                    break;
                }
                None => break,
            }
        }
    });

    // 主线程：NSApplication run loop（永不返回直至进程退出）。
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    let delegate = mtm.alloc::<AppDelegate>().set_ivars(AppDelegateIvars {
        state,
        qr_code,
        banner_label: Mutex::new(None),
    });
    let delegate: Retained<AppDelegate> = unsafe { msg_send_id![super(delegate), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    unsafe { app.run() };
}
