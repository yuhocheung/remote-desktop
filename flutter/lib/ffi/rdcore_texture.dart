import 'dart:async';
import 'dart:ffi' as ffi;
import 'package:flutter/services.dart';

/// 真零拷贝纹理桥：与 Flutter 原生插件（`rdcore.texture`）通信，经 Flutter
/// `TextureRegistry` 创建 GPU 纹理，并把其**同进程内**的可写像素缓冲地址交给 Rust。
///
/// 流程：
/// 1. [create] 让原生端创建一块原生像素缓冲（iOS=`CVPixelBuffer` / Android=`ANativeWindow`）
///    并注册到 Flutter 引擎，返回 [RdCoreTextureHandle]（`textureId` + `ptr` + `stride` + `format`）。
/// 2. Dart 把 `ptr/stride/format` 经 FFI 传给 Rust（`rdcore_connection_attach_texture`），
///    Rust 把解码帧**直接写入该缓冲**——像素绝不进入 Dart 堆（真零拷贝）。
/// 3. 每收到一帧（Rust 写入完成），调用 [markFrameAvailable] 通知 Flutter 重新合成该纹理。
/// 4. 分辨率变化时 [resize] 重新分配缓冲并返回新 `ptr/stride`。
/// 5. [dispose] 释放。
///
/// 若原生插件不可用（桌面 / headless / 未注册），[create] 抛 [MissingPluginException]，
/// 调用方据此回退旧 `pull_frame` 字节路径。
class RdCoreTexture {
  RdCoreTexture._() {
    _channel = const MethodChannel('rdcore.texture');
  }

  factory RdCoreTexture() => _instance;
  static final RdCoreTexture _instance = RdCoreTexture._();

  late final MethodChannel _channel;

  /// 创建一块 `width × height` 的真零拷贝纹理缓冲。
  ///
  /// 返回 [RdCoreTextureHandle]：`textureId` 供 Flutter `Texture` 控件合成；
  /// `ptr` 为原生可写缓冲地址（传给 Rust）；`stride` 为每行字节数；
  /// `format`：0=BGRA（iOS）/ 1=RGBA（Android）。
  /// 原生插件未注册时抛 [MissingPluginException]（调用方回退字节路径）。
  Future<RdCoreTextureHandle> create(int width, int height) async {
    final Map<Object?, Object?>? res = await _channel.invokeMapMethod(
      'create',
      <String, Object>{'width': width, 'height': height},
    );
    if (res == null) {
      throw PlatformException(
        code: 'no-texture',
        message: '原生纹理插件未返回结果',
      );
    }
    final textureId = res['textureId'] as int;
    // 纹理句柄（原生 backing 地址）需单独取——它是 `rdcore_connection_attach_texture`
    // 与 `rdcore_texture_submit` 的桥梁，Rust 不会读写它，只原样回传给插件。
    final ptr = await getTexturePtr(textureId);
    return RdCoreTextureHandle(
      textureId: textureId,
      ptr: ptr,
      stride: (res['stride'] as int?) ?? (width * 4),
      format: (res['format'] as int?) ?? 0,
      width: width,
      height: height,
    );
  }

  /// 取原生纹理的可写 backing 地址（纹理句柄）。Rust 侧 `attach_texture` 与 `submit`
  /// C 函数用它定位原生缓冲。找不到时返回 0。
  Future<int> getTexturePtr(int textureId) async {
    final Map<Object?, Object?>? res = await _channel.invokeMapMethod(
      'getTexturePtr',
      <String, Object>{'textureId': textureId},
    );
    return (res?['ptr'] as int?) ?? 0;
  }

  /// 原生插件导出的 `rdcore_texture_submit` C 函数地址（Dart 经 FFI 取符号后传给 Rust
  /// 注册为全局提交函数）。无原生插件 / 桌面构建返回 0，调用方据此回退字节路径。
  static int get submitFnAddress {
    try {
      final lib = ffi.DynamicLibrary.process();
      // 取插件导出的 C 函数指针（NativeFunction），读其地址传给 Rust 注册为全局提交函数。
      final p = lib.lookup<ffi.NativeFunction<ffi.Void Function(ffi.Pointer<ffi.Void>)>>(
          'rdcore_texture_submit');
      return p.address;
    } on Object {
      return 0;
    }
  }

  /// 把纹理缓冲重新分配为 `width × height`，返回更新后的 [RdCoreTextureHandle]
  /// （新 `ptr`/`stride`）。旧缓冲由原生端在下次确认后释放。
  Future<RdCoreTextureHandle> resize(int textureId, int width, int height) async {
    final Map<Object?, Object?>? res = await _channel.invokeMapMethod(
      'resize',
      <String, Object>{
        'textureId': textureId,
        'width': width,
        'height': height,
      },
    );
    if (res == null) {
      throw PlatformException(
        code: 'no-texture',
        message: '原生纹理插件未返回 resize 结果',
      );
    }
    final ptr = await getTexturePtr(textureId);
    return RdCoreTextureHandle(
      textureId: textureId,
      ptr: ptr,
      stride: (res['stride'] as int?) ?? (width * 4),
      format: (res['format'] as int?) ?? 0,
      width: width,
      height: height,
    );
  }

  /// 通知 Flutter 该 `textureId` 的像素缓冲已更新，触发重新合成。
  /// 这是控制消息（无像素数据），开销极低。
  void markFrameAvailable(int textureId) {
    _channel.invokeMapMethod(
      'markFrameAvailable',
      <String, Object>{'textureId': textureId},
    );
  }

  /// 释放纹理（原生端解除注册并释放缓冲）。返回 Future 以便调用方 await，
  /// 插件未注册时会抛 [MissingPluginException]（调用方捕获忽略即可）。
  Future<void> dispose(int textureId) {
    return _channel.invokeMapMethod(
      'dispose',
      <String, Object>{'textureId': textureId},
    );
  }
}

/// 一块真零拷贝纹理缓冲的句柄。
class RdCoreTextureHandle {
  const RdCoreTextureHandle({
    required this.textureId,
    required this.ptr,
    required this.stride,
    required this.format,
    required this.width,
    required this.height,
  });

  /// Flutter `Texture` 控件使用的纹理 id。
  final int textureId;

  /// 原生可写像素缓冲地址（同进程；传给 Rust 的 `rdcore_connection_attach_texture`）。
  final int ptr;

  /// 每行字节数（允许行对齐）。
  final int stride;

  /// 像素格式：0=BGRA（iOS）/ 1=RGBA（Android）。
  final int format;

  /// 缓冲宽（像素）。
  final int width;

  /// 缓冲高（像素）。
  final int height;
}
