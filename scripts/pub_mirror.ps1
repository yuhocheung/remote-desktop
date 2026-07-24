# Track B（kimi-k3）：pub.dev / Flutter 存储的中国大陆镜像配置（PowerShell）。
#
# 背景见 scripts/pub_mirror.sh。用法（在跑 flutter 命令前执行）：
#   . .\scripts\pub_mirror.ps1
#   flutter pub get

$env:PUB_HOSTED_URL = "https://pub.flutter-io.cn"
$env:FLUTTER_STORAGE_BASE_URL = "https://storage.flutter-io.cn"

Write-Host "[pub_mirror] PUB_HOSTED_URL=$env:PUB_HOSTED_URL"
Write-Host "[pub_mirror] FLUTTER_STORAGE_BASE_URL=$env:FLUTTER_STORAGE_BASE_URL"
