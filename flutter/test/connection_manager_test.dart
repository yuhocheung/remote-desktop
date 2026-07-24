import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';

import 'package:remote_desktop/app/connection_manager.dart';
import 'package:remote_desktop/connection/connection_controller.dart';
import 'package:remote_desktop/connection/mock_connection.dart';
import 'package:remote_desktop/connection/signaling.dart';
import 'package:remote_desktop/connection/websocket_signaling.dart';
import 'package:remote_desktop/models/app_settings.dart';
import 'package:remote_desktop/models/consent_scope.dart';

/// 验证 H 生产 UI 的 app 主壳（[ConnectionManager] / [AppSettings] /
/// [WebSocketSignaling]）。
/// 不依赖原生库与信令服务器，可在 `flutter test` 中直接运行。
void main() {
  group('AppSettings', () {
    test('默认值', () {
      const s = AppSettings();
      expect(s.deviceName, '我的设备');
      expect(s.signalingBaseUrl, 'ws://8.138.237.243:8080');
      expect(s.defaultScopes, {ConsentScope.view, ConsentScope.input});
      expect(s.stunUrl, 'stun:8.138.237.243:3478');
      expect(s.turnUrl, 'turn:8.138.237.243:3478?transport=udp');
      expect(s.turnUser, isNotNull);
      expect(s.turnPass, isNotNull);
    });

    test('copyWith 各字段独立替换', () {
      const s = AppSettings();
      expect(
        s.copyWith(deviceName: 'B').deviceName,
        'B',
      );
      expect(s.copyWith(deviceName: 'B').signalingBaseUrl, s.signalingBaseUrl);
      expect(s.copyWith(deviceName: 'B').defaultScopes, s.defaultScopes);

      expect(
        s.copyWith(signalingBaseUrl: 'wss://h:9').signalingBaseUrl,
        'wss://h:9',
      );
      final scopes = {ConsentScope.view, ConsentScope.fileTransfer};
      expect(s.copyWith(defaultScopes: scopes).defaultScopes, scopes);
    });
  });

  group('ConnectionManager（内存场景，无网络）', () {
    test('addSession / sessionById / removeSession', () {
      final m = ConnectionManager();
      final (a, b) = InMemorySignaling.pair();
      final c = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: b,
        isHost: false,
      );
      final id = m.addSession(c, peerName: 'peer');
      expect(m.sessions.length, 1);
      expect(m.sessionById(id), isNotNull);
      expect(m.sessionById(id)!.peerName, 'peer');

      m.removeSession(id);
      expect(m.sessions.length, 0);
      expect(m.sessionById(id), isNull);

      // 重复删除 / 未知 id 应安全 no-op。
      expect(() => m.removeSession(id), returnsNormally);
      expect(() => m.removeSession('nope'), returnsNormally);
      a.close();
    });

    test('removeSession 释放控制器（dispose 幂等）', () {
      final m = ConnectionManager();
      final (a, b) = InMemorySignaling.pair();
      final c = ConnectionController(
        backend: MockConnection(isHost: false),
        transport: b,
        isHost: false,
      );
      final id = m.addSession(c);
      m.removeSession(id);
      // 控制器已被 dispose；再次 dispose 应安全（ConnectionController 幂等保护）。
      expect(() => c.dispose(), returnsNormally);
      a.close();
    });
  });

  group('WebSocketSignaling', () {
    test('实现 SignalingTransport，send/close 不抛未处理异常', () async {
      final t = WebSocketSignaling('ws://127.0.0.1:0/deadbeef?token=00');
      expect(t.incoming, isA<Stream<Uint8List>>());
      // 连接尚未建立时 send 应缓冲而非抛。
      expect(() => t.send(Uint8List.fromList([1, 2, 3])), returnsNormally);
      await t.close();
      // close 后再 send 应安全 no-op。
      expect(() => t.send(Uint8List.fromList([4])), returnsNormally);
    });
  });
}
