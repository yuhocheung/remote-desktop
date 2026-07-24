import 'dart:io';

/// 受控端（Host）对外展示的身份名：操作系统 + 电脑名。
///
/// 形如 "Windows 11 - Yuho'pc"。仅 Host 侧在建立本地身份（[LocalIdentity]）时使用；
/// Viewer 收到后经由安全指示器原样显示为「对端」名称，无需改动 Rust / FFI 符号。
///
/// 实现说明：
/// - 电脑名取自 [Platform.localHostname]（Windows 电脑名 / macOS hostname），缺失时回退到
///   环境变量 COMPUTERNAME / HOSTNAME。
/// - 操作系统：Windows 通过 `cmd /c ver` 解析 build 号区分 Windows 10 / 11（build >= 22000 即
///   Windows 11）；其余平台用 [Platform.operatingSystem] 的友好名。全部 best-effort，失败回退到
///   平台原始标识，保证永不抛异常。
Future<String> hostPeerLabel() async {
  final computerName = _computerName();
  final os = await _friendlyOs();
  return '$os - $computerName';
}

String _computerName() {
  try {
    final h = Platform.localHostname;
    if (h.isNotEmpty) return h;
  } on Object {
    // 极少数平台 localHostname 不可用，忽略。
  }
  return Platform.environment['COMPUTERNAME'] ??
      Platform.environment['HOSTNAME'] ??
      '未知设备';
}

Future<String> _friendlyOs() async {
  final os = Platform.operatingSystem;
  switch (os) {
    case 'windows':
      // build >= 22000 即 Windows 11；否则 Windows 10。
      final ver = await _windowsVersion();
      return ver ?? 'Windows';
    case 'macos':
      return 'macOS';
    case 'linux':
      return 'Linux';
    case 'android':
      return 'Android';
    case 'ios':
      return 'iOS';
    default:
      return os;
  }
}

Future<String?> _windowsVersion() async {
  try {
    final r = await Process.run('cmd', const ['/c', 'ver']);
    final out = (r.stdout as String?) ?? '';
    // 形如 "Microsoft Windows [Version 10.0.22621.1]"
    final m = RegExp(r'Version\s+(\d+)\.(\d+)\.(\d+)').firstMatch(out);
    if (m != null) {
      final major = int.tryParse(m.group(1)!);
      final build = int.tryParse(m.group(3)!);
      if (major == 10 && build != null && build >= 22000) return 'Windows 11';
      if (major == 10) return 'Windows 10';
    }
  } on Object {
    // 命令不可用 / 解析失败，交回退逻辑处理。
  }
  return null;
}
