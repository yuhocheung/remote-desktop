import 'dart:convert';
import 'dart:ffi';
import 'dart:typed_data';
import 'package:ffi/ffi.dart';
import '../ffi/rdcore_bindings.dart';
import '../models/peer_identity.dart';
import 'connection_backend.dart';
import 'native_connection.dart';

/// 本机长期身份 + 带外配对 + 会话创建。封装 Rust 端 `RdLocal` 句柄。
///
/// 配对流程（架构文档「带外指纹核对」）：本机展示 [fingerprint] 供对端当面核对，
/// 同时把 [peerJson] 生成二维码让对端扫码；对端扫码得到本机 JSON 后调用
/// [rememberPeer] 导入。只有配对过的对端，握手验签才会通过（防冒充 / MITM）。
class LocalIdentity {
  LocalIdentity._(this.lib, this._handle, this.displayName);

  final RdCoreLib lib;
  final Pointer<Void> _handle;
  final String displayName;

  /// 暴露给 NativeConnection 复用的本机句柄（同进程内，安全）。
  Pointer<Void> get handle => _handle;

  /// 生成新的本机身份。
  factory LocalIdentity.create({required String displayName}) {
    final l = RdCoreLib();
    l.check();
    final namePtr = displayName.toNativeUtf8();
    final h = l.identityNew(namePtr);
    malloc.free(namePtr);
    if (h.address == 0) {
      throw RdCoreException('创建身份失败: ${l.takeLastError() ?? "未知"}');
    }
    return LocalIdentity._(l, h, displayName);
  }

  /// 公钥指纹（空格分隔大写十六进制），用于带外展示给用户核对。
  String get fingerprint {
    final p = lib.localFingerprint(_handle);
    if (p.address == 0) {
      throw RdCoreException('读取指纹失败: ${lib.takeLastError() ?? "未知"}');
    }
    final s = p.toDartString();
    lib.stringFree(p);
    return s;
  }

  /// 本机设备 ID（16 字节）。
  Uint8List get deviceId {
    final p = lib.localDeviceId(_handle);
    if (p.address == 0) throw RdCoreException('读取设备ID失败');
    final b = p.ref.data.asTypedList(p.ref.len);
    final c = Uint8List.fromList(b);
    lib.bytesFree(p);
    return c;
  }

  /// 导出本机身份 JSON（供对端扫码/带外导入）。
  String get peerJson {
    final p = lib.localPeerJson(_handle);
    if (p.address == 0) {
      throw RdCoreException('导出身份失败: ${lib.takeLastError() ?? "未知"}');
    }
    final s = p.toDartString();
    lib.stringFree(p);
    return s;
  }

  /// 本机身份的结构化模型（供 UI 展示 / 带外配对核对）。
  PeerIdentity get peer =>
      PeerIdentity.fromJson(jsonDecode(peerJson) as Map<String, dynamic>);

  /// 导入对端身份 JSON（带外配对）。失败抛 [RdCoreException]。
  void rememberPeer(String json) {
    final jp = json.toNativeUtf8();
    final r = lib.rememberPeer(_handle, jp);
    malloc.free(jp);
    if (r.address != 0) {
      final s = r.toDartString();
      lib.stringFree(r);
      throw RdCoreException('导入对端失败: $s');
    }
  }

  /// 在已配对的基础上开一个连接会话。返回 [NativeConnection]（复用本机句柄）。
  NativeConnection createSession({
    required String role,
    required Uint8List sessionId,
    String? unattendedPin,
  }) {
    return NativeConnection.fromIdentity(
      this,
      role: role,
      sessionId: sessionId,
      unattendedPin: unattendedPin,
    );
  }

  void dispose() => lib.identityFree(_handle);
}
