// Track B（kimi-k3）配对逻辑测试（B3）。
import 'dart:typed_data';

import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/pairing.dart';

void main() {
  group('PairingInvite', () {
    final sid = Uint8List.fromList(List<int>.generate(16, (i) => i * 17 % 256));
    final token =
        List<String>.generate(64, (i) => '0123456789abcdef'[i % 16]).join();

    test('code 往返解析', () {
      final invite = PairingInvite(sessionId: sid, token: token);
      final parsed = PairingInvite.parse(invite.code);
      expect(parsed, isNotNull);
      expect(parsed!.sessionId, sid);
      expect(parsed.token, token);
    });

    test('sessionHex 为 32 字符小写 hex', () {
      final invite = PairingInvite(sessionId: sid, token: token);
      expect(invite.sessionHex.length, 32);
      expect(invite.sessionHex, matches(RegExp(r'^[0-9a-f]{32}$')));
    });

    test('signalingUrl 形态正确（路径首段即 session hex）', () {
      final invite = PairingInvite(sessionId: sid, token: token);
      final url = invite.signalingUrl('signal.example.com');
      // 服务端从路径第一段取 session_id，URL 必须是 wss://host/<32hex>?token=<64hex>，
      // 不得含 signaling/ 前缀段（否则被误当 session_id 拒绝）。
      expect(url, 'wss://signal.example.com/${invite.sessionHex}?token=$token');
    });

    test('signalingUrlFromBase 保留 baseUrl 协议（ws/wss）', () {
      final invite = PairingInvite(sessionId: sid, token: token);
      // 本地开发基址 ws:// → 保留 ws://。
      expect(
        invite.signalingUrlFromBase('ws://127.0.0.1:8080'),
        'ws://127.0.0.1:8080/${invite.sessionHex}?token=$token',
      );
      // 生产基址 wss:// → 保留 wss://。
      expect(
        invite.signalingUrlFromBase('wss://signal.example.com'),
        'wss://signal.example.com/${invite.sessionHex}?token=$token',
      );
    });

    test('拒绝非法配对码', () {
      expect(PairingInvite.parse(''), isNull);
      expect(PairingInvite.parse('nocolon'), isNull);
      expect(PairingInvite.parse('zz:abc'), isNull); // 非 hex session
      expect(PairingInvite.parse('${'0' * 32}:short'), isNull); // token 太短
      expect(PairingInvite.parse('${'0' * 32}:${'G' * 64}'), isNull); // token 非小写 hex
    });

    test('拒绝错误长度 session', () {
      final bad = '${'0' * 30}:$token'; // session 30 字符而非 32
      expect(PairingInvite.parse(bad), isNull);
    });
  });
}
