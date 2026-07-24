// 真实 Dart FFI 媒体 / 输入集成测试（Track A 端到端）：
// 加载真实的 rdcore_ffi.dll，跑完整握手 + E2E 密钥后，经 [NativeConnection] 的媒体 seam
// 完成「Host 抓取合成帧 → 编码 → AEAD 加密 → 传输 → Viewer 解密/解码/渲染」以及
// 「Viewer 发送输入 → Host 轮询收到」的往返，断言无损。
//
// 运行（dll 已通过 cargo build -p rdcore-ffi 产出，置于 cwd 或 RDCORE_FFI_PATH）：
//   RDCORE_FFI_PATH=$PWD/../target/debug/rdcore_ffi.dll \
//     flutter test test/native_media_integration_test.dart
//
// 若原生库不可用，组内测试 markTestSkipped，不会让套件变红。
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/local_identity.dart';
import 'package:remote_desktop/connection/native_connection.dart';
import 'package:remote_desktop/ffi/rdcore_bindings.dart';
import 'package:remote_desktop/models/consent_scope.dart';
import 'package:remote_desktop/models/media_frame.dart';
import 'package:remote_desktop/models/remote_input.dart';

void main() {
  final lib = RdCoreLib();

  group('真实 FFI 媒体 / 输入（Track A 端到端）', () {
    test('Host→Viewer 帧无损 + Viewer→Host 输入往返', () async {
      if (!lib.available) {
        // 原生库不可用（未放置 dll / 不在搜索路径 / 缺 MSVC 运行库）时优雅跳过，
        // 不使套件变红。注意：markTestSkipped 在 setUp 中本版本不阻断 body，故置于 body 起始。
        markTestSkipped(
            'rdcore_ffi 原生库不可用（设置 RDCORE_FFI_PATH 或放到 cwd），跳过媒体集成测试');
        return;
      }
      final hostId = LocalIdentity.create(displayName: 'host');
      final viewerId = LocalIdentity.create(displayName: 'viewer');
      hostId.rememberPeer(viewerId.peerJson);
      viewerId.rememberPeer(hostId.peerJson);
      final sid = Uint8List(16)..fillRange(0, 16, 0x42);

      final host = NativeConnection.fromIdentity(
          hostId, role: 'host', sessionId: sid);
      final viewer = NativeConnection.fromIdentity(
          viewerId, role: 'viewer', sessionId: sid);

      // 完整握手 + 同意 + E2E 密钥交换（Offer 由 Viewer 发起）。
      final offer = viewer.makeOffer();
      host.ingestOffer(offer);
      final answer = host.makeAnswer();
      viewer.ingestAnswer(answer);
      host.hostRequestConsent();
      host.hostDecide(grant: true, scopes: {ConsentScope.view, ConsentScope.input});
      // Rust ECDH 要求本端先生成 ephemeral key，才能 ingest 对端密钥：
      // 先双方各 makeSessionKeyExchange（生成本端密钥并缓存），再互相 ingest。
      final vEx = viewer.makeSessionKeyExchange();
      final hEx = host.makeSessionKeyExchange();
      host.ingestSessionKeyExchange(vEx);
      viewer.ingestSessionKeyExchange(hEx);

      // 媒体 seam：把两端接入同一对进程内通道。
      host.attachLoopbackMedia(viewer);
      const w = 64, h = 48, frames = 10, color = 0xAB;
      host.hostSetCapture(width: w, height: h, frames: frames, color: color);
      host.hostStartCapture(30);

      // Viewer 拉帧，断言无损（纯色 0xAB）。给后台抓取线程时间产出。
      var got = 0;
      RdMediaFrame? last;
      for (var i = 0; i < frames * 8 && got < frames; i++) {
        final f = viewer.pullFrame();
        if (f != null) {
          got++;
          last = f;
          expect(f.width, w);
          expect(f.height, h);
          expect(f.rgba.length, w * h * 4);
          expect(f.rgba.every((b) => b == color), isTrue,
              reason: '帧像素应全为纯色 $color');
        } else {
          await Future.delayed(const Duration(milliseconds: 10));
        }
      }
      expect(got, frames, reason: '应拉满 $frames 帧');
      expect(last, isNotNull);

      // Viewer 发送输入 → Host 轮询收到。
      viewer.sendInputEvent(RdInputEvent.mouseButton(0, true));
      RdInputEvent? recv;
      for (var i = 0; i < 50 && recv == null; i++) {
        recv = host.pollInput();
        if (recv == null) await Future.delayed(const Duration(milliseconds: 10));
      }
      expect(recv, isNotNull, reason: 'Host 应轮询到 Viewer 发来的输入');
      expect(recv!.kind, RdInputKind.mouseButton);
      expect(recv.button, 0); // Left
      expect(recv.pressed, isTrue);

      viewer.dispose();
      host.dispose();
    });
  });
}
