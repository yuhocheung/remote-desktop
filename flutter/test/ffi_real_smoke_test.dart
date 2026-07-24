// 真实 Dart FFI 冒烟测试：用真实的 [RdCoreLib]（即真实的 Dart FFI 绑定）
// 加载真实的 rdcore_ffi.dll，跑完整协议握手。这把「Flutter NativeConnection 路径
// 是否真的能接通 Rust 核心」在 Dart 侧坐实——之前只在 Python ctypes 层面验证过。
//
// 运行（在 flutter/ 目录，dll 已通过 tool/build_ffi.sh 放到本目录）：
//   RDCORE_FFI_PATH=$PWD/rdcore_ffi.dll flutter test test/ffi_real_smoke_test.dart
//
// 若原生库不可用（未放置 .dll），组内测试会被 markTestSkipped，不会让套件变红。
import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:typed_data';

import 'package:ffi/ffi.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/ffi/rdcore_bindings.dart';

// ignore_for_file: avoid_positional_boolean_parameters
// ignore_for_file: prefer_single_quote

void main() {
  final lib = RdCoreLib();

  // ───────── 安全封装 helpers ─────────
  String? readStr(ffi.Pointer<Utf8> p) {
    if (p.address == 0) return null;
    final s = p.toDartString();
    lib.stringFree(p);
    return s;
  }

  Uint8List? readBytes(ffi.Pointer<RdBytes> p) {
    if (p.address == 0) return null;
    final rb = p.ref;
    final copy = Uint8List.fromList(rb.data.asTypedList(rb.len));
    lib.bytesFree(p);
    return copy;
  }

  ffi.Pointer<Utf8> cstr(String s) => s.toNativeUtf8();

  ffi.Pointer<Utf8> sendBytes(
    ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int) fn,
    ffi.Pointer<ffi.Void> sess,
    Uint8List payload,
  ) {
    final buf = malloc<ffi.Uint8>(payload.length);
    buf.asTypedList(payload.length).setAll(0, payload);
    final err = fn(sess, buf, payload.length);
    malloc.free(buf);
    return err;
  }

  Uint8List makeBytes(
    ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>) fn,
    ffi.Pointer<ffi.Void> sess,
  ) {
    final p = fn(sess);
    final b = readBytes(p);
    if (b == null) throw StateError('${fn.runtimeType} 返回 NULL');
    return b;
  }

  Uint8List cryptBytes(
    ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int) fn,
    ffi.Pointer<ffi.Void> sess,
    Uint8List plain,
  ) {
    final buf = malloc<ffi.Uint8>(plain.length);
    buf.asTypedList(plain.length).setAll(0, plain);
    final p = fn(sess, buf, plain.length);
    malloc.free(buf);
    return readBytes(p)!;
  }

  group('真实 Dart FFI 冒烟（加载 rdcore_ffi.dll 跑完整协议）', () {
    setUp(() {
      if (!lib.available) {
        markTestSkipped(
            'rdcore_ffi 原生库不可用（未放置 dll / 不在搜索路径），跳过真实 FFI 冒烟');
      }
    });

    test('完整协议：身份→配对→验签→同意→E2E 密钥→AEAD→篡改拒收→撤销', () {
      // 版本读取
      final ver = readStr(lib.version());
      expect(ver, isNotNull);
      expect(ver!.length, greaterThan(0), reason: '库加载成功且 rdcore_version 可读');

      // 1) 两个设备的长期身份
      final hostName = cstr('host-laptop');
      final viewerName = cstr('viewer-phone');
      final hostLocal = lib.identityNew(hostName);
      final viewerLocal = lib.identityNew(viewerName);
      malloc.free(hostName);
      malloc.free(viewerName);
      expect(hostLocal.address, isNot(0));
      expect(viewerLocal.address, isNot(0));

      final hostFp = readStr(lib.localFingerprint(hostLocal))!;
      final viewerFp = readStr(lib.localFingerprint(viewerLocal))!;
      expect(hostFp, isNot(viewerFp), reason: '双方指纹应不同');

      // 2) 带外配对：互相导入对方身份 JSON
      final hostJson = readStr(lib.localPeerJson(hostLocal))!;
      final viewerJson = readStr(lib.localPeerJson(viewerLocal))!;
      expect(readStr(lib.rememberPeer(viewerLocal, cstr(hostJson))), isNull,
          reason: 'viewer 导入 host 身份');
      expect(readStr(lib.rememberPeer(hostLocal, cstr(viewerJson))), isNull,
          reason: 'host 导入 viewer 身份');

      // 3) 开会话
      final sid = Uint8List(16)..fillRange(0, 16, 1);
      final sidPtr = malloc<ffi.Uint8>(16);
      sidPtr.asTypedList(16).setAll(0, sid);
      final nullUtf8 = ffi.nullptr.cast<Utf8>();
      final host = lib.sessionNew(hostLocal, 1, sidPtr, nullUtf8);
      final viewer = lib.sessionNew(viewerLocal, 0, sidPtr, nullUtf8);
      malloc.free(sidPtr);
      expect(host.address, isNot(0));
      expect(viewer.address, isNot(0));

      // 4) Viewer 发 Offer → Host 验签收下
      final offer = makeBytes(lib.makeOffer, viewer);
      expect(readStr(sendBytes(lib.ingestOffer, host, offer)), isNull,
          reason: 'host 验签 viewer Offer');

      // 5) Host 回 Answer → Viewer 验签收下
      final answer = makeBytes(lib.makeAnswer, host);
      expect(readStr(sendBytes(lib.ingestAnswer, viewer, answer)), isNull,
          reason: 'viewer 验签 host Answer');

      // 6) Host 同意（View + Input）
      final reqState = readStr(lib.hostRequestConsent(host, nullUtf8))!;
      expect(reqState, contains('AwaitingConsent'), reason: 'Host 进入同意流程');
      final decState = readStr(lib.hostDecide(host, 1, (1 | 2), 0))!;
      expect(decState, contains('Active'), reason: 'Host 批准 → 激活');

      // 7) 端到端 X25519 密钥交换（双向）
      final vEx = makeBytes(lib.makeSessionKey, viewer);
      final hEx = makeBytes(lib.makeSessionKey, host);
      expect(readStr(sendBytes(lib.ingestSessionKey, host, vEx)), isNull,
          reason: 'host 接受 viewer 密钥交换');
      expect(readStr(sendBytes(lib.ingestSessionKey, viewer, hEx)), isNull,
          reason: 'viewer 接受 host 密钥交换');

      // 8) E2E AEAD 加解密往返
      final plain = Uint8List.fromList([...utf8.encode('hello remote desktop'), 0, 1, 2]);
      final ct = cryptBytes(lib.encrypt, viewer, plain);
      final dec = cryptBytes(lib.decrypt, host, ct);
      expect(dec, equals(plain), reason: 'E2E AEAD 加解密往返一致');

      // 篡改密文应被拒绝
      final ctBad = Uint8List.fromList(ct)..[0] ^= 0xFF;
      final badBuf = malloc<ffi.Uint8>(ctBad.length);
      badBuf.asTypedList(ctBad.length).setAll(0, ctBad);
      final badPtr = lib.decrypt(host, badBuf, ctBad.length);
      malloc.free(badBuf);
      expect(readBytes(badPtr), isNull, reason: '篡改密文被拒绝（decrypt 返回 NULL）');

      // 9) 不可伪造安全指示器
      final ind = readStr(lib.securityIndicator(host, 1))!;
      expect(ind, contains('encrypted'), reason: '安全指示器含加密标记');

      // 10) 已认证对端信息
      expect(readStr(lib.peerDisplayName(host)), 'viewer-phone');
      expect(readStr(lib.peerFingerprint(host)), viewerFp);

      // 11) 撤销
      final rev = readStr(lib.revoke(host))!;
      expect(rev, contains('Closed'), reason: 'Host 撤销 → 关闭');

      // 清理
      lib.sessionFree(host);
      lib.sessionFree(viewer);
      lib.identityFree(hostLocal);
      lib.identityFree(viewerLocal);
    });

    test('未配对对端的 Offer 必须被拒（MITM 防护）', () {
      final a = lib.identityNew(cstr('A'));
      final b = lib.identityNew(cstr('B'));
      final sid = Uint8List(16)..fillRange(0, 16, 7);
      final sidPtr = malloc<ffi.Uint8>(16);
      sidPtr.asTypedList(16).setAll(0, sid);
      final nullUtf8 = ffi.nullptr.cast<Utf8>();
      final sa = lib.sessionNew(a, 1, sidPtr, nullUtf8);
      final sb = lib.sessionNew(b, 0, sidPtr, nullUtf8);
      malloc.free(sidPtr);

      final offerB = makeBytes(lib.makeOffer, sb);
      final err = sendBytes(lib.ingestOffer, sa, offerB);
      expect(readStr(err), isNotNull, reason: '未配对对端的 Offer 被拒绝');

      lib.sessionFree(sa);
      lib.sessionFree(sb);
      lib.identityFree(a);
      lib.identityFree(b);
    });
  });
}
