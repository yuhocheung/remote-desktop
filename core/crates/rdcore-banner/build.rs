//! 构建脚本：把品牌图标 `icon.ico` 拷到编译产物同目录，
//! 使 `rdcore-banner.exe` 运行时能经 `GetModuleFileNameW` 找到它作为托盘图标。
//! 图标源 = 仓库根目录的 `icon.ico`（单一事实来源；由用户在项目根放置）。
//! 非 Windows 平台跳过（图标仅 Windows 托盘使用）。

fn main() {
    #[cfg(windows)]
    {
        if let (Ok(manifest), Ok(out)) = (
            std::env::var("CARGO_MANIFEST_DIR"),
            std::env::var("OUT_DIR"),
        ) {
            // CARGO_MANIFEST_DIR = <repo>/core/crates/rdcore-banner → 上三级即仓库根。
            let repo_root = std::path::Path::new(&manifest)
                .join("..")
                .join("..")
                .join("..");
            let ico = repo_root.join("icon.ico");
            if ico.exists() {
                // OUT_DIR = <workspace>/target/<profile>/build/<pkg>-<hash>/out
                // 回退三级即到达 <workspace>/target/<profile>，与最终 exe 同目录。
                let bin_dir = std::path::Path::new(&out).join("..").join("..").join("..");
                let _ = std::fs::create_dir_all(&bin_dir);
                let _ = std::fs::copy(&ico, bin_dir.join("icon.ico"));
            }
        }
    }
    // 非 Windows：无需任何资源处理。
}
