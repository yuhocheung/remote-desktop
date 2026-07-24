//! Host 硬件编码器能力探测（最佳实践中“运行时探测 + 自动回退”的基石）。
//!
//! 设计原则（对齐 RustDesk 的 `check_hwcodec` 思路）：
//! 1. **探测与编码解耦**——本模块只回答“本机有没有、有哪些硬件 H.264 编码器”，
//!    不触碰任何 GPU 实际编码；结果供 `new_encoder` 做优先级决策。
//! 2. **纯逻辑、可单测**——Windows 下通过 `LoadLibraryW` 探测厂商 DLL 是否存在、
//!    并通过 Media Foundation 枚举硬件 H.264 编码 MFT；非 Windows / 未开 `hwcodec`
//!    一律返回空（保守回退软编），保证默认构建与 CI 不依赖任何 GPU。
//! 3. **vendor-agnostic**——Media Foundation 的硬件 MFT 已抽象 NVIDIA(NVENC) /
//!    Intel(QSV) / AMD(AMF)，故探测到“存在硬件 MFT”即足以启用 MF 后端；厂商 DLL
//!    探测仅用于更丰富的日志 / 协商提示。

/// 检测到的硬件编码器类型（仅用于遥测与日志，不影响后端选择——都走 Media Foundation）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoderKind {
    /// NVIDIA NVENC（`nvencodeapi64.dll`）。
    Nvenc,
    /// Intel QuickSync（`mfxplugin64*.dll`）。
    Qsv,
    /// AMD AMF（`amfrt64.dll`）。
    Amf,
    /// Media Foundation 硬件编码 MFT（由系统 GPU 驱动暴露，厂商无关）。
    MediaFoundation,
}

impl HwEncoderKind {
    /// 人类可读标识（用于日志 / 协商能力上报）。
    pub fn as_str(&self) -> &'static str {
        match self {
            HwEncoderKind::Nvenc => "nvenc",
            HwEncoderKind::Qsv => "qsv",
            HwEncoderKind::Amf => "amf",
            HwEncoderKind::MediaFoundation => "mf",
        }
    }
}

/// 探测本机可用的硬件 H.264 编码器列表（按优先级排序：厂商专用优先于通用 MF）。
///
/// - `hwcodec` feature + Windows：真实探测（DLL + MF 硬件 MFT 枚举）。
/// - 其它情况：返回空 `Vec`（保守，调用方应回退软编）。
pub fn detect_hw_encoders() -> Vec<HwEncoderKind> {
    #[cfg(all(feature = "hwcodec", windows))]
    {
        detect_hw_encoders_windows()
    }
    #[cfg(not(all(feature = "hwcodec", windows)))]
    {
        Vec::new()
    }
}

/// 是否存在任意硬件编码器（供 `new_encoder` 快速决策）。
pub fn has_hw_encoder() -> bool {
    !detect_hw_encoders().is_empty()
}

#[cfg(all(feature = "hwcodec", windows))]
fn detect_hw_encoders_windows() -> Vec<HwEncoderKind> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;

    let mut kinds: Vec<HwEncoderKind> = Vec::new();

    // 1) 厂商专用 DLL 探测（存在即说明驱动装了对应硬件编码器运行时）。
    let probe = |name: &str| -> bool {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: 仅探测模块是否存在；不触碰内部状态，也不释放句柄（探测用，泄漏可忽略）。
        unsafe { LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())).is_ok() }
    };

    if probe("nvencodeapi64.dll") {
        kinds.push(HwEncoderKind::Nvenc);
    }
    // Intel 的 MFX 插件 DLL 有多种命名（旧 mfxplugin64.dll / 新 libmfx64-*.dll）。
    if probe("mfxplugin64.dll") || probe("mfxplugin64_hw.dll") || probe("libmfx64.dll") {
        kinds.push(HwEncoderKind::Qsv);
    }
    if probe("amfrt64.dll") {
        kinds.push(HwEncoderKind::Amf);
    }

    // 2) Media Foundation 硬件 H.264 编码 MFT 枚举（厂商无关，最终兜底“有 GPU 就能编”）。
    if mf_has_h264_hw_encoder() == Some(true) {
        kinds.push(HwEncoderKind::MediaFoundation);
    }

    kinds
}

/// 通过 Media Foundation 枚举 `MFT_CATEGORY_VIDEO_ENCODER`，确认存在支持 H.264 输出的
/// **硬件** MFT（即 GPU 厂商在系统里注册的硬件编码器）。
#[cfg(all(feature = "hwcodec", windows))]
fn mf_has_h264_hw_encoder() -> Option<bool> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    // SAFETY: 仅做无副作用的枚举查询；MF/COM 初始化失败不代表系统无编码器，返回 None 让调用方保守处理。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        let hr = MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            Some(&input as *const MFT_REGISTER_TYPE_INFO),
            Some(&output as *const MFT_REGISTER_TYPE_INFO),
            None::<&IMFAttributes>,
            &mut pp,
            &mut count,
        );
        if hr.is_err() || pp.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(pp as *const std::ffi::c_void));
            return None;
        }
        let has = count > 0;
        // 逐个“移出”激活器引用（ptr::read 后由其 Drop 自动 Release），数组由 MF 经
        // CoTaskMem 分配，须 CoTaskMemFree 释放。
        let activates = std::slice::from_raw_parts(pp, count as usize);
        for i in 0..(count as usize) {
            // SAFETY: 每个槽位仅被读出一次；随后 CoTaskMemFree 释放数组内存，不再访问。
            let owned = std::ptr::read(activates.as_ptr().add(i));
            drop(owned);
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(pp as *const std::ffi::c_void));
        Some(has)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_runs_and_is_safe_off_hwcodec() {
        // 本机（沙箱无 GPU / 未开 hwcodec）应返回空，且不 panic。
        let kinds = detect_hw_encoders();
        assert!(kinds.is_empty(), "沙箱无 GPU，应探测为空: {:?}", kinds);
        assert!(!has_hw_encoder());
    }

    #[test]
    fn kind_as_str_is_stable() {
        assert_eq!(HwEncoderKind::Nvenc.as_str(), "nvenc");
        assert_eq!(HwEncoderKind::MediaFoundation.as_str(), "mf");
    }
}
