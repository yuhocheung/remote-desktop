#!/usr/bin/env bash
# Track B（kimi-k3）：pub.dev / Flutter 存储的中国大陆镜像配置。
#
# 背景：本环境（及中国大陆网络）下，`https://pub.dev` 的 API 可达，但**包下载 CDN
# `storage.googleapis.com` 被墙**（IPv6 黑洞 / 连接重置），导致 `flutter pub get` 在
# 下载任何**新**包时 socket error（解析已有依赖则因缓存可过）。
#
# 解法：用 flutter-io.cn 镜像同时代理 pub API 与下载 CDN。
#
# 用法（在跑 flutter pub get / flutter test / flutter analyze 前 source）：
#   source scripts/pub_mirror.sh
#   flutter pub get
#
# Windows PowerShell 用户可改用 scripts/pub_mirror.ps1。

export PUB_HOSTED_URL="https://pub.flutter-io.cn"
export FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn"

echo "[pub_mirror] PUB_HOSTED_URL=$PUB_HOSTED_URL"
echo "[pub_mirror] FLUTTER_STORAGE_BASE_URL=$FLUTTER_STORAGE_BASE_URL"
