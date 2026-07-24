// Track B（kimi-k3）文件传输 Dart 封装（B6）。
//
// 薄封装 `rdcore_bindings.dart` 的 `rdcore_file_*` 原生函数：在已建立的 E2E 会话密钥下
// 把 FileTransferEvent 封成 Message::Encrypted 字节，经控制通道收发（云端只见密文）。
// 安全：Host 逐次同意（acceptFileTransfer）之前收到的 Chunk 一律被原生侧拒收。
import 'dart:ffi' as ffi;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import '../ffi/rdcore_bindings.dart';

/// 一次文件传输的句柄侧（Viewer 发送 / Host 接收）。
class FileTransferClient {
  FileTransferClient(this._lib, this._session);

  final RdCoreLib _lib;
  final ffi.Pointer<ffi.Void> _session;

  Uint8List _takeBytes(ffi.Pointer<RdBytes> p) {
    if (p.address == 0) {
      throw StateError(_lib.takeLastError() ?? 'native returned null bytes');
    }
    final out = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
    _lib.bytesFree(p);
    return out;
  }

  ffi.Pointer<ffi.Uint8> _alloc(Uint8List bytes) {
    final ptr = malloc<ffi.Uint8>(bytes.length);
    ptr.asTypedList(bytes.length).setAll(0, bytes);
    return ptr;
  }

  // ── Viewer（发送方）──

  /// 提议一次传输，返回加密字节（经控制通道发给 Host）。
  Uint8List sendOffer(int transferId, String name, int size) {
    final namePtr = name.toNativeUtf8();
    try {
      final p = _lib.fileSendOffer(_session, transferId, namePtr, size);
      return _takeBytes(p);
    } finally {
      malloc.free(namePtr);
    }
  }

  /// 在 Host Accept 后发送一个分片。
  Uint8List sendChunk(int transferId, int seq, Uint8List data) {
    final ptr = _alloc(data);
    try {
      final p = _lib.fileSendChunk(_session, transferId, seq, ptr, data.length);
      return _takeBytes(p);
    } finally {
      malloc.free(ptr);
    }
  }

  /// 发送收尾事件（含总分片数）。
  Uint8List sendDone(int transferId, int chunks) {
    final p = _lib.fileSendDone(_session, transferId, chunks);
    return _takeBytes(p);
  }

  /// 处理 Host 的决定字节。返回 true=Accept（可开始发 Chunk）/ false=Reject。
  bool onDecision(Uint8List encrypted) {
    final ptr = _alloc(encrypted);
    try {
      final p = _lib.fileViewerOnDecision(_session, ptr, encrypted.length);
      if (p.address == 0) {
        throw StateError(_lib.takeLastError() ?? 'invalid decision');
      }
      final b = p.ref.data.asTypedList(p.ref.len);
      final accepted = b.isNotEmpty && b[0] == 1;
      _lib.bytesFree(p);
      return accepted;
    } finally {
      malloc.free(ptr);
    }
  }

  // ── Host（接收方）──

  /// 收到 Offer 字节，建立接收会话，返回 transfer_id（null 表示非 Offer/校验失败）。
  int? hostOnOffer(Uint8List encrypted) {
    final ptr = _alloc(encrypted);
    try {
      final p = _lib.fileHostOnOffer(_session, ptr, encrypted.length);
      if (p.address == 0) return null;
      final b = p.ref.data.asTypedList(p.ref.len);
      if (b.length < 8) {
        _lib.bytesFree(p);
        return null;
      }
      final id = ByteData.sublistView(Uint8List.fromList(b)).getUint64(0, Endian.little);
      _lib.bytesFree(p);
      return id;
    } finally {
      malloc.free(ptr);
    }
  }

  /// Host 对 Offer 做决定，返回回传 Viewer 的加密字节。
  Uint8List hostDecide(int transferId, bool accept, {String? reason}) {
    final reasonPtr = (reason ?? '').toNativeUtf8();
    try {
      final p = _lib.fileHostDecide(_session, transferId, accept ? 1 : 0, reasonPtr);
      return _takeBytes(p);
    } finally {
      malloc.free(reasonPtr);
    }
  }

  /// 收到一个分片/收尾字节。完成时返回完整文件字节，否则返回 null。
  /// 出错会抛 StateError（含原生错误）。
  Uint8List? hostOnEvent(Uint8List encrypted) {
    final ptr = _alloc(encrypted);
    try {
      final p = _lib.fileHostOnEvent(_session, ptr, encrypted.length);
      if (p.address == 0) {
        final err = _lib.takeLastError();
        if (err != null) throw StateError(err);
        return null; // 中间分片，未完成
      }
      final out = Uint8List.fromList(p.ref.data.asTypedList(p.ref.len));
      _lib.bytesFree(p);
      return out;
    } finally {
      malloc.free(ptr);
    }
  }
}
