import 'dart:async';

import 'package:flutter/material.dart';

import 'app/connection_manager.dart';
import 'ui/home_screen.dart';

void main() {
  // 把未捕获的同步/异步异常统一兜住，避免直接白屏无信息。
  runZonedGuarded<void>(() {
    // 任何插件/平台通道初始化前先确保 binding 就绪。
    WidgetsFlutterBinding.ensureInitialized();
    // 构建期抛错也渲染成可读的错误界面，而不是白屏。
    ErrorWidget.builder = (details) {
      return Scaffold(
        body: SingleChildScrollView(
          padding: const EdgeInsets.all(16),
          child: Text(
            '运行时错误:\n${details.exception}\n\n${details.stack}',
            style: const TextStyle(color: Colors.red),
          ),
        ),
      );
    };
    runApp(RemoteDesktopApp());
  }, (error, stack) {
    debugPrint('🔥 未捕获错误: $error');
    debugPrint('$stack');
  });
}

class RemoteDesktopApp extends StatelessWidget {
  RemoteDesktopApp({super.key});

  /// App 级连接管理器（单例），贯穿首页 / 配对 / 设置 / 远程屏，供多会话复用。
  final ConnectionManager _manager = ConnectionManager();

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: '远程桌面',
      theme: ThemeData(
        useMaterial3: true,
        colorSchemeSeed: Colors.blue,
        // App 全局 UI 字体：Inter 负责拉丁/数字/配对码，CJK 回退到更美观的系统无衬线体。
        fontFamily: 'Inter',
        fontFamilyFallback: const [
          'Microsoft YaHei',
          'PingFang SC',
          'Noto Sans CJK SC',
          'sans-serif',
        ],
      ),
      home: HomeScreen(manager: _manager),
    );
  }
}
