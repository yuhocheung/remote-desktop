// Track B（kimi-k3）配对 / 发现（B3）。
//
// Host 生成配对邀请（session_id + token）→ 展示配对码/二维码，并经 [PairingClient.publishInvite]
// 发布到共享 token 库文件（A5↔B2 对接点，含心跳保鲜）；
// Viewer 输码/扫码 → 解析出 session_id + token → 带 token 连信令。
// 配对不焚毁：受控端在线期间可重复扫码建连；主动取消 / 刷新 / 退出即失效。
// token 格式见计划 §5.1：session_id 16B（hex 32 字符）、token 32B（hex 64 字符）。
import 'dart:ffi' as ffi;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import '../ffi/rdcore_bindings.dart';

/// 一次配对邀请（Host 侧展示，Viewer 侧输入）。
class PairingInvite {
  const PairingInvite({required this.sessionId, required this.token});

  /// 16 字节会话 ID。
  final Uint8List sessionId;

  /// 64 字符小写 hex token。
  final String token;

  /// session_id 的 32 字符小写 hex 展示（信令 URL 路径 / 配对码用）。
  String get sessionHex =>
      sessionId.map((b) => b.toRadixString(16).padLeft(2, '0')).join();

  /// 配对码（输码形态）：`<32hex session_id>:<64hex token>`。
  String get code => '$sessionHex:$token';

  /// 信令 WebSocket URL（wss）。
  ///
  /// 服务端从路径**第一段**取 session_id（见 signaling-svc 的
  /// `path().trim_start_matches('/').split('/').next()`），故必须是
  /// `wss://host/<32hex>?token=<64hex>`，**不能**再加 `signaling/` 前缀段
  /// （否则服务端会把前缀段误当 session_id 而 400 拒绝）。
  String signalingUrl(String host) => 'wss://$host/$sessionHex?token=$token';

  /// 信令 WebSocket URL，从含协议的基址构造（支持 `ws://` / `wss://`）。
  ///
  /// 与 [signalingUrl] 的区别：后者硬编码 `wss://` 适合生产域名；本方法接受
  /// `ws://127.0.0.1:8080` 这类本地开发基址，保留协议灵活性。服务端从路径
  /// **第一段**取 session_id（见 signaling-svc 的
  /// `path().trim_start_matches('/').split('/').next()`），故 baseUrl 不得
  /// 含额外路径段（如 `signaling/`），否则该段会被误当 session_id 而 400。
  String signalingUrlFromBase(String baseUrl) =>
      '$baseUrl/$sessionHex?token=$token';

  /// 从配对码解析（输码路径）。非法格式返回 null。
  static PairingInvite? parse(String code) {
    final parts = code.trim().split(':');
    if (parts.length != 2) return null;
    final sid = _fromHex(parts[0], 16);
    final token = parts[1];
    if (sid == null) return null;
    if (token.length != 64 || !_isLowerHex(token)) return null;
    return PairingInvite(sessionId: sid, token: token);
  }

  static bool _isLowerHex(String s) =>
      s.codeUnits.every((c) => (c >= 0x30 && c <= 0x39) || (c >= 0x61 && c <= 0x66));

  static Uint8List? _fromHex(String s, int bytes) {
    if (s.length != bytes * 2 || !_isLowerHex(s)) return null;
    final out = Uint8List(bytes);
    for (var i = 0; i < bytes; i++) {
      out[i] = int.parse(s.substring(i * 2, i * 2 + 2), radix: 16);
    }
    return out;
  }
}

/// 配对封装：生成邀请（Host）/ 解析（Viewer）。
class PairingClient {
  PairingClient(this._lib);

  final RdCoreLib _lib;

  /// Host 生成配对邀请。原生库不可用时抛 [RdCoreNativeUnavailable]。
  PairingInvite createInvite() {
    _lib.check();
    final p = _lib.createPairing();
    if (p.address == 0) {
      throw StateError(_lib.takeLastError() ?? 'create_pairing failed');
    }
    final sid = Uint8List.fromList(
        List<int>.generate(16, (i) => p.ref.sessionId[i]));
    final token = p.ref.token.toDartString();
    _lib.pairingInfoFree(p);
    return PairingInvite(sessionId: sid, token: token);
  }

  /// 受控端发布配对：写入共享 token 库文件并启动心跳保鲜。
  /// 信令侧以文件为事实——发布后配对码即可被扫码连接；重复发布即「刷新二维码」，
  /// 旧配对随即失效。原生库过旧（缺符号）或写文件失败时抛 [StateError]。
  void publishInvite(PairingInvite invite) {
    _lib.check();
    final publish = _lib.pairingPublish;
    if (publish == null) {
      throw StateError('当前原生库不支持配对发布/取消（请重新构建 rdcore-ffi）');
    }
    final sidPtr = calloc<ffi.Uint8>(16);
    final tokPtr = invite.token.toNativeUtf8();
    try {
      for (var i = 0; i < 16; i++) {
        sidPtr[i] = invite.sessionId[i];
      }
      final ok = publish(sidPtr, tokPtr);
      if (ok == 0) {
        throw StateError(_lib.takeLastError() ?? 'pairing_publish failed');
      }
    } finally {
      calloc.free(sidPtr);
      calloc.free(tokPtr);
    }
  }

  /// 受控端撤销配对（主动取消 / 会话结束）：停心跳并删除 token 库文件，
  /// 配对码在下一次握手时失效。幂等；原生库过旧（缺符号）时静默跳过。
  void revokeInvite() {
    if (!_lib.available) return;
    _lib.pairingRevoke?.call();
  }

  /// Viewer 解析配对码（输码）。
  PairingInvite? parse(String code) => PairingInvite.parse(code);
}
