// Track B（kimi-k3）剪贴板 Dart 封装（B6）。
//
// 薄封装 `rdcore_bindings.dart` 的 `rdcore_clipboard_*` 原生函数：在 E2E 会话密钥下
// 把 ClipboardEvent 封成 Message::Encrypted 字节收发。剪贴板默认 opt-in（consent scope）。
import 'dart:ffi' as ffi;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import '../ffi/rdcore_bindings.dart';

/// 剪贴板动作（与原生 `action` 参数一致）。
enum ClipboardOp { request, data, clear }

/// 一条解码后的剪贴板事件。
class ClipboardMessage {
  const ClipboardMessage(this.op, this.data);
  final ClipboardOp op;
  final Uint8List? data;
}

class ClipboardClient {
  ClipboardClient(this._lib, this._session);

  final RdCoreLib _lib;
  final ffi.Pointer<ffi.Void> _session;

  ffi.Pointer<ffi.Uint8> _alloc(Uint8List bytes) {
    final ptr = malloc<ffi.Uint8>(bytes.length);
    ptr.asTypedList(bytes.length).setAll(0, bytes);
    return ptr;
  }

  /// 发送一条剪贴板事件，返回加密字节。
  Uint8List send(int seq, ClipboardOp op, [Uint8List? data]) {
    final action = switch (op) {
      ClipboardOp.request => 0,
      ClipboardOp.data => 1,
      ClipboardOp.clear => 2,
    };
    final payload = data ?? Uint8List(0);
    final ptr = payload.isEmpty ? ffi.nullptr.cast<ffi.Uint8>() : _alloc(payload);
    try {
      final p = _lib.clipboardSend(_session, seq, action, ptr, payload.length);
      if (p.address == 0) {
        throw StateError(_lib.takeLastError() ?? 'clipboard send failed');
      }
      final out = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
      _lib.bytesFree(p);
      return out;
    } finally {
      if (payload.isNotEmpty) malloc.free(ptr);
    }
  }

  /// 接收并解码一条剪贴板加密字节。
  ClipboardMessage recv(Uint8List encrypted) {
    final ptr = _alloc(encrypted);
    try {
      final p = _lib.clipboardRecv(_session, ptr, encrypted.length);
      if (p.address == 0) {
        throw StateError(_lib.takeLastError() ?? 'clipboard recv failed');
      }
      final b = p.ref.data.asTypedList(p.ref.len);
      final op = switch (b[0]) {
        0 => ClipboardOp.request,
        1 => ClipboardOp.data,
        _ => ClipboardOp.clear,
      };
      final data = op == ClipboardOp.data ? Uint8List.fromList(b.sublist(1)) : null;
      _lib.bytesFree(p);
      return ClipboardMessage(op, data);
    } finally {
      malloc.free(ptr);
    }
  }
}
