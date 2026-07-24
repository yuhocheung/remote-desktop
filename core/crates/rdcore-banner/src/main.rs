// 纯 GUI 进程（置顶横幅窗口 + 托盘图标 + 托盘二维码弹窗），无任何控制台交互：
// 固定 Windows GUI 子系统，无论从何种父进程拉起都不出现终端窗口。
#![windows_subsystem = "windows"]

fn main() {
    // 横幅进程入口：创建置顶窗口 + 启动 IPC，阻塞直至窗口销毁。
    rdcore_banner::run_banner();
}
