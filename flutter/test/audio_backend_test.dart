import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/mock_connection.dart';

/// 音频（Track A）后端契约测试：用 [MockConnection] 验证 [ConnectionBackend]
/// 新增的 [pullAudio] / [attachLoopbackAudio] / [hostSetCaptureAudio] /
/// [hostStartCaptureAudio]。
void main() {
  group('MockConnection 音频（Track A 后端契约）', () {
    test('pullAudio 返回合法 Raw PCM 帧', () {
      final c = MockConnection(isHost: false);
      final a = c.pullAudio();
      expect(a, isNotNull);
      expect(a!.isRaw, isTrue);
      expect(a.channels, 2);
      expect(a.sampleRate, 48000);
      expect(a.isValid, isTrue);
    });

    test('attachLoopbackAudio 为 no-op 不抛异常', () {
      final host = MockConnection(isHost: true);
      final viewer = MockConnection(isHost: false);
      expect(() => host.attachLoopbackAudio(viewer), returnsNormally);
    });

    test('hostSetCaptureAudio / hostStartCaptureAudio 不抛异常', () {
      final host = MockConnection(isHost: true);
      expect(
          () => host.hostSetCaptureAudio(
                channels: 2,
                sampleRate: 48000,
                samplesPerFrame: 960,
                frames: 10,
                byte: 0,
              ),
          returnsNormally);
      expect(() => host.hostStartCaptureAudio(30), returnsNormally);
    });
  });
}
