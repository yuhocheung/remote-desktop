#Requires -Version 5.1
<#
.SYNOPSIS
  构建「低驱动地板」FFmpeg 硬件编码开发库（ffmpeg 7.1.2 + nv-codec-headers 12.2.72.0）。

.DESCRIPTION
  背景：ffmpeg 8.1 的 h264_nvenc 要求 NVENC API 13.1（NVIDIA 驱动 >= 610.00），
  老驱动用户会静默回退到 openh264 软编。本脚本用钉死的 vcpkg 基线编译：
    - ffmpeg        7.1.2
    - ffnvcodec     12.2.72.0   -> NVENC API 12.2 -> 最低驱动 551.76（2024-03）
    - amd-amf / mfx-dispatch    -> h264_amf / h264_qsv 一并构建
  NVENC 向后兼容：旧头文件编译的客户端在新驱动上照常工作，
  因此本构建同时覆盖 551.76 以上的老驱动与最新驱动。

  产物布局与 gyan full-shared 一致（include/lib/bin），直接可用 FFMPEG_DIR 指向。

.PARAMETER VcpkgRoot
  vcpkg 检出目录（默认 C:\dev\vcpkg-ffmpeg712）。约占用 1.5 GB。

.PARAMETER OutDir
  开发库输出目录（默认 C:\ffmpeg-7.1.2-lowfloor-dev）。FFMPEG_DIR 指向它。

.PARAMETER Triplet
  vcpkg triplet（默认 x64-windows）。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\build_ffmpeg_lowfloor.ps1

.NOTES
  前置：git、curl（Win10+ 自带 tar/curl）、Visual Studio 2019+ Build Tools
  （含 C++ 工作负载；若 vcpkg 报 English language pack 缺失，请在 VS Installer
  的「语言包」页勾选 English 后重跑）。构建全程联网，约 20-40 分钟。
#>
param(
    [string]$VcpkgRoot = "C:\dev\vcpkg-ffmpeg712",
    [string]$OutDir    = "C:\ffmpeg-7.1.2-lowfloor-dev",
    [string]$Triplet   = "x64-windows"
)

$ErrorActionPreference = "Stop"

# 钉死的 vcpkg 基线（2025-10-13，commit 全量 SHA，已核实端口版本）：
#   ports/ffmpeg/vcpkg.json    -> 7.1.2
#   ports/ffnvcodec/vcpkg.json -> 12.2.72.0
$Baseline = "34823ada10080ddca99b60e85f80f55e18a44eea"

# 与现有 gyan full-shared(8.1) 对齐的特性面：默认特性(avcodec/avdevice/avfilter/
# avformat/swresample/swscale) + 硬件三件套。不加 gpl/nonfree，保持 LGPL 动态链接合规。
$Spec = "ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,amf,nvcodec,qsv]:$Triplet"

# 终端不继承 IE 系统代理，而 curl/vcpkg 的下载默认直连（本机即因此下载失败）。
# 未显式设置代理变量时，自动从注册表读取系统代理（仅处理 host:port 简单形式；
# 分协议/高级形式请运行前自行 $env:HTTPS_PROXY = "..." 设置）。
if (-not $env:HTTPS_PROXY -and -not $env:HTTP_PROXY) {
    try {
        $ie = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" -ErrorAction Stop
        if ($ie.ProxyEnable -eq 1 -and $ie.ProxyServer -and $ie.ProxyServer -notmatch '[=;]') {
            $env:HTTP_PROXY = "http://$($ie.ProxyServer)"
            $env:HTTPS_PROXY = "http://$($ie.ProxyServer)"
            Write-Host "    检测到系统代理，下载将走 $($ie.ProxyServer)"
        }
    } catch { }
}

function Invoke-Curl {
    param([string]$Url, [string]$Out)
    for ($i = 1; $i -le 6; $i++) {
        & curl.exe -fSL --connect-timeout 20 -o $Out $Url
        if ($LASTEXITCODE -eq 0) { return }
        Write-Warning "下载失败（第 $i 次），2s 后重试: $Url"
        Start-Sleep -Seconds 2
    }
    throw "下载失败（已重试 6 次）: $Url"
}

Write-Host "==> [1/5] 获取 vcpkg 基线 $Baseline"
if (-not (Test-Path "$VcpkgRoot\bootstrap-vcpkg.bat")) {
    $zip = Join-Path $env:TEMP "vcpkg-$Baseline.zip"
    #  tarball 下载比 git clone 全量历史轻得多；SHA 钉死保证可复现。
    Invoke-Curl -Url "https://github.com/microsoft/vcpkg/archive/$Baseline.zip" -Out $zip
    $stage = Join-Path $env:TEMP "vcpkg-$Baseline"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    # Win10+ 自带 bsdtar，比 Expand-Archive 快数倍。
    & tar.exe -xf $zip -C $env:TEMP
    if ($LASTEXITCODE -ne 0) { throw "解压 vcpkg 基线包失败" }
    New-Item -ItemType Directory -Force -Path (Split-Path $VcpkgRoot) | Out-Null
    Move-Item $stage $VcpkgRoot
    Remove-Item $zip -Force
} else {
    Write-Host "    已存在，跳过下载（$VcpkgRoot）"
}

Write-Host "==> [2/5] 引导 vcpkg"
if (-not (Test-Path "$VcpkgRoot\vcpkg.exe")) {
    & "$VcpkgRoot\bootstrap-vcpkg.bat" -disableMetrics
    if ($LASTEXITCODE -ne 0) { throw "bootstrap-vcpkg 失败" }
}

Write-Host "==> [3/5] vcpkg install $Spec（20-40 分钟；下载损坏由弹性循环自动预置重试）"
& (Join-Path $PSScriptRoot "_vcpkg_resilient_install.ps1") -VcpkgRoot $VcpkgRoot -Spec $Spec
if ($LASTEXITCODE -ne 0) { throw "vcpkg install 失败" }

Write-Host "==> [4/5] 汇聚产物 -> $OutDir"
$inst = Join-Path $VcpkgRoot "installed\$Triplet"
foreach ($d in @("include", "lib", "bin")) {
    $src = Join-Path $inst $d
    $dst = Join-Path $OutDir $d
    & robocopy $src $dst /E /NFL /NDL /NJH /NJS | Out-Null
    # robocopy 退出码 0-7 均为成功；>=8 才是错误。
    if ($LASTEXITCODE -ge 8) { throw "robocopy 失败 ($src -> $dst, code $LASTEXITCODE)" }
    $global:LASTEXITCODE = 0
}

Write-Host "==> [5/5] 校验关键文件"
$expect = @(
    "include\libavcodec\avcodec.h",
    "include\ffnvcodec\nvEncodeAPI.h",
    "lib\avcodec.lib",
    "bin\avcodec-61.dll",
    "bin\avutil-59.dll"
)
$missing = $expect | Where-Object { -not (Test-Path (Join-Path $OutDir $_)) }
if ($missing) { throw "产物校验失败，缺少: $($missing -join ', ')" }

Write-Host ""
Write-Host "构建完成。接入工程（新终端）：" -ForegroundColor Green
Write-Host "  setx FFMPEG_DIR `"$OutDir`""
Write-Host "当前终端立即生效："
Write-Host "  `$env:FFMPEG_DIR = `"$OutDir`""
Write-Host "  `$env:PATH = `"$OutDir\bin;`$env:PATH`""
Write-Host "然后把 core/crates/rdcore-encode/Cargo.toml 中 ffmpeg-next 版本由 `"8`" 改为 `"7`"，"
Write-Host "执行 cargo clean -p ffmpeg-sys-next 后重新构建，详见 docs/ffmpeg_hw_lowfloor.md"
