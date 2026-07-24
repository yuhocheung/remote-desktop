#Requires -Version 5.1
<#
.SYNOPSIS
  带「下载损坏自动预置重试」的 vcpkg install 弹性循环（供 build_ffmpeg_lowfloor.ps1 内部调用）。

.DESCRIPTION
  本机代理（127.0.0.1:7888）会随机截断 GitHub 下载流，导致 vcpkg 报
  "unexpected hash"。本脚本循环执行 vcpkg install：每轮失败时解析日志中的
  下载 URL / 文件名 / 期望 SHA512，用 curl 反复下载直到 hash 匹配并预置到
  downloads\ 目录，然后继续下一轮，直到 install 成功。非下载类失败直接抛错。
#>
param(
    [Parameter(Mandatory = $true)][string]$VcpkgRoot,
    [Parameter(Mandatory = $true)][string]$Spec,
    [int]$MaxRounds = 40
)

$ErrorActionPreference = "Stop"

# 与主脚本一致的系统代理自动检测。
if (-not $env:HTTPS_PROXY -and -not $env:HTTP_PROXY) {
    try {
        $ie = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" -ErrorAction Stop
        if ($ie.ProxyEnable -eq 1 -and $ie.ProxyServer -and $ie.ProxyServer -notmatch '[=;]') {
            $env:HTTP_PROXY = "http://$($ie.ProxyServer)"
            $env:HTTPS_PROXY = "http://$($ie.ProxyServer)"
        }
    } catch { }
}

$downloads = Join-Path $VcpkgRoot "downloads"
New-Item -ItemType Directory -Force -Path $downloads | Out-Null

function Save-WithHash {
    param([string]$Url, [string]$Dest, [string]$Sha512)
    for ($i = 1; $i -le 8; $i++) {
        & curl.exe -fsSL --connect-timeout 20 -o $Dest $Url
        if ($LASTEXITCODE -eq 0 -and (Test-Path $Dest)) {
            $actual = (Get-FileHash $Dest -Algorithm SHA512).Hash.ToLower()
            if ($actual -eq $Sha512.ToLower()) { return $true }
        }
        Remove-Item $Dest -Force -ErrorAction SilentlyContinue
        Write-Warning "预置下载第 $i 次 hash 不符/失败，重试: $Url"
        Start-Sleep -Seconds 2
    }
    return $false
}

for ($round = 1; $round -le $MaxRounds; $round++) {
    Write-Host "===== vcpkg install 第 $round/$MaxRounds 轮 ====="
    Get-ChildItem (Join-Path $downloads "*.part") -ErrorAction SilentlyContinue | Remove-Item -Force

    $log = & (Join-Path $VcpkgRoot "vcpkg.exe") install $Spec --recurse 2>&1 | Out-String
    $log | Write-Host
    if ($LASTEXITCODE -eq 0) {
        Write-Host "vcpkg install 成功" -ForegroundColor Green
        exit 0
    }

    # 取日志中最后一次下载尝试与其期望 hash（vcpkg 遇 hash 错误即中止，一轮至多一个）。
    $dm = [regex]::Matches($log, 'Downloading (\S+) -> (\S+)')
    $hm = [regex]::Matches($log, 'Expected:\s*([0-9a-fA-F]{128})')
    if ($dm.Count -eq 0 -or $hm.Count -eq 0) {
        throw "vcpkg install 失败，且不是下载 hash 问题，请检查上方日志"
    }
    $url  = $dm[$dm.Count - 1].Groups[1].Value
    $file = $dm[$dm.Count - 1].Groups[2].Value
    $hash = $hm[$hm.Count - 1].Groups[1].Value

    Write-Host "检测到下载损坏，自动预置: $file"
    if (-not (Save-WithHash -Url $url -Dest (Join-Path $downloads $file) -Sha512 $hash)) {
        throw "预置下载 8 次仍 hash 不匹配: $url"
    }
}

throw "超过 $MaxRounds 轮仍未完成，请检查网络/代理"
