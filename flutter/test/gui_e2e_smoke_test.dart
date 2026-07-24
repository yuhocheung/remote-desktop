// 真机 GUI 端到端冒烟（headless widget 测试，可在 `flutter test` 下直接运行）。
//
// 驱动真实的 widget 树（DemoScreen → Host 面板 + Viewer 远程屏），走完整交互流：
//   点击「发起连接」→ Host 弹出不可伪造同意弹窗 → 点击「允许」
//   → 双侧不可伪造安全横幅显示「端到端加密：已建立」、Viewer 远程屏激活。
//
// 若 rdcore_ffi.dll 可用（RDCORE_FFI_PATH 或 cwd 下），[buildDemoSession] 会自动
// 走真实 FFI 路径（真实 Dart 绑定 + 真实原生库）；否则退回 Mock 后端，UI 流仍被验证。
// 这把「真实 Rust 核心 + 真实 Dart FFI 绑定 + 真实 Flutter UI 状态机」在端到端层面打通。
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:remote_desktop/connection/connection_controller.dart';
import 'package:remote_desktop/connection/demo_session.dart';
import 'package:remote_desktop/ffi/rdcore_bindings.dart';
import 'package:remote_desktop/ui/demo_screen.dart';
import 'package:remote_desktop/ui/remote_frame_view.dart';

void main() {
  testWidgets('完整 GUI 端到端：连接 → 同意 → 不可伪造加密横幅（真实 FFI 优先）',
      (tester) async {
    final useNative = RdCoreLib().available;
    final session = buildDemoSession(deviceName: 'smoke', useNative: useNative);

    // 给测试更大的画面，避免同意弹窗在小视口下溢出导致「允许」按钮点不到。
    await tester.binding.setSurfaceSize(const Size(1024, 900));
    await tester.pump();
    await tester.pumpWidget(MaterialApp(home: DemoScreen(session: session)));
    await tester.pumpAndSettle();

    // 1) Viewer 发起连接
    await tester.tap(find.widgetWithText(FilledButton, '发起连接（Viewer 侧）'));
    // 信令为异步（microtask）级联，用固定多帧 pump 确保整条链路 flush 完成。
    for (var i = 0; i < 10; i++) {
      await tester.pump(const Duration(milliseconds: 30));
    }

    // 2) Host 应进入等待同意，弹出不可伪造同意弹窗（含已认证对端指纹）
    expect(find.text('有连接请求'), findsOneWidget,
        reason: 'Host 应收到连接请求并弹出同意弹窗');
    expect(find.textContaining('指纹：'), findsWidgets,
        reason: '同意弹窗应展示来自已认证对端的不可伪造指纹');

    // 3) Host 点击「允许」（默认授予 view + input）
    final allowBtn = find.widgetWithText(FilledButton, '允许');
    await tester.ensureVisible(allowBtn);
    await tester.pump(const Duration(milliseconds: 30));
    await tester.tap(allowBtn, warnIfMissed: false);
    for (var i = 0; i < 10; i++) {
      await tester.pump(const Duration(milliseconds: 30));
    }

    // 4) 双侧不可伪造安全横幅应显示「端到端加密：已建立」
    expect(find.text('端到端加密：已建立'), findsWidgets,
        reason: 'Host 与 Viewer 的不可伪造安全横幅都应显示加密已建立');
    // 诊断：Host 同意后状态（暴露真实 FFI 路径下的原生错误）
    expect(session.host.error, isNull,
        reason: 'Host 同意后不应报原生错误: ${session.host.error}');
    expect(session.host.phase, ConnectionPhase.active,
        reason: 'Host 应在同意后进入 active（实际 phase=${session.host.phase}）');
    // Host 侧应显示已允许连接（含授予范围标签）
    expect(find.textContaining('已允许连接'), findsOneWidget);
    // Viewer 侧应进入激活态，渲染真实远程帧视图（替代旧占位文案）
    expect(find.byType(RemoteFrameView), findsOneWidget,
        reason: 'Viewer 激活后应渲染真实 RemoteFrameView 画面视图');
    expect(find.textContaining('键盘自动捕获'), findsOneWidget,
        reason: 'Viewer 激活后应展示输入捕获提示');

    session.dispose();
  });
}
