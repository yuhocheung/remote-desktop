// FFI 绑定：public late 字段以私有函数 typedef 作类型是 dart:ffi 的惯用模式，
// 这些 typedef 只在本库内使用，无需公开。故对本文件关闭该 lint。
// ignore_for_file: library_private_types_in_public_api

import 'dart:ffi' as ffi;
import 'dart:io' show Platform;
import 'package:ffi/ffi.dart';

/// 按当前平台打开 `rdcore-ffi` 原生库。
///
/// 各平台产物名与打包位置（见 `windows|linux|macos|android/` 的接线 + `tool/build_ffi.sh`）：
/// - Windows：`rdcore_ffi.dll`，由 `windows/CMakeLists.txt` INSTALL 到可执行同级。
/// - Linux：`librdcore_ffi.so`，由 `linux/CMakeLists.txt` INSTALL 到 `lib/` 并被 rpath 命中。
/// - macOS：`librdcore_ffi.dylib`，随 .app 的 `Contents/Frameworks/` 打包。
/// - Android：`librdcore_ffi.so`，放入 `android/app/src/main/jniLibs/<abi>/`，系统 loader 直接按名解析。
/// - iOS：静态链进 app 可执行文件，用 `DynamicLibrary.process()` 查符号。
ffi.DynamicLibrary _openRdCore() {
  // 允许通过环境变量 RDCORE_FFI_PATH 显式指定原生库绝对路径
  // （CI 流水线 / 打包前冒烟 / 自定义部署场景）。未设置或路径无效时回退到平台默认名。
  final envPath = Platform.environment['RDCORE_FFI_PATH'];
  if (envPath != null && envPath.isNotEmpty) {
    try {
      return ffi.DynamicLibrary.open(envPath);
    } on Object {
      // 路径无效（如 Windows 上误用了 POSIX 风格路径）时忽略，继续走默认名。
    }
  }
  if (Platform.isWindows) {
    return ffi.DynamicLibrary.open('rdcore_ffi.dll');
  }
  if (Platform.isMacOS) {
    return ffi.DynamicLibrary.open('librdcore_ffi.dylib');
  }
  if (Platform.isIOS) {
    // iOS 不允许动态加载随包 .dylib，rdcore-ffi 以静态库链入主可执行文件。
    return ffi.DynamicLibrary.process();
  }
  // Android 与 Linux 均使用 ELF 共享库命名。
  return ffi.DynamicLibrary.open('librdcore_ffi.so');
}

/// 镜像 Rust 端 `#[repr(C)] pub struct RdBytes { data: *mut u8, len: usize }`。
/// `usize` 在 64 位平台为 8 字节，对应 Dart `Uint64`。
final class RdBytes extends ffi.Struct {
  external ffi.Pointer<ffi.Uint8> data;
  @ffi.Uint64()
  external int len;
}

/// 镜像 Rust 端 `#[repr(C)] pub struct RdMediaFrame { width:u32, height:u32, data:*mut u8, len:usize }`。
/// Viewer 拉帧得到此结构，Dart 负责拷贝像素后调用 `rdcore_media_frame_free` 释放。
final class RdMediaFrameNative extends ffi.Struct {
  @ffi.Uint32()
  external int width;
  @ffi.Uint32()
  external int height;
  external ffi.Pointer<ffi.Uint8> data;
  @ffi.Uint64()
  external int len;
}

/// 镜像 Rust 端 `#[repr(C)] pub struct RdAudioFrame { codec:i32, channels:u16,
/// sample_rate:u32, data:*mut u8, len:usize }`。
/// `codec`：0=Raw（16-bit 交错 PCM），1=Opus。Viewer 拉帧得到此结构，Dart 负责拷贝
/// 音频字节后调用 `rdcore_audio_frame_free` 释放。
final class RdAudioFrameNative extends ffi.Struct {
  @ffi.Int32()
  external int codec;
  @ffi.Uint16()
  external int channels;
  @ffi.Uint32()
  external int sampleRate;
  external ffi.Pointer<ffi.Uint8> data;
  @ffi.Uint64()
  external int len;
}

/// 镜像 Rust 端 `#[repr(C)] pub struct RdInputEvent { kind:i32, x:i32, y:i32, button:i32,
/// pressed:i32, delta_x:i16, delta_y:i16, key_code:u32, modifiers:u32 }`。
///
/// `kind` 取值：0=MouseMove 1=MouseButton 2=MouseWheel 3=Key（与 `RdInputKind` 一致）。
final class NativeRdInputEvent extends ffi.Struct {
  @ffi.Int32()
  external int kind;
  @ffi.Int32()
  external int x;
  @ffi.Int32()
  external int y;
  @ffi.Int32()
  external int button;
  @ffi.Int32()
  external int pressed;
  @ffi.Int16()
  external int deltaX;
  @ffi.Int16()
  external int deltaY;
  @ffi.Uint32()
  external int keyCode;
  @ffi.Uint32()
  external int modifiers;
}

/// 镜像 Rust 端 `#[repr(C)] pub struct RdPairingInfo { session_id: [u8;16], token: *mut c_char }`。
/// Dart 拷贝 `sessionId` 与 `token` 后须调 `rdcore_pairing_info_free` 释放。
final class RdPairingInfoNative extends ffi.Struct {
  @ffi.Array(16)
  external ffi.Array<ffi.Uint8> sessionId;
  external ffi.Pointer<Utf8> token;
}

// ───────────────────────── 原生函数签名 ─────────────────────────
typedef _NativeVersion = ffi.Pointer<Utf8> Function();
typedef _Version = ffi.Pointer<Utf8> Function();

typedef _NativeStringFree = ffi.Void Function(ffi.Pointer<Utf8>);
typedef _StringFree = void Function(ffi.Pointer<Utf8>);

typedef _NativeBytesFree = ffi.Void Function(ffi.Pointer<RdBytes>);
typedef _BytesFree = void Function(ffi.Pointer<RdBytes>);

typedef _NativeLastError = ffi.Pointer<Utf8> Function();
typedef _LastError = ffi.Pointer<Utf8> Function();

typedef _NativeIdentityNew = ffi.Pointer<ffi.Void> Function(ffi.Pointer<Utf8>);
typedef _IdentityNew = ffi.Pointer<ffi.Void> Function(ffi.Pointer<Utf8>);

typedef _NativeIdentityFree = ffi.Void Function(ffi.Pointer<ffi.Void>);
typedef _IdentityFree = void Function(ffi.Pointer<ffi.Void>);

typedef _NativeLocalFingerprint = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _LocalFingerprint = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeLocalDeviceId = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);
typedef _LocalDeviceId = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);

typedef _NativeLocalPeerJson = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _LocalPeerJson = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeRememberPeer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>);
typedef _RememberPeer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>);

typedef _NativeSessionNew = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<ffi.Void>, ffi.Int, ffi.Pointer<ffi.Uint8>, ffi.Pointer<Utf8>);
typedef _SessionNew = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<ffi.Void>, int, ffi.Pointer<ffi.Uint8>, ffi.Pointer<Utf8>);

typedef _NativeSessionFree = ffi.Void Function(ffi.Pointer<ffi.Void>);
typedef _SessionFree = void Function(ffi.Pointer<ffi.Void>);

typedef _NativeMakeOffer = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);
typedef _MakeOffer = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);

typedef _NativeIngestOffer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _IngestOffer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeMakeAnswer = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);
typedef _MakeAnswer = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);

typedef _NativeIngestAnswer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _IngestAnswer = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeMakeSessionKey = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);
typedef _MakeSessionKey = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);

typedef _NativeIngestSessionKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _IngestSessionKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeEncrypt = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _Encrypt = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeDecrypt = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _Decrypt = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeHostRequestConsent = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>);
typedef _HostRequestConsent = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<Utf8>);

typedef _NativeHostDecide = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Int, ffi.Uint32, ffi.Int64);
typedef _HostDecide = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, int, int);

typedef _NativeTick = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _Tick = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeHeartbeat = ffi.Void Function(ffi.Pointer<ffi.Void>);
typedef _Heartbeat = void Function(ffi.Pointer<ffi.Void>);

typedef _NativeRevoke = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _Revoke = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeOnDisconnected = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _OnDisconnected = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeConnectionState = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _ConnectionStateFn = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeSecurityIndicator = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Int);
typedef _SecurityIndicatorFn = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int);

typedef _NativePeerDisplayName = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _PeerDisplayName = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativePeerFingerprint = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);
typedef _PeerFingerprint = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativePeerDeviceId = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);
typedef _PeerDeviceId = ffi.Pointer<RdBytes> Function(ffi.Pointer<ffi.Void>);

// ───────────────────── 媒体 / 输入（Track A）原生函数签名 ─────────────────────
typedef _NativeAttachLoopbackMedia = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Void>);
typedef _AttachLoopbackMedia = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Void>);

typedef _NativeHostSetCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint32, ffi.Uint32, ffi.Uint32, ffi.Uint8);
typedef _HostSetCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, int, int, int);

typedef _NativeHostStartCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint32);
typedef _HostStartCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int);

typedef _NativeViewerPullFrame = ffi.Pointer<RdMediaFrameNative> Function(
    ffi.Pointer<ffi.Void>);
typedef _ViewerPullFrame = ffi.Pointer<RdMediaFrameNative> Function(
    ffi.Pointer<ffi.Void>);

typedef _NativeMediaFrameFree = ffi.Void Function(
    ffi.Pointer<RdMediaFrameNative>);
typedef _MediaFrameFree = void Function(ffi.Pointer<RdMediaFrameNative>);

typedef _NativeViewerSendInput = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<NativeRdInputEvent>);
typedef _ViewerSendInput = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<NativeRdInputEvent>);

typedef _NativeViewerSendInputKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>,
    ffi.Uint32,
    ffi.Pointer<ffi.Uint8>,
    ffi.Int32,
    ffi.Uint32);
typedef _ViewerSendInputKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, ffi.Pointer<ffi.Uint8>, int, int);

typedef _NativeHostPollInput = ffi.Pointer<NativeRdInputEvent> Function(
    ffi.Pointer<ffi.Void>);
typedef _HostPollInput = ffi.Pointer<NativeRdInputEvent> Function(
    ffi.Pointer<ffi.Void>);

typedef _NativeInputEventFree = ffi.Void Function(
    ffi.Pointer<NativeRdInputEvent>);
typedef _InputEventFree = void Function(ffi.Pointer<NativeRdInputEvent>);

// ───────────────────── 音频（Track A）原生函数签名 ─────────────────────
typedef _NativeAttachLoopbackAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Void>);
typedef _AttachLoopbackAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Void>);

typedef _NativeHostSetCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint16, ffi.Uint32, ffi.Uint32, ffi.Uint32, ffi.Uint8);
typedef _HostSetCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, int, int, int, int);

typedef _NativeHostStartCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Int32);
typedef _HostStartCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int);

typedef _NativeViewerPullAudio = ffi.Pointer<RdAudioFrameNative> Function(
    ffi.Pointer<ffi.Void>);
typedef _ViewerPullAudio = ffi.Pointer<RdAudioFrameNative> Function(
    ffi.Pointer<ffi.Void>);

typedef _NativeAudioFrameFree = ffi.Void Function(
    ffi.Pointer<RdAudioFrameNative>);
typedef _AudioFrameFree = void Function(ffi.Pointer<RdAudioFrameNative>);

// ───────────────────── 真实 WebRTC 连接（缺口 M 已闭环，音频面缺口 C 已闭环）原生函数签名 ─────────────────────
// 封装 `rdcore-app::Connection`：Rust 端自管理信令，完成 Offer/Answer/ICE/E2E/同意。
// 句柄为 `Pointer<Void>`（与 RdSession/RdLocal 一致）；媒体/输入复用既有
// `RdMediaFrameNative` / `NativeRdInputEvent` / `RdAudioFrameNative` 结构（与 headless 后端内存布局一致）。
typedef _NativeConnectionNewViewer = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<ffi.Void>,
    ffi.Int32,
    ffi.Int32,
    ffi.Int32,
    ffi.Pointer<Utf8>);
typedef _ConnectionNewViewer = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<ffi.Void>,
    int,
    int,
    int,
    ffi.Pointer<Utf8>);

typedef _NativeConnectionNewHost = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<ffi.Void>,
    ffi.Int32,
    ffi.Int32,
    ffi.Int32,
    ffi.Int32,
    ffi.Pointer<Utf8>);
typedef _ConnectionNewHost = ffi.Pointer<ffi.Void> Function(
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<Utf8>,
    ffi.Pointer<ffi.Void>,
    int,
    int,
    int,
    int,
    ffi.Pointer<Utf8>);

typedef _NativeConnectionEstablish = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionEstablish = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeConnectionPullFrame = ffi.Pointer<RdMediaFrameNative> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionPullFrame = ffi.Pointer<RdMediaFrameNative> Function(
    ffi.Pointer<ffi.Void>);

typedef _NativeConnectionSendInput = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<NativeRdInputEvent>);
typedef _ConnectionSendInput = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<NativeRdInputEvent>);

typedef _NativeConnectionRecvInput = ffi.Pointer<NativeRdInputEvent> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionRecvInput = ffi.Pointer<NativeRdInputEvent> Function(
    ffi.Pointer<ffi.Void>);

// 真实 WebRTC 连接上发送「带字符的按键」（KeyWithChar）：与 headless 回环的
// `RdSession` 版 `viewerSendInputKey` 区分——真实路径 Viewer 句柄是 `RdConnection`。
typedef _NativeConnectionSendInputKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint32, ffi.Pointer<ffi.Uint8>, ffi.Int32, ffi.Uint32);
typedef _ConnectionSendInputKey = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, ffi.Pointer<ffi.Uint8>, int, int);

typedef _NativeConnectionStartCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Int32);
typedef _ConnectionStartCapture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int);

typedef _NativeConnectionFree = ffi.Void Function(ffi.Pointer<ffi.Void>);
typedef _ConnectionFree = void Function(ffi.Pointer<ffi.Void>);

typedef _NativeConnectionSecurityIndicator = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionSecurityIndicatorFn = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>);

// 真实 WebRTC 连接的音频面（缺口 C 闭环：复用既有的 `RdAudioFrameNative` 结构，
// 由 `rdcore_connection_pull_audio` / `rdcore_connection_start_capture_audio` 暴露）。
typedef _NativeConnectionPullAudio = ffi.Pointer<RdAudioFrameNative> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionPullAudio = ffi.Pointer<RdAudioFrameNative> Function(
    ffi.Pointer<ffi.Void>);

typedef _NativeConnectionStartCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint16, ffi.Uint32, ffi.Uint32, ffi.Int32);
typedef _ConnectionStartCaptureAudio = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, int, int, int, int);

// ───────────────────── 真零拷贝纹理（终极 Viewer 渲染路径）原生函数签名 ─────────────────────
// Viewer 把原生纹理缓冲（同进程可写地址）挂到连接，Rust 直接写入——像素不进 Dart 堆。
typedef _NativeConnectionAttachTexture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>,
    ffi.Pointer<ffi.Void>,
    ffi.Uint32,
    ffi.Uint32,
    ffi.Uint32,
    ffi.Uint32,
    ffi.Uint32);
typedef _ConnectionAttachTexture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Void>, int, int, int, int, int);

typedef _NativeConnectionDetachTexture = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionDetachTexture = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Void>);

typedef _NativeConnectionRenderToTexture = ffi.Int32 Function(
    ffi.Pointer<ffi.Void>);
typedef _ConnectionRenderToTexture = int Function(ffi.Pointer<ffi.Void>);

typedef _NativeConnectionLastFrameSize = ffi.Int32 Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint32>, ffi.Pointer<ffi.Uint32>);
typedef _ConnectionLastFrameSize = int Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint32>, ffi.Pointer<ffi.Uint32>);

// 全局注册原生插件导出的 `rdcore_texture_submit` C 函数指针（Dart 经 FFI 传入，推送模型）。
typedef _NativeTextureSetSubmitFn = ffi.Void Function(ffi.Pointer<ffi.Void>);
typedef _TextureSetSubmitFn = void Function(ffi.Pointer<ffi.Void>);

// ───────────────────── 文件传输 / 剪贴板（Track B / kimi-k3）原生函数签名 ─────────────────────
// §8 注 1：以下为 B 侧独立绑定块，与 Track A 媒体/输入绑定互不修改。
typedef _NativeFileSendOffer = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint64, ffi.Pointer<Utf8>, ffi.Uint64);
typedef _FileSendOffer = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, int, ffi.Pointer<Utf8>, int);

typedef _NativeFileSendChunk = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint64, ffi.Uint64, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _FileSendChunk = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, int, int, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeFileSendDone = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint64, ffi.Uint64);
typedef _FileSendDone = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, int, int);

typedef _NativeFileHostOnOffer = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _FileHostOnOffer = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeFileHostDecide = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint64, ffi.Int, ffi.Pointer<Utf8>);
typedef _FileHostDecide = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, int, int, ffi.Pointer<Utf8>);

typedef _NativeFileHostOnEvent = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _FileHostOnEvent = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeFileViewerOnDecision = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _FileViewerOnDecision = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeClipboardSend = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Uint64, ffi.Int, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _ClipboardSend = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, int, int, ffi.Pointer<ffi.Uint8>, int);

typedef _NativeClipboardRecv = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, ffi.Uint64);
typedef _ClipboardRecv = ffi.Pointer<RdBytes> Function(
    ffi.Pointer<ffi.Void>, ffi.Pointer<ffi.Uint8>, int);

// ───────────────────── 配对 / 发现（Track B / kimi-k3）原生函数签名 ─────────────────────
typedef _NativeCreatePairing = ffi.Pointer<RdPairingInfoNative> Function();
typedef _CreatePairing = ffi.Pointer<RdPairingInfoNative> Function();

typedef _NativePairingInfoFree = ffi.Void Function(
    ffi.Pointer<RdPairingInfoNative>);
typedef _PairingInfoFree = void Function(ffi.Pointer<RdPairingInfoNative>);

typedef _NativeSessionIdToHex = ffi.Pointer<Utf8> Function(
    ffi.Pointer<ffi.Uint8>);
typedef _SessionIdToHex = ffi.Pointer<Utf8> Function(ffi.Pointer<ffi.Uint8>);

typedef _NativePairingPublish = ffi.Int32 Function(
    ffi.Pointer<ffi.Uint8>, ffi.Pointer<Utf8>);
typedef _PairingPublish = int Function(
    ffi.Pointer<ffi.Uint8>, ffi.Pointer<Utf8>);

typedef _NativePairingRevoke = ffi.Void Function();
typedef _PairingRevoke = void Function();

/// 加载并持有 `rdcore_ffi.dll` 的符号。若原生库不可用（如未放置 .dll），
/// [available] 为 false，调用原生方法会抛 [RdCoreNativeUnavailable]。
class RdCoreLib {
  RdCoreLib._internal() {
    try {
      _lib = _openRdCore();
      _available = true;
      version = _lib.lookupFunction<_NativeVersion, _Version>('rdcore_version');
      stringFree =
          _lib.lookupFunction<_NativeStringFree, _StringFree>('rdcore_string_free');
      bytesFree =
          _lib.lookupFunction<_NativeBytesFree, _BytesFree>('rdcore_bytes_free');
      lastError =
          _lib.lookupFunction<_NativeLastError, _LastError>('rdcore_last_error');
      identityNew =
          _lib.lookupFunction<_NativeIdentityNew, _IdentityNew>('rdcore_identity_new');
      identityFree = _lib.lookupFunction<_NativeIdentityFree, _IdentityFree>(
          'rdcore_identity_free');
      localFingerprint = _lib.lookupFunction<_NativeLocalFingerprint, _LocalFingerprint>(
          'rdcore_local_fingerprint');
      localDeviceId = _lib.lookupFunction<_NativeLocalDeviceId, _LocalDeviceId>(
          'rdcore_local_device_id');
      localPeerJson = _lib.lookupFunction<_NativeLocalPeerJson, _LocalPeerJson>(
          'rdcore_local_peer_json');
      rememberPeer = _lib.lookupFunction<_NativeRememberPeer, _RememberPeer>(
          'rdcore_remember_peer_json');
      sessionNew =
          _lib.lookupFunction<_NativeSessionNew, _SessionNew>('rdcore_session_new');
      sessionFree = _lib.lookupFunction<_NativeSessionFree, _SessionFree>(
          'rdcore_session_free');
      makeOffer =
          _lib.lookupFunction<_NativeMakeOffer, _MakeOffer>('rdcore_make_offer');
      ingestOffer =
          _lib.lookupFunction<_NativeIngestOffer, _IngestOffer>('rdcore_ingest_offer');
      makeAnswer =
          _lib.lookupFunction<_NativeMakeAnswer, _MakeAnswer>('rdcore_make_answer');
      ingestAnswer = _lib.lookupFunction<_NativeIngestAnswer, _IngestAnswer>(
          'rdcore_ingest_answer');
      makeSessionKey = _lib.lookupFunction<_NativeMakeSessionKey, _MakeSessionKey>(
          'rdcore_make_session_key_exchange');
      ingestSessionKey = _lib.lookupFunction<_NativeIngestSessionKey, _IngestSessionKey>(
          'rdcore_ingest_session_key_exchange');
      encrypt = _lib.lookupFunction<_NativeEncrypt, _Encrypt>('rdcore_encrypt');
      decrypt = _lib.lookupFunction<_NativeDecrypt, _Decrypt>('rdcore_decrypt');
      hostRequestConsent =
          _lib.lookupFunction<_NativeHostRequestConsent, _HostRequestConsent>(
              'rdcore_host_request_consent');
      hostDecide =
          _lib.lookupFunction<_NativeHostDecide, _HostDecide>('rdcore_host_decide');
      tick = _lib.lookupFunction<_NativeTick, _Tick>('rdcore_tick');
      heartbeat =
          _lib.lookupFunction<_NativeHeartbeat, _Heartbeat>('rdcore_heartbeat');
      revoke = _lib.lookupFunction<_NativeRevoke, _Revoke>('rdcore_revoke');
      onDisconnected = _lib.lookupFunction<_NativeOnDisconnected, _OnDisconnected>(
          'rdcore_on_disconnected');
      connectionState = _lib.lookupFunction<_NativeConnectionState, _ConnectionStateFn>(
          'rdcore_connection_state');
      securityIndicator = _lib.lookupFunction<_NativeSecurityIndicator,
          _SecurityIndicatorFn>('rdcore_security_indicator');
      peerDisplayName = _lib.lookupFunction<_NativePeerDisplayName, _PeerDisplayName>(
          'rdcore_peer_display_name');
      peerFingerprint = _lib.lookupFunction<_NativePeerFingerprint, _PeerFingerprint>(
          'rdcore_peer_fingerprint');
      peerDeviceId = _lib.lookupFunction<_NativePeerDeviceId, _PeerDeviceId>(
          'rdcore_peer_device_id');
      // 媒体 / 输入（Track A）
      attachLoopbackMedia = _lib.lookupFunction<_NativeAttachLoopbackMedia,
          _AttachLoopbackMedia>('rdcore_session_attach_loopback_media');
      hostSetCapture = _lib.lookupFunction<_NativeHostSetCapture, _HostSetCapture>(
          'rdcore_host_set_capture');
      hostStartCapture = _lib.lookupFunction<_NativeHostStartCapture,
          _HostStartCapture>('rdcore_host_start_capture');
      viewerPullFrame = _lib.lookupFunction<_NativeViewerPullFrame, _ViewerPullFrame>(
          'rdcore_viewer_pull_frame');
      mediaFrameFree = _lib.lookupFunction<_NativeMediaFrameFree, _MediaFrameFree>(
          'rdcore_media_frame_free');
      viewerSendInput = _lib.lookupFunction<_NativeViewerSendInput, _ViewerSendInput>(
          'rdcore_viewer_send_input');
      viewerSendInputKey = _lib.lookupFunction<_NativeViewerSendInputKey,
          _ViewerSendInputKey>('rdcore_viewer_send_input_key');
      hostPollInput = _lib.lookupFunction<_NativeHostPollInput, _HostPollInput>(
          'rdcore_host_poll_input');
      inputEventFree = _lib.lookupFunction<_NativeInputEventFree, _InputEventFree>(
          'rdcore_input_event_free');
      // 音频（Track A）
      attachLoopbackAudio = _lib.lookupFunction<_NativeAttachLoopbackAudio,
          _AttachLoopbackAudio>('rdcore_session_attach_loopback_audio');
      hostSetCaptureAudio = _lib.lookupFunction<_NativeHostSetCaptureAudio,
          _HostSetCaptureAudio>('rdcore_host_set_capture_audio');
      hostStartCaptureAudio = _lib.lookupFunction<_NativeHostStartCaptureAudio,
          _HostStartCaptureAudio>('rdcore_host_start_capture_audio');
      viewerPullAudio = _lib.lookupFunction<_NativeViewerPullAudio, _ViewerPullAudio>(
          'rdcore_viewer_pull_audio');
      audioFrameFree = _lib.lookupFunction<_NativeAudioFrameFree, _AudioFrameFree>(
          'rdcore_audio_frame_free');
      // 文件传输 / 剪贴板（Track B / kimi-k3）
      fileSendOffer = _lib.lookupFunction<_NativeFileSendOffer, _FileSendOffer>(
          'rdcore_file_send_offer');
      fileSendChunk = _lib.lookupFunction<_NativeFileSendChunk, _FileSendChunk>(
          'rdcore_file_send_chunk');
      fileSendDone = _lib.lookupFunction<_NativeFileSendDone, _FileSendDone>(
          'rdcore_file_send_done');
      fileHostOnOffer = _lib.lookupFunction<_NativeFileHostOnOffer, _FileHostOnOffer>(
          'rdcore_file_host_on_offer');
      fileHostDecide = _lib.lookupFunction<_NativeFileHostDecide, _FileHostDecide>(
          'rdcore_file_host_decide');
      fileHostOnEvent = _lib.lookupFunction<_NativeFileHostOnEvent, _FileHostOnEvent>(
          'rdcore_file_host_on_event');
      fileViewerOnDecision = _lib.lookupFunction<_NativeFileViewerOnDecision,
          _FileViewerOnDecision>('rdcore_file_viewer_on_decision');
      clipboardSend = _lib.lookupFunction<_NativeClipboardSend, _ClipboardSend>(
          'rdcore_clipboard_send');
      clipboardRecv = _lib.lookupFunction<_NativeClipboardRecv, _ClipboardRecv>(
          'rdcore_clipboard_recv');
      // 配对 / 发现（Track B / kimi-k3）
      createPairing = _lib.lookupFunction<_NativeCreatePairing, _CreatePairing>(
          'rdcore_create_pairing');
      pairingInfoFree = _lib.lookupFunction<_NativePairingInfoFree, _PairingInfoFree>(
          'rdcore_pairing_info_free');
      sessionIdToHex = _lib.lookupFunction<_NativeSessionIdToHex, _SessionIdToHex>(
          'rdcore_session_id_to_hex');
      // 配对发布 / 撤销（受控端取消 & 刷新二维码）。单独 try：旧版原生库缺这两个
      // 符号时仅降级为「不支持取消/刷新」，不影响其余功能。
      try {
        pairingPublish = _lib.lookupFunction<_NativePairingPublish, _PairingPublish>(
            'rdcore_pairing_publish');
        pairingRevoke = _lib.lookupFunction<_NativePairingRevoke, _PairingRevoke>(
            'rdcore_pairing_revoke');
      } on Object {
        pairingPublish = null;
        pairingRevoke = null;
      }
      // 真实 WebRTC 连接（缺口 M：Viewer Peer）
      connectionNewViewer = _lib.lookupFunction<_NativeConnectionNewViewer,
          _ConnectionNewViewer>('rdcore_connection_new_viewer');
      connectionNewHost = _lib.lookupFunction<_NativeConnectionNewHost,
          _ConnectionNewHost>('rdcore_connection_new_host');
      connectionEstablish = _lib.lookupFunction<_NativeConnectionEstablish,
          _ConnectionEstablish>('rdcore_connection_establish');
      connectionPullFrame = _lib.lookupFunction<_NativeConnectionPullFrame,
          _ConnectionPullFrame>('rdcore_connection_pull_frame');
      connectionSendInput = _lib.lookupFunction<_NativeConnectionSendInput,
          _ConnectionSendInput>('rdcore_connection_send_input');
      connectionSendInputKey = _lib.lookupFunction<_NativeConnectionSendInputKey,
          _ConnectionSendInputKey>('rdcore_connection_send_input_key');
      connectionRecvInput = _lib.lookupFunction<_NativeConnectionRecvInput,
          _ConnectionRecvInput>('rdcore_connection_recv_input');
      connectionStartCapture = _lib.lookupFunction<_NativeConnectionStartCapture,
          _ConnectionStartCapture>('rdcore_connection_start_capture');
      connectionFree = _lib.lookupFunction<_NativeConnectionFree, _ConnectionFree>(
          'rdcore_connection_free');
      connState = _lib.lookupFunction<_NativeConnectionState, _ConnectionStateFn>(
          'rdcore_conn_state');
      // 该符号缺失不致命：安全指示器非关键，Dart 调用方已 try/catch 降级为占位。
      // 容错 lookup 防止单个符号缺失导致整个 RdCoreLib 构造失败（"全有或全无"陷阱）。
      try {
        connectionSecurityIndicator = _lib.lookupFunction<_NativeConnectionSecurityIndicator,
            _ConnectionSecurityIndicatorFn>('rdcore_connection_security_indicator');
      } on Object {
        connectionSecurityIndicator = (_) => ffi.nullptr.cast<Utf8>();
      }
      // 音频面（缺口 C 闭环）。
      connectionPullAudio = _lib.lookupFunction<_NativeConnectionPullAudio,
          _ConnectionPullAudio>('rdcore_connection_pull_audio');
      connectionStartCaptureAudio =
          _lib.lookupFunction<_NativeConnectionStartCaptureAudio,
              _ConnectionStartCaptureAudio>('rdcore_connection_start_capture_audio');
      // 真零拷贝纹理（终极 Viewer 渲染路径）。这些符号在旧版原生库可能缺失：
      // 缺失时降级为 null，Dart 自动回退旧 pull_frame 字节路径（桌面 / headless 兼容）。
      // 用独立 try/catch 包裹（与 connectionSecurityIndicator 同手法）——lookupFunction 在
      // 符号缺失时会抛 ArgumentError，捕获后降级为 null，调用方据此回退。
      try {
        connectionAttachTexture = _lib.lookupFunction<_NativeConnectionAttachTexture,
            _ConnectionAttachTexture>('rdcore_connection_attach_texture');
      } on Object {
        connectionAttachTexture = null;
      }
      try {
        connectionDetachTexture = _lib.lookupFunction<_NativeConnectionDetachTexture,
            _ConnectionDetachTexture>('rdcore_connection_detach_texture');
      } on Object {
        connectionDetachTexture = null;
      }
      try {
        connectionRenderToTexture = _lib.lookupFunction<_NativeConnectionRenderToTexture,
            _ConnectionRenderToTexture>('rdcore_connection_render_to_texture');
      } on Object {
        connectionRenderToTexture = null;
      }
      try {
        connectionLastFrameSize = _lib.lookupFunction<_NativeConnectionLastFrameSize,
            _ConnectionLastFrameSize>('rdcore_connection_last_frame_size');
      } on Object {
        connectionLastFrameSize = null;
      }
      try {
        textureSetSubmitFn = _lib.lookupFunction<_NativeTextureSetSubmitFn,
            _TextureSetSubmitFn>('rdcore_texture_set_submit_fn');
      } on Object {
        textureSetSubmitFn = null;
      }
    } on Object {
      _available = false;
    }
  }

  factory RdCoreLib() => _instance;
  static final RdCoreLib _instance = RdCoreLib._internal();

  late final ffi.DynamicLibrary _lib;
  bool get available => _available;
  bool _available = false;

  late final _Version version;
  late final _StringFree stringFree;
  late final _BytesFree bytesFree;
  late final _LastError lastError;
  late final _IdentityNew identityNew;
  late final _IdentityFree identityFree;
  late final _LocalFingerprint localFingerprint;
  late final _LocalDeviceId localDeviceId;
  late final _LocalPeerJson localPeerJson;
  late final _RememberPeer rememberPeer;
  late final _SessionNew sessionNew;
  late final _SessionFree sessionFree;
  late final _MakeOffer makeOffer;
  late final _IngestOffer ingestOffer;
  late final _MakeAnswer makeAnswer;
  late final _IngestAnswer ingestAnswer;
  late final _MakeSessionKey makeSessionKey;
  late final _IngestSessionKey ingestSessionKey;
  late final _Encrypt encrypt;
  late final _Decrypt decrypt;
  late final _HostRequestConsent hostRequestConsent;
  late final _HostDecide hostDecide;
  late final _Tick tick;
  late final _Heartbeat heartbeat;
  late final _Revoke revoke;
  late final _OnDisconnected onDisconnected;
  late final _ConnectionStateFn connectionState;
  late final _SecurityIndicatorFn securityIndicator;
  late final _PeerDisplayName peerDisplayName;
  late final _PeerFingerprint peerFingerprint;
  late final _PeerDeviceId peerDeviceId;
  late final _AttachLoopbackMedia attachLoopbackMedia;
  late final _HostSetCapture hostSetCapture;
  late final _HostStartCapture hostStartCapture;
  late final _ViewerPullFrame viewerPullFrame;
  late final _MediaFrameFree mediaFrameFree;
  late final _ViewerSendInput viewerSendInput;
  late final _ViewerSendInputKey viewerSendInputKey;
  late final _HostPollInput hostPollInput;
  late final _InputEventFree inputEventFree;
  // 音频（Track A）
  late final _AttachLoopbackAudio attachLoopbackAudio;
  late final _HostSetCaptureAudio hostSetCaptureAudio;
  late final _HostStartCaptureAudio hostStartCaptureAudio;
  late final _ViewerPullAudio viewerPullAudio;
  late final _AudioFrameFree audioFrameFree;
  // 文件传输 / 剪贴板（Track B / kimi-k3）
  late final _FileSendOffer fileSendOffer;
  late final _FileSendChunk fileSendChunk;
  late final _FileSendDone fileSendDone;
  late final _FileHostOnOffer fileHostOnOffer;
  late final _FileHostDecide fileHostDecide;
  late final _FileHostOnEvent fileHostOnEvent;
  late final _FileViewerOnDecision fileViewerOnDecision;
  late final _ClipboardSend clipboardSend;
  late final _ClipboardRecv clipboardRecv;
  // 配对 / 发现（Track B / kimi-k3）
  late final _CreatePairing createPairing;
  late final _PairingInfoFree pairingInfoFree;
  late final _SessionIdToHex sessionIdToHex;
  // 配对发布 / 撤销（受控端取消 & 刷新二维码）；旧版原生库缺符号时为 null。
  _PairingPublish? pairingPublish;
  _PairingRevoke? pairingRevoke;
  // 真实 WebRTC 连接（缺口 M：Viewer Peer）
  late final _ConnectionNewViewer connectionNewViewer;
  late final _ConnectionNewHost connectionNewHost;
  late final _ConnectionEstablish connectionEstablish;
  late final _ConnectionPullFrame connectionPullFrame;
  late final _ConnectionSendInput connectionSendInput;
  late final _ConnectionSendInputKey connectionSendInputKey;
  late final _ConnectionRecvInput connectionRecvInput;
  late final _ConnectionStartCapture connectionStartCapture;
  late final _ConnectionFree connectionFree;
  late final _ConnectionStateFn connState;
  late final _ConnectionSecurityIndicatorFn connectionSecurityIndicator;
  late final _ConnectionPullAudio connectionPullAudio;
  late final _ConnectionStartCaptureAudio connectionStartCaptureAudio;
  // 真零拷贝纹理（终极 Viewer 渲染路径）；旧版原生库缺符号时为 null。
  _ConnectionAttachTexture? connectionAttachTexture;
  _ConnectionDetachTexture? connectionDetachTexture;
  _ConnectionRenderToTexture? connectionRenderToTexture;
  _ConnectionLastFrameSize? connectionLastFrameSize;
  _TextureSetSubmitFn? textureSetSubmitFn;

  void check() {
    if (!_available) {
      throw const RdCoreNativeUnavailable(
          'rdcore-ffi 原生库未加载：请先运行 tool/build_ffi.sh 构建并 stage 到对应平台的 runner '
          '(Windows: rdcore_ffi.dll / Linux/Android: librdcore_ffi.so / macOS: librdcore_ffi.dylib / iOS: 静态链接)。');
    }
  }

  /// 读取并清空上一次原生错误（返回 null 表示无错误）。
  String? takeLastError() {
    if (!_available) return null;
    final p = lastError();
    if (p.address == 0) return null;
    final s = p.toDartString();
    stringFree(p);
    return s;
  }
}

/// 原生库不可用时抛出。
class RdCoreNativeUnavailable implements Exception {
  const RdCoreNativeUnavailable(this.message);
  final String message;
  @override
  String toString() => 'RdCoreNativeUnavailable: $message';
}
