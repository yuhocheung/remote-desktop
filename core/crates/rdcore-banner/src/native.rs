//! 生产用渲染器：真实 Windows 置顶窗口（仅在 `windows-native` feature 下编译）。
//!
//! 关键点（不可伪造的核心）：
//! - 窗口带 `WS_EX_TOPMOST`，永远盖在受控应用之上。
//! - 横幅由**独立进程**绘制，且窗口线程独立于主程序消息循环，受控应用无法
//!   `ShowWindow(SW_HIDE)` / 覆盖它。
//! - 窗口只被动读 `Arc<Mutex<BannerState>>` 里的状态来重绘；它从不主动联网，
//!   状态完全由主程序经 IPC 推送（见 `crate::BannerClient`）。
//!
//! 托盘与配对二维码（本 feature 的附加 UI）：
//! - 进程启动时在**右下角通知区**放置托盘图标；配对码由 Host Agent 经 `--qr` 传入时，
//!   「配对二维码」窗口**启动即自动弹出**（安装后引导扫码），关闭（X）仅隐藏回托盘；
//!   之后左键单击托盘 / 右键菜单可再次打开。
//! - 二维码窗口与横幅同属本进程的消息循环线程；关闭（X）仅隐藏回托盘，不退出进程。
//!
//! 安全边界 = 进程隔离 + 置顶窗口，而非 IPC 通道本身。生产应把默认 UDP IPC 换成
//! 带 ACL 的命名管道（`\\.\pipe\rdcore-banner`，仅允许本机同用户/服务写入）。

#![cfg(feature = "windows-native")]

use std::cell::Cell;
use std::ptr::null;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{GetLastError, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Ole::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{BannerRenderer, BannerState};

/// 托盘图标回调消息（自定义，基于 WM_APP）。
const WM_TRAYICON: u32 = WM_APP + 1;
/// 托盘图标 ID（单实例，固定即可）。
const TRAY_UID: u32 = 1;
/// 托盘右键菜单项。
const ID_TRAY_SHOW_QR: usize = 1001;
const ID_TRAY_EXIT: usize = 1002;

thread_local! {
    /// 二维码弹窗句柄（仅窗口线程访问；0 = 未创建/无配对码）。
    static QR_HWND: Cell<isize> = const { Cell::new(0) };
    /// 配对码是否已复制到剪贴板（用于绘制反馈文案）。
    static QR_COPIED: Cell<bool> = const { Cell::new(false) };
    /// 「TaskbarCreated」广播消息 ID：Explorer 重启后任务栏重建，需重放 NIM_ADD
    /// 找回托盘图标（窗口线程在消息循环前注册并保存于此）。
    static TASKBAR_CREATED_MSG: Cell<u32> = const { Cell::new(0) };
}

/// 生产渲染器：一个独立窗口线程 + 共享状态。
///
/// `hwnd` 存为 `usize`（而非裸指针）以保证 `Send`——窗口句柄只在拥有它的消息循环
/// 线程里使用，跨线程只搬运其整数值。
pub struct WindowsTopmostBannerRenderer {
    state: Arc<Mutex<BannerState>>,
    hwnd: Arc<Mutex<Option<usize>>>,
}

impl WindowsTopmostBannerRenderer {
    /// `qr_code`：配对码（`<32hex session>:<64hex token>`），有值时启用托盘 + 二维码弹窗。
    pub fn new(qr_code: Option<String>) -> Self {
        let state = Arc::new(Mutex::new(BannerState::default()));
        let hwnd = Arc::new(Mutex::new(None));
        let st = state.clone();
        let hw = hwnd.clone();
        // 窗口必须在创建它的线程里跑消息循环，故单独起线程。
        thread::spawn(move || unsafe { run_window(st, hw, qr_code) });
        Self { state, hwnd }
    }
}

impl BannerRenderer for WindowsTopmostBannerRenderer {
    fn render(&self, s: &BannerState) {
        *self.state.lock().unwrap() = s.clone();
        if let Some(h) = *self.hwnd.lock().unwrap() {
            let hwnd = h as isize; // HWND 在 windows-sys 中为 isize 别名
            unsafe {
                InvalidateRect(hwnd, null(), TRUE);
                UpdateWindow(hwnd);
            }
        }
    }

    fn on_close(&self) {
        if let Some(h) = *self.hwnd.lock().unwrap() {
            let hwnd = h as isize;
            unsafe {
                // 跨线程安全地请窗口自行销毁（消息循环线程会处理 WM_CLOSE）。
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
    }
}

/// 把 (r,g,b) 拼成 COLORREF。
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// UTF-16 宽字符（NUL 结尾）。
fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 诊断日志：追加写 `%TEMP%\rdcore-banner.log`（托盘/窗口创建失败排查用，静默环境无控制台可看）。
fn diag(msg: &str) {
    if let Some(dir) = std::env::var_os("TEMP") {
        let path = std::path::PathBuf::from(dir).join("rdcore-banner.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }
}

/// 窗口线程：注册类、建置顶窗口与托盘图标、跑消息循环直到退出。
unsafe fn run_window(
    state: Arc<Mutex<BannerState>>,
    hwnd_slot: Arc<Mutex<Option<usize>>>,
    qr_code: Option<String>,
) {
    diag("run_window: entered");
    let class_name = w("RdCoreBanner");
    let qr_class_name = w("RdCoreQrPairing");

    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(wnd_proc);
    wc.hInstance = 0; // 顶层弹窗类用调用进程模块即可
    wc.lpszClassName = class_name.as_ptr();
    if RegisterClassW(&wc) == 0 {
        diag(&format!("run_window: RegisterClassW(RdCoreBanner) failed, err={}", GetLastError()));
        return;
    }

    if qr_code.is_some() {
        let mut qwc: WNDCLASSW = std::mem::zeroed();
        qwc.lpfnWndProc = Some(qr_wnd_proc);
        qwc.hInstance = 0;
        qwc.lpszClassName = qr_class_name.as_ptr();
        qwc.hbrBackground = (COLOR_WINDOW + 1) as isize;
        if RegisterClassW(&qwc) == 0 {
            diag(&format!("run_window: RegisterClassW(RdCoreQrPairing) failed, err={}", GetLastError()));
            return;
        }
    }

    // 把共享状态指针作为 lpParam 传入，建窗后在 WM_NCCREATE 里存进 GWLP_USERDATA。
    let state_ptr = Arc::as_ptr(&state) as *const Mutex<BannerState> as *mut std::ffi::c_void;

    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let height = 30;
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST,
        class_name.as_ptr(),
        null(),
        WS_POPUP,
        0,
        0,
        screen_w,
        height,
        0, // hWndParent = NULL
        0, // hMenu = NULL
        0, // hInstance = NULL（调用进程模块）
        state_ptr,
    );
    if hwnd == 0 {
        diag(&format!("run_window: CreateWindowExW failed, err={}", GetLastError()));
        return;
    }
    diag(&format!("run_window: banner window created hwnd={hwnd}"));

    *hwnd_slot.lock().unwrap() = Some(hwnd as usize);
    ShowWindow(hwnd, SW_SHOW);
    UpdateWindow(hwnd);

    // 托盘图标：右下角通知区常驻，回调进本窗口的 WM_TRAYICON。
    tray_add(hwnd);

    // Explorer 重启（崩溃 / 重启资源管理器）后任务栏重建，托盘图标被 shell 丢弃；
    // 监听 TaskbarCreated 广播，届时在 wnd_proc 里重放 NIM_ADD 找回图标。
    let tbc = RegisterWindowMessageW(w("TaskbarCreated").as_ptr());
    TASKBAR_CREATED_MSG.with(|c| c.set(tbc));

    // 配对二维码弹窗：创建后立即自动弹出并置前——Host 启动（含安装完成首次启动）
    // 即见二维码引导扫码；关闭（X）仅隐藏回托盘，之后经托盘左键/右键菜单可再次打开。
    if let Some(code) = qr_code {
        let qr_hwnd = create_qr_window(qr_class_name.as_ptr(), &code);
        diag(&format!("run_window: qr window hwnd={qr_hwnd}, err={}", GetLastError()));
        QR_HWND.with(|c| c.set(qr_hwnd));
        if qr_hwnd != 0 {
            ShowWindow(qr_hwnd, SW_SHOW);
            UpdateWindow(qr_hwnd);
            SetForegroundWindow(qr_hwnd);
        }
    }

    diag("run_window: entering message loop");
    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) != 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    diag("run_window: message loop exited");
    // 窗口（含托盘）已销毁：请求整个进程退出，唤醒主线程阻塞的 IPC recv。
    // 否则窗口没了进程却残留并占用横幅端口，下次启动 bind 失败 → 托盘图标再也出不来。
    crate::request_quit();
}

/// 加载受控端品牌图标（仓库根 `icon.ico`，由构建脚本拷到 exe 同目录；安装包也随附）。
///
/// 优先从 exe 同目录读取 `icon.ico`；若文件缺失（老版本/便携运行）则回退系统默认
/// 应用图标，保证托盘永不空白。返回 HICON（可能为 0，调用方应判空）。
unsafe fn load_brand_icon() -> isize {
    let mut buf = [0u16; 1024];
    let n = GetModuleFileNameW(0, buf.as_mut_ptr(), buf.len() as u32);
    if n > 0 && (n as usize) < buf.len() {
        let exe = String::from_utf16_lossy(&buf[..n as usize]);
        if let Some(dir) = std::path::Path::new(&exe).parent() {
            let ico = dir.join("icon.ico");
            if let Some(p) = ico.to_str() {
                let wpath: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
                let h = LoadImageW(
                    0,
                    wpath.as_ptr(),
                    IMAGE_ICON,
                    0,
                    0,
                    LR_LOADFROMFILE | LR_DEFAULTCOLOR | LR_DEFAULTSIZE,
                );
                if h != 0 {
                    return h;
                }
            }
        }
    }
    LoadIconW(0, IDI_APPLICATION)
}

/// 添加托盘图标（双击/右键行为见 wnd_proc 的 WM_TRAYICON 分支）。
unsafe fn tray_add(hwnd: isize) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = load_brand_icon();
    let tip = w("RdCore 远程桌面受控端 — 点击查看配对二维码");
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    let ok = Shell_NotifyIconW(NIM_ADD, &nid);
    diag(&format!(
        "tray_add: Shell_NotifyIconW(NIM_ADD) -> {ok}, hIcon={}, err={}",
        nid.hIcon,
        GetLastError()
    ));
}

/// 移除托盘图标（进程退出前必须调用，否则图标残留）。
unsafe fn tray_remove(hwnd: isize) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// 创建配对二维码窗口（初始隐藏，由调用方决定何时 ShowWindow；配对码经 lpCreateParams
/// 传入，窗口自持有）。
unsafe fn create_qr_window(class: *const u16, code: &str) -> isize {
    // 窗口自持配对码：堆上 Box，WM_NCDESTROY 时回收。
    let code_box = Box::into_raw(Box::new(code.to_string()));
    CreateWindowExW(
        0,
        class,
        w("RdCore 配对二维码").as_ptr(),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        460,
        640,
        0,
        0,
        0,
        code_box as *mut std::ffi::c_void,
    )
}

/// 显示 / 隐藏二维码弹窗（`show=None` 表示切换）。
fn toggle_qr_window(show: Option<bool>) {
    QR_HWND.with(|c| {
        let h = c.get();
        if h == 0 {
            return;
        }
        unsafe {
            let visible = IsWindowVisible(h) != 0;
            if show.unwrap_or(!visible) {
                ShowWindow(h, SW_SHOW);
                SetForegroundWindow(h);
            } else {
                ShowWindow(h, SW_HIDE);
            }
        }
    });
}

/// 横幅窗口过程：画状态条；处理托盘回调与关闭。
unsafe extern "system" fn wnd_proc(
    hwnd: isize,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Explorer 重启后收到 TaskbarCreated 广播：任务栏已重建，重放 NIM_ADD 找回托盘图标。
    if msg != 0 && msg == TASKBAR_CREATED_MSG.with(|c| c.get()) {
        tray_add(hwnd);
        return 0;
    }
    match msg {
        WM_NCCREATE => {
            let cs = lparam as *const CREATESTRUCTW;
            let lp = (*cs).lpCreateParams;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lp as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr != 0 {
                let m = &*(ptr as *const Mutex<BannerState>);
                if let Ok(s) = m.lock() {
                    paint(hwnd, &s);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TRAYICON => {
            match lparam as u32 {
                // 左键单击：切换二维码弹窗。
                x if x == WM_LBUTTONUP => toggle_qr_window(None),
                // 右键：菜单（显示二维码 / 退出横幅）。
                x if x == WM_RBUTTONUP => {
                    let menu = CreatePopupMenu();
                    if menu != 0 {
                        AppendMenuW(menu, MF_STRING, ID_TRAY_SHOW_QR, w("显示配对二维码").as_ptr());
                        AppendMenuW(menu, MF_STRING, ID_TRAY_EXIT, w("退出").as_ptr());
                        let mut pt: POINT = std::mem::zeroed();
                        GetCursorPos(&mut pt);
                        // Win32 要求弹菜单前先置前台，否则菜单不收 ESC/外点。
                        SetForegroundWindow(hwnd);
                        let cmd = TrackPopupMenu(
                            menu,
                            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                            pt.x,
                            pt.y,
                            0,
                            hwnd,
                            null(),
                        );
                        DestroyMenu(menu);
                        match cmd as usize {
                            ID_TRAY_SHOW_QR => toggle_qr_window(Some(true)),
                            ID_TRAY_EXIT => {
                                PostMessageW(hwnd, WM_CLOSE, 0, 0);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            tray_remove(hwnd);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 用共享状态重绘横幅客户区（深色底 + 浅色单行摘要）。
unsafe fn paint(hwnd: isize, s: &BannerState) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rect);

    let brush = CreateSolidBrush(rgb(18, 18, 20));
    FillRect(hdc, &rect, brush);

    let text = w(&crate::banner_summary(s));
    SetTextColor(hdc, rgb(235, 235, 235));
    SetBkMode(hdc, TRANSPARENT as i32);
    DrawTextW(
        hdc,
        text.as_ptr(),
        -1,
        &mut rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    DeleteObject(brush);
    EndPaint(hwnd, &ps);
}

/// 配对二维码窗口过程：白底绘制 QR 矩阵 + 配对码文本；关闭仅隐藏（进程由横幅生命周期管理）。
unsafe extern "system" fn qr_wnd_proc(
    hwnd: isize,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lparam as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 左键点击：若命中底部「提示+配对码」文本区，则将配对码复制到剪贴板。
        WM_LBUTTONDOWN => {
            let mut pt: POINT = std::mem::zeroed();
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
            let mut cr: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut cr);
            let r = qr_text_rect(&cr);
            if pt.x >= r.left && pt.x <= r.right && pt.y >= r.top && pt.y <= r.bottom {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if ptr != 0 {
                    let code = &*(ptr as *const String);
                    copy_to_clipboard(hwnd, code);
                    QR_COPIED.with(|c| c.set(true));
                    InvalidateRect(hwnd, null(), TRUE);
                }
            }
            0
        }
        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr != 0 {
                let code = &*(ptr as *const String);
                paint_qr(hwnd, code);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_NCDESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ptr != 0 {
                drop(Box::from_raw(ptr as *mut String));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            QR_HWND.with(|c| c.set(0));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 绘制二维码窗口内容：白底 + 居中 QR 矩阵 + 配对码与提示文本。
unsafe fn paint_qr(hwnd: isize, code: &str) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rect: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rect);

    let white = CreateSolidBrush(rgb(255, 255, 255));
    FillRect(hdc, &rect, white);

    // QR 矩阵（字节模式；配对码 ~97 字符 → 约 57×57 模块）。
    let text_h = 200; // 底部文本区高度（放大以完整显示配对码）
    if let Ok(qr) = qrcode::QrCode::new(code.as_bytes()) {
        let m = qr.width() as i32;
        let quiet = 4;
        let span = m + quiet * 2;
        let avail_w = rect.right - rect.left - 24;
        let avail_h = rect.bottom - rect.top - text_h - 24;
        let scale = (avail_w / span).min(avail_h / span).max(1);
        let ox = rect.left + (rect.right - rect.left - span * scale) / 2 + quiet * scale;
        let oy = rect.top + 12 + quiet * scale;

        let black = CreateSolidBrush(rgb(0, 0, 0));
        let colors = qr.into_colors();
        for (i, c) in colors.iter().enumerate() {
            if *c == qrcode::Color::Dark {
                let x = ox + (i as i32 % m) * scale;
                let y = oy + (i as i32 / m) * scale;
                let cell = RECT {
                    left: x,
                    top: y,
                    right: x + scale,
                    bottom: y + scale,
                };
                FillRect(hdc, &cell, black);
            }
        }
        DeleteObject(black);
    }

    // 文本区（提示 + 配对码），点击此区域即复制配对码。
    let tr = qr_text_rect(&rect);
    let segoe = make_font("Segoe UI", -14, 400);
    let default_font = SelectObject(hdc, segoe);

    // 提示文案（复制后给出反馈）。
    let hint = if QR_COPIED.with(|c| c.get()) {
        "✦ 配对码已复制到剪贴板".to_string()
    } else {
        "用控制端 App 扫码，或点击下方配对码复制到剪贴板：".to_string()
    };
    let mut hint_r = RECT {
        left: tr.left,
        top: tr.top,
        right: tr.right,
        bottom: tr.top + 44,
    };
    SetTextColor(hdc, rgb(90, 90, 95));
    SetBkMode(hdc, TRANSPARENT as i32);
    DrawTextW(
        hdc,
        w(&hint).as_ptr(),
        -1,
        &mut hint_r,
        DT_CENTER | DT_WORDBREAK,
    );

    // 配对码：等宽字体。原串无空格，DT_WORDBREAK 不会断行 → 整串被当超长单词横向溢出被裁。
    // 这里每 33 字符手动插入换行：97 字符恰好 3 行（32hex: + 32 + 32），冒号落在第 1 行末，对齐美观；
    // 每行 ≤33 字符（Consolas -16 约 280px，远小于可用 428px）完整显示。点击复制仍用原始 code。
    let code_wrapped: String = code
        .chars()
        .collect::<Vec<_>>()
        .chunks(33)
        .map(|c| c.iter().copied().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let consolas = make_font("Consolas", -16, 400);
    SelectObject(hdc, consolas);
    SetTextColor(hdc, rgb(20, 20, 24));
    let mut code_r = RECT {
        left: tr.left,
        top: tr.top + 52,
        right: tr.right,
        bottom: tr.bottom,
    };
    DrawTextW(
        hdc,
        w(&code_wrapped).as_ptr(),
        -1,
        &mut code_r,
        DT_CENTER | DT_WORDBREAK,
    );

    // 还原默认字体并释放我们创建的字体。
    SelectObject(hdc, default_font);
    DeleteObject(segoe);
    DeleteObject(consolas);

    DeleteObject(white);
    EndPaint(hwnd, &ps);
}

/// 计算底部「提示 + 配对码」文本区矩形（客户区坐标）；绘制与点击命中共用，保证一致。
fn qr_text_rect(client: &RECT) -> RECT {
    let text_h = 200;
    RECT {
        left: client.left + 16,
        top: client.bottom - text_h + 12,
        right: client.right - 16,
        bottom: client.bottom - 12,
    }
}

/// 创建逻辑字体（face 为字体名；height 为负表示字符高度；weight 为 FW_* 值）。
unsafe fn make_font(face: &str, height: i32, weight: i32) -> isize {
    let mut lf: LOGFONTW = std::mem::zeroed();
    lf.lfHeight = height;
    lf.lfWeight = weight;
    lf.lfCharSet = DEFAULT_CHARSET as u8;
    lf.lfQuality = DEFAULT_QUALITY as u8;
    let fw = w(face);
    let n = fw.len().min(lf.lfFaceName.len());
    lf.lfFaceName[..n].copy_from_slice(&fw[..n]);
    CreateFontIndirectW(&lf)
}

/// 复制 UTF-16 文本到系统剪贴板（CF_UNICODETEXT）。
unsafe fn copy_to_clipboard(hwnd: isize, text: &str) {
    if OpenClipboard(hwnd) == 0 {
        return;
    }
    EmptyClipboard();
    let data = w(text); // NUL 结尾的 UTF-16
    let size = (data.len() * 2) as usize;
    // GlobalAlloc 返回 HGLOBAL(=*mut c_void)，统一转 isize 便于后续作为 HANDLE 传递。
    let h: isize = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size) as isize;
    if h != 0 {
        let hp = h as *mut std::ffi::c_void;
        let p = GlobalLock(hp) as *mut u16;
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            GlobalUnlock(hp);
            SetClipboardData(CF_UNICODETEXT as u32, h);
        }
    }
    CloseClipboard();
}
