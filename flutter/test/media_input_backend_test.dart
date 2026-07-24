import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/mock_connection.dart';
import 'package:remote_desktop/models/remote_input.dart';

/// 媒体 / 输入（Track A）后端契约测试：用 [MockConnection] 验证
/// [ConnectionBackend] 新增的 [pullFrame] / [sendInputEvent] / [pollInput]。
void main() {
  group('MockConnection 媒体 / 输入（Track A 后端契约）', () {
    test('pullFrame 返回合法 RGBA 帧（2x2）', () {
      final c = MockConnection(isHost: false);
      final f = c.pullFrame();
      expect(f, isNotNull);
      expect(f!.width, 2);
      expect(f.height, 2);
      expect(f.rgba.length, 16); // 2*2*4
      expect(f.isValid, isTrue);
    });

    test('sendInputEvent 记录最近一次输入', () {
      final c = MockConnection(isHost: false);
      final ev = RdInputEvent.mouseButton(0, true);
      c.sendInputEvent(ev);
      expect(c.lastSentInput, equals(ev));
      expect(c.lastSentInput!.kind, RdInputKind.mouseButton);
      expect(c.lastSentInput!.pressed, isTrue);
    });

    test('pollInput 在 mock 下返回 null（无 host/viewer 配对）', () {
      final c = MockConnection(isHost: true);
      expect(c.pollInput(), isNull);
    });

    test('RdInputEvent 工厂与 Native 往返一致', () {
      final ev = RdInputEvent.mouseMove(10, 20);
      expect(ev.kind, RdInputKind.mouseMove);
      expect(ev.x, 10);
      expect(ev.y, 20);
      expect(RdInputEvent.mouseWheel(1, -2).deltaY, -2);
      final k = RdInputEvent.key(0x1e, true, modifiers: 2);
      expect(k.pressed, isTrue);
      expect(k.modifiers, 2);
    });
  });
}
