#!/usr/bin/env python3
"""从 Rust Host Agent 的 config.rs 同步 ICE/信令默认配置到 Flutter。

为什么存在这个脚本：
  Flutter 是独立编译的 App，运行时无法读取 Rust 的 `config.rs` 或部署用的
  `.env`。为避免「联调 VPS 地址/凭据」在 Rust 与 Flutter 两侧各抄一份导致漂移，
  约定 `core/crates/rdcore-desktop/src/config.rs` 里的 `pub const` 为**单一来源**，
  本脚本把它解析出来，生成 `flutter/lib/models/default_config.dart` 供 Flutter 引用。

用法：
  python3 tool/sync_flutter_config.py

接入：
  justfile 的 `build-flutter` 会在 `flutter build` 之前自动调用本脚本（recipe `sync-config`）。
  日常改了 config.rs 的默认值后，重新 `just build-flutter` 或手动跑本脚本即可。

注意：生成的 default_config.dart 会提交进 git（保证不跑脚本也能编译）；CI 可加一步比对，
确认它与 config.rs 一致（详见各 generate 校验思路）。
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = REPO_ROOT / "core" / "crates" / "rdcore-desktop" / "src" / "config.rs"
OUT_DART = REPO_ROOT / "flutter" / "lib" / "models" / "default_config.dart"

# config.rs 的 const 名 -> 生成的 Dart 常量名（均为 &str 字面量）
MAP = {
    "DEFAULT_SIGNALING": "kDefaultSignalingBaseUrl",
    "DEFAULT_STUN": "kDefaultStunUrl",
    "DEFAULT_TURN_URL": "kDefaultTurnUrl",
    "DEFAULT_TURN_USER": "kDefaultTurnUser",
    "DEFAULT_TURN_PASS": "kDefaultTurnPass",
}

# 匹配：pub const NAME: &str = "value";
CONST_RE = re.compile(
    r'pub\s+const\s+(?P<name>[A-Z_][A-Z0-9_]*)\s*:\s*&str\s*=\s*"(?P<value>(?:[^"\\]|\\.)*)"\s*;'
)


def main() -> int:
    if not CONFIG_RS.is_file():
        print(f"[sync-flutter-config] 找不到 {CONFIG_RS}", file=sys.stderr)
        return 1

    text = CONFIG_RS.read_text(encoding="utf-8")
    found: dict[str, str] = {}
    for m in CONST_RE.finditer(text):
        name = m.group("name")
        if name in MAP:
            # 反转义 Dart 字符串里的反斜杠/引号（config.rs 这里都是普通 IP/URL，基本无转义）
            value = m.group("value").encode().decode("unicode_escape")
            found[name] = value

    missing = [k for k in MAP if k not in found]
    if missing:
        print(
            f"[sync-flutter-config] config.rs 缺少预期常量: {missing}",
            file=sys.stderr,
        )
        return 1

    lines = [
        "// GENERATED FILE — DO NOT EDIT BY HAND.",
        "// 由 tool/sync_flutter_config.py 从",
        "// core/crates/rdcore-desktop/src/config.rs 的 `pub const` 生成。",
        "// 改默认值请改 config.rs，然后重跑脚本（或 `just build-flutter`）。",
        "//",
        "// 这是联调用 VPS 的默认信令/STUN/TURN 配置；生产环境应在 App「设置」页",
        "// 覆盖，或经安全配置通道下发，切勿依赖此硬编码值。",
        "",
        "// 信令服务器基址（不含路径与查询）。",
        f"const String {MAP['DEFAULT_SIGNALING']} = '{found['DEFAULT_SIGNALING']}';",
        "// STUN 服务器 URL。",
        f"const String {MAP['DEFAULT_STUN']} = '{found['DEFAULT_STUN']}';",
        "// TURN 中继 URL。",
        f"const String {MAP['DEFAULT_TURN_URL']} = '{found['DEFAULT_TURN_URL']}';",
        "// TURN 用户名。",
        f"const String {MAP['DEFAULT_TURN_USER']} = '{found['DEFAULT_TURN_USER']}';",
        "// TURN 凭据（联调静态共享凭据；生产应改为动态凭据）。",
        f"const String {MAP['DEFAULT_TURN_PASS']} = '{found['DEFAULT_TURN_PASS']}';",
        "",
    ]

    OUT_DART.parent.mkdir(parents=True, exist_ok=True)
    OUT_DART.write_text("\n".join(lines), encoding="utf-8")
    print(f"[sync-flutter-config] 已生成 {OUT_DART}")
    for k in MAP:
        print(f"  {MAP[k]} = {found[k]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
