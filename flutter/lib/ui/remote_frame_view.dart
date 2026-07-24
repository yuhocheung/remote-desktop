import 'dart:async';
import 'dart:typed_data';
import 'dart:ui' as ui;
import 'package:flutter/material.dart';
import '../models/media_frame.dart';

/// 把一帧 RGBA 像素高效渲染为 Flutter 图像。
///
/// 通过 `dart:ui` 把 RGBA 字节解码为 GPU 纹理（`ui.Image`），再用
/// [CustomPaint] 绘制——避免逐像素 `Rect` 绘制，大帧也能高效上屏。
/// 这是对 Rust 端 `rdcore-decode` / `rdcore-render`（RGBA 帧）在 Dart 侧的最终呈现。
class RemoteFrameView extends StatefulWidget {
  const RemoteFrameView({super.key, required this.frame});

  /// 待渲染的帧；为 null 或不合法尺寸时显示占位。
  final RdMediaFrame? frame;

  @override
  State<RemoteFrameView> createState() => _RemoteFrameViewState();
}

class _RemoteFrameViewState extends State<RemoteFrameView> {
  ui.Image? _image;

  @override
  void initState() {
    super.initState();
    _decode();
  }

  @override
  void didUpdateWidget(covariant RemoteFrameView old) {
    super.didUpdateWidget(old);
    if (widget.frame != old.frame) _decode();
  }

  Future<void> _decode() async {
    final f = widget.frame;
    if (f == null || !f.isValid) {
      if (mounted) setState(() => _image = null);
      return;
    }
    try {
      final buffer = await ui.ImmutableBuffer.fromUint8List(
        Uint8List.fromList(f.rgba),
      );
      final descriptor = ui.ImageDescriptor.raw(
        buffer,
        width: f.width,
        height: f.height,
        pixelFormat: ui.PixelFormat.rgba8888,
      );
      final codec = await descriptor.instantiateCodec();
      final fi = await codec.getNextFrame();
      if (mounted) setState(() => _image = fi.image);
    } catch (_) {
      if (mounted) setState(() => _image = null);
    }
  }

  @override
  void dispose() {
    _image?.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    if (_image == null) {
      return const Center(child: CircularProgressIndicator());
    }
    // 填满父容器（而非仅图片原始尺寸），否则外层用于捕获指针输入的 Listener 只占据
    // 图片原始像素大小的区域，指针事件无法命中。图像拉伸铺满可用区域。
    return CustomPaint(
      painter: _FramePainter(_image!),
      size: Size.infinite,
    );
  }
}

class _FramePainter extends CustomPainter {
  _FramePainter(this.image);
  final ui.Image image;

  @override
  void paint(Canvas canvas, Size size) {
    canvas.drawImageRect(
      image,
      Rect.fromLTWH(0, 0, image.width.toDouble(), image.height.toDouble()),
      Rect.fromLTWH(0, 0, size.width, size.height),
      Paint(),
    );
  }

  @override
  bool shouldRepaint(covariant _FramePainter old) => old.image != image;
}
