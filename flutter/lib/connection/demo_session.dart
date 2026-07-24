import 'dart:math';
import 'dart:typed_data';

import 'connection_controller.dart';
import 'local_identity.dart';
import 'mock_connection.dart';
import 'signaling.dart';
import '../ffi/rdcore_bindings.dart';

/// 一对互连的演示控制器（Host + Viewer），通过内存回环信令相连。
///
/// `start()` 由 Viewer 侧发起握手；`dispose()` 释放两端资源。
class DemoSession {
  DemoSession({required this.host, required this.viewer});

  final ConnectionController host;
  final ConnectionController viewer;

  /// 由 Viewer 侧发起握手（发出 Offer）。
  void start() => viewer.startAsViewer();

  void dispose() {
    host.dispose();
    viewer.dispose();
  }
}

/// 构建演示会话。
///
/// [useNative] 为 true 且 [RdCoreLib] 可用时走真实 FFI 路径：两端各建本地身份、
/// 互相带外配对（互相导入对端身份 JSON）、共享 16 字节会话 ID；否则退回纯 Dart 的
/// [MockConnection]（无需原生库，可在 `flutter test` 中运行）。
DemoSession buildDemoSession({
  required String deviceName,
  required bool useNative,
}) {
  final (hostSig, viewerSig) = InMemorySignaling.pair();

  if (useNative && RdCoreLib().available) {
    final hostId = LocalIdentity.create(displayName: '$deviceName (Host)');
    final viewerId = LocalIdentity.create(displayName: '$deviceName (Viewer)');
    // 带外配对：互相导入对端身份 JSON（真实场景由扫码 / 当面核对完成）。
    hostId.rememberPeer(viewerId.peerJson);
    viewerId.rememberPeer(hostId.peerJson);
    final sid = _randomSessionId();
    final hostBackend =
        hostId.createSession(role: 'host', sessionId: sid);
    final viewerBackend =
        viewerId.createSession(role: 'viewer', sessionId: sid);
    return DemoSession(
      host: ConnectionController(
          backend: hostBackend, transport: hostSig, isHost: true),
      viewer: ConnectionController(
          backend: viewerBackend, transport: viewerSig, isHost: false),
    );
  }

  return DemoSession(
    host: ConnectionController(
        backend: MockConnection(isHost: true), transport: hostSig, isHost: true),
    viewer: ConnectionController(
        backend: MockConnection(isHost: false),
        transport: viewerSig,
        isHost: false),
  );
}

Uint8List _randomSessionId() {
  final r = Random.secure();
  return Uint8List.fromList(List<int>.generate(16, (_) => r.nextInt(256)));
}
