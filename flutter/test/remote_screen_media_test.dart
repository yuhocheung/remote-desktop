import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/connection_controller.dart';
import 'package:remote_desktop/connection/mock_connection.dart';
import 'package:remote_desktop/connection/signaling.dart';
import 'package:remote_desktop/models/consent_scope.dart';
import 'package:remote_desktop/models/remote_input.dart';
import 'package:remote_desktop/ui/remote_frame_view.dart';
import 'package:remote_desktop/ui/remote_screen.dart';

/// Viewer 远程屏 widget 测试（Track A）：激活后渲染真实帧视图，并在画面区捕获指针输入。
///
/// 走 [MockConnection] + [InMemorySignaling]，不依赖原生库，可在 `flutter test` 直接运行。
void main() {
  testWidgets('Viewer 激活后渲染 RemoteFrameView 并捕获指针按下/抬起', (tester) async {
    await tester.binding.setSurfaceSize(const Size(1024, 900));

    ConnectionController? viewer;
    ConnectionController? host;
    MockConnection? viewerBackend;
    MockConnection? hostBackend;

    // 整个握手 + 渲染 + 输入捕获放进 runAsync：testWidgets 默认 FakeAsync 会把它创建的
    // StreamController 的投递 microtask 捕获到 FakeAsync 队列，而 runAsync 只排空真实队列，
    // 导致 host→viewer 的消息（answer / sessionKey）永远丢失、viewer 卡在 connecting。
    // 在 runAsync 内创建信令与控制器，整条级联都在真实 async 区，[Future] 才能正常排空。
    await tester.runAsync(() async {
      final (hostSig, viewerSig) = InMemorySignaling.pair();
      hostBackend = MockConnection(isHost: true);
      viewerBackend = MockConnection(isHost: false);
      host = ConnectionController(
          backend: hostBackend!, transport: hostSig, isHost: true);
      viewer = ConnectionController(
          backend: viewerBackend!, transport: viewerSig, isHost: false);

      await tester.pumpWidget(MaterialApp(home: RemoteScreen(controller: viewer!)));

      // 起始（未激活）：不应渲染帧视图。
      expect(find.byType(RemoteFrameView), findsNothing);

      viewer!.startAsViewer();
      for (var i = 0; i < 20 && !viewer!.isActive; i++) {
        await Future(() {});
      }
      host!.approve({ConsentScope.view, ConsentScope.input});
      for (var i = 0; i < 20 && !viewer!.isActive; i++) {
        await Future(() {});
      }
      expect(viewer!.isActive, isTrue);

      // 让 50ms 帧轮询定时器拉到至少一帧（真实定时器，需真实时间推进）。
      await Future.delayed(const Duration(milliseconds: 200));
      await tester.pump();

      expect(find.byType(RemoteFrameView), findsOneWidget,
          reason: 'Viewer 激活后应渲染真实帧视图');

      // 触摸 tap 流程：业界标准（参考 RustDesk InputModel）触摸 down 不立即发送，
      // 需 up 时判断是 tap / 长按 / 拖动。tap 完成后发送 left down + left up。
      final center = tester.getCenter(find.byType(RemoteFrameView));
      final g = await tester.startGesture(center);
      await tester.pump();
      // down 不立即发（待 up 判断 tap/长按/拖动）。

      await g.up();
      await tester.pump();
      // tap 完成：发送 left down + left up，lastSentInput 最终为 pressed=false。
      expect(viewerBackend!.lastSentInput, isNotNull);
      expect(viewerBackend!.lastSentInput!.kind, RdInputKind.mouseButton);
      expect(viewerBackend!.lastSentInput!.button, 0); // Left
      expect(viewerBackend!.lastSentInput!.pressed, isFalse,
          reason: 'tap 抬起最终发送 pressed=false');
    });

    // teardown：先卸载 RemoteScreen（移除对 controller 的监听），再释放控制器。
    // 否则 ChangeNotifier.dispose() 在仍有监听器时会抛错，导致 host.dispose() 被跳过、
    // Host 的 _inputTimer 周期定时器残留，进而 teardown 的 pumpAndSettle 永远等不到空闲而挂起。
    // dispose 幂等，重复调用安全。
    addTearDown(() async {
      await tester.pumpWidget(const SizedBox.shrink());
      viewer?.dispose();
      host?.dispose();
    });
  });
}
