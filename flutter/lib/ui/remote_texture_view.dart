import 'package:flutter/widgets.dart';

/// 真零拷贝远程画面视图：直接用 Flutter `Texture` 控件合成由原生端经
/// Flutter `TextureRegistry` 创建的 GPU 纹理。
///
/// 与 [RemoteFrameView]（把 RGBA 字节经 `dart:ui` 解码进 `ui.Image` 再 `CustomPaint`）不同，
/// 本视图**不持有任何像素字节**——像素由 Rust 直接写入原生纹理缓冲（同进程内存 / GPU 纹理），
/// Dart 仅用 `textureId` 让引擎重新合成。这是终极零拷贝路径，避免每次帧的多次 Dart 拷贝。
class RemoteTextureView extends StatefulWidget {
  const RemoteTextureView({super.key, required this.textureId});

  /// Flutter `TextureRegistry` 分配的纹理 id（来自原生端真零拷贝纹理桥）。
  final int textureId;

  @override
  State<RemoteTextureView> createState() => _RemoteTextureViewState();
}

class _RemoteTextureViewState extends State<RemoteTextureView> {
  @override
  Widget build(BuildContext context) {
    // Flutter 的 `Texture` 控件会填满父级约束框（按纹理原始分辨率拉伸铺满），
    // 与旧 `RemoteFrameView` 的 `Size.infinite` 表现一致，外层用于捕获指针输入的
    // Listener 也能正常命中整块区域。
    return Texture(textureId: widget.textureId);
  }
}
