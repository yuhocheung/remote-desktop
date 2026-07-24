import Flutter
import CoreVideo
import CoreFoundation

/// 真零拷贝纹理插件（iOS）。
///
/// 与 RustDesk `flutter_texture_rgba_renderer` 同构：本插件经 Flutter `TextureRegistry`
/// 创建 GPU 纹理，把其底层 `CVPixelBuffer` 的**可写基址**作为纹理句柄经 `getTexturePtr`
/// 返回给 Dart；同时导出 C 函数 `rdcore_texture_submit`，由 Rust 每解码一帧直接调用，把
/// RGBA 缓冲拷进 `CVPixelBuffer` 并通知 Flutter 重新合成。像素从不进入 Dart 堆。
///
/// 设计要点：
/// 1. 原生插件只负责**像素搬运**（`rdcore_texture_submit` 把 Rust 解码出的 RGBA 拷进
///    `CVPixelBuffer`）。帧通知（让 Flutter 重新合成）由 Dart 媒体循环在 `renderFn` 返回 1
///    后统一经 `tex-frame` → `markFrameAvailable` 触发，**不在 submit 内重复通知**，避免双通知。
/// 2. 锁策略（关键，曾导致画面空白）：`CVPixelBuffer` **不能**在创建时永久持有 CPU 锁——
///    底层 IOSurface 被 CPU 永久锁住时 GPU 无法读取，纹理会整片空白。正确做法是 `submit`
///    每次写入时 `CVPixelBufferLockBaseAddress` / 写完立即 `CVPixelBufferUnlockBaseAddress`
///    （参考 `flutter_texture_rgba_renderer`）。`create`/`resize` 仅在取稳定基址时临时锁一次。
///    代价：写入与 GPU 上传之间可能轻微撕裂；彻底消除需真双缓冲（两块 IOSurface 交替）或改用
///    `flutter_gpu_texture_renderer` 的 Metal 纹理直出路径。
private final class TextureEntry: NSObject, FlutterTexture {
  let textureId: Int64
  var pixelBuffer: CVPixelBuffer
  let registry: FlutterTextureRegistry
  /// 当前基址（submit 据此定位缓冲）。
  var baseAddress: Int

  init(textureId: Int64, pixelBuffer: CVPixelBuffer, registry: FlutterTextureRegistry, baseAddress: Int) {
    self.textureId = textureId
    self.pixelBuffer = pixelBuffer
    self.registry = registry
    self.baseAddress = baseAddress
    super.init()
  }

  /// Flutter 合成时拉取像素缓冲（所有权转交 Flutter，上传后由其释放）。
  func copyPixelBuffer() -> Unmanaged<CVPixelBuffer>? {
    return Unmanaged.passRetained(pixelBuffer)
  }
}

final class RdCoreTexturePlugin: NSObject, FlutterPlugin {
  static let shared = RdCoreTexturePlugin()
  private var textures: [Int64: TextureEntry] = [:]
  private var addrToId: [Int: Int64] = [:]
  private var nextKey: Int64 = 1
  /// 串行化 submit 写入（多连接并发 / 与主线程读之间尽量有序）。
  private let submitLock = NSLock()

  static func register(with registrar: FlutterPluginRegistrar) {
    let channel = FlutterMethodChannel(name: "rdcore.texture", binaryMessenger: registrar.messenger())
    let instance = RdCoreTexturePlugin.shared
    instance.registrar = registrar
    registrar.addMethodCallDelegate(instance, channel: channel)
  }

  private weak var registrar: FlutterPluginRegistrar?

  func handle(_ call: FlutterMethodCall, result: @escaping FlutterResult) {
    switch call.method {
    case "create":
      guard let args = call.arguments as? [String: Any],
            let w = args["width"] as? Int,
            let h = args["height"] as? Int else {
        result(FlutterError(code: "bad_args", message: "create 需要 width/height", details: nil))
        return
      }
      create(width: w, height: h, result: result)
    case "resize":
      guard let args = call.arguments as? [String: Any],
            let id = args["textureId"] as? Int,
            let w = args["width"] as? Int,
            let h = args["height"] as? Int else {
        result(FlutterError(code: "bad_args", message: "resize 需要 textureId/width/height", details: nil))
        return
      }
      resize(textureId: Int64(id), width: w, height: h, result: result)
    case "getTexturePtr":
      guard let args = call.arguments as? [String: Any],
            let id = args["textureId"] as? Int else {
        result(FlutterError(code: "bad_args", message: "getTexturePtr 需要 textureId", details: nil))
        return
      }
      guard let entry = textures[Int64(id)] else {
        result(FlutterError(code: "no_texture", message: "未知 textureId", details: nil))
        return
      }
      result(["ptr": entry.baseAddress])
    case "close", "dispose":
      guard let args = call.arguments as? [String: Any],
            let id = args["textureId"] as? Int else {
        result(FlutterError(code: "bad_args", message: "close/dispose 需要 textureId", details: nil))
        return
      }
      close(textureId: Int64(id))
      result(nil)
    case "markFrameAvailable":
      guard let args = call.arguments as? [String: Any],
            let id = args["textureId"] as? Int else {
        result(FlutterError(code: "bad_args", message: "markFrameAvailable 需要 textureId", details: nil))
        return
      }
      if let entry = textures[Int64(id)] {
        entry.registry.textureFrameAvailable(Int64(id))
      } else {
        // 诊断：textureId 在插件未注册（应为 create 已注册后才调用），说明时序竞态或 id 错配。
        print("[rdcore:tex] markFrameAvailable: 未知 textureId \(id)，跳过（可能时序竞态）")
      }
      result(nil)
    default:
      result(FlutterMethodNotImplemented)
    }
  }

  private func create(width: Int, height: Int, result: @escaping FlutterResult) {
    guard let registry = registrar?.textures() else {
      result(FlutterError(code: "no_registry", message: "TextureRegistry 不可用", details: nil))
      return
    }
    var pb: CVPixelBuffer?
    // 必须带 IOSurface 属性：Flutter 的 copyPixelBuffer() 合成路径要求底层 CVPixelBuffer
    // 由 IOSurface 支持，否则 GPU 无法读取 → 整片空白（白屏）。这是白屏的核心根因之一。
    let attrs: CFDictionary = [
      kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
      kCVPixelBufferMetalCompatibilityKey: true,
      kCVPixelBufferCGImageCompatibilityKey: true,
      kCVPixelBufferCGBitmapContextCompatibilityKey: true,
    ] as CFDictionary
    let status = CVPixelBufferCreate(
      kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA, attrs, &pb)
    guard status == kCVReturnSuccess, let pixelBuffer = pb else {
      result(FlutterError(code: "cv_err", message: "CVPixelBufferCreate 失败", details: "\(status)"))
      return
    }
    // 锁一次以便取到稳定基址，取完立即释放（写入由 submit 每次锁/写/解锁，
    // 绝不能永久持有 CPU 锁，否则底层 IOSurface 无法被 GPU 读取 → 画面空白）。
    CVPixelBufferLockBaseAddress(pixelBuffer, [])
    guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else {
      CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
      result(FlutterError(code: "cv_err", message: "基址为空", details: nil))
      return
    }
    let addr = Int(bitPattern: base)
    CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
    let id = nextKey
    nextKey += 1
    let entry = TextureEntry(textureId: id, pixelBuffer: pixelBuffer, registry: registry, baseAddress: addr)
    let flutterId = registry.register(entry)
    entry.baseAddress = addr
    textures[flutterId] = entry
    addrToId[addr] = flutterId
    // format=0 表示插件底层为 BGRA（CVPixelBuffer），Rust 源为 RGBA 时由 submit 做 R/B 交换。
    result(["textureId": flutterId, "ptr": addr, "stride": width * 4, "format": 0])
  }

  private func resize(textureId: Int64, width: Int, height: Int, result: @escaping FlutterResult) {
    guard let entry = textures[textureId], let registry = registrar?.textures() else {
      result(FlutterError(code: "no_texture", message: "未知/未注册 textureId", details: nil))
      return
    }
    // 释放旧缓冲映射（旧 CVPixelBuffer 已处于解锁态，无需再 Unlock；
    // 直接解绑地址映射，旧缓冲由 Swift ARC + Flutter 的 in-flight IOSurface 引用负责回收）。
    addrToId.removeValue(forKey: entry.baseAddress)
    var pb: CVPixelBuffer?
    // 与 create 一致：必须 IOSurface 支持，否则 copyPixelBuffer() 合成空白。
    let attrs: CFDictionary = [
      kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
      kCVPixelBufferMetalCompatibilityKey: true,
      kCVPixelBufferCGImageCompatibilityKey: true,
      kCVPixelBufferCGBitmapContextCompatibilityKey: true,
    ] as CFDictionary
    let status = CVPixelBufferCreate(
      kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA, attrs, &pb)
    guard status == kCVReturnSuccess, let pixelBuffer = pb else {
      result(FlutterError(code: "cv_err", message: "CVPixelBufferCreate 失败", details: "\(status)"))
      return
    }
    CVPixelBufferLockBaseAddress(pixelBuffer, [])
    guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else {
      CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
      result(FlutterError(code: "cv_err", message: "基址为空", details: nil))
      return
    }
    let addr = Int(bitPattern: base)
    CVPixelBufferUnlockBaseAddress(pixelBuffer, [])
    entry.pixelBuffer = pixelBuffer
    entry.baseAddress = addr
    addrToId[addr] = textureId
    print("[rdcore:tex] resize: 新建缓冲 addr=\(addr) \(width)x\(height)")
    result(["textureId": textureId, "ptr": addr, "stride": width * 4, "format": 0])
  }

  private func close(textureId: Int64) {
    // 与 submit 串行化：避免 unregister/释放纹理的同时，另一个线程的 submit 正写该缓冲
    // （submit 在 Dart isolate 线程、close 在主线程，需 submitLock 互斥）。
    submitLock.lock()
    defer { submitLock.unlock() }
    guard let entry = textures.removeValue(forKey: textureId) else { return }
    addrToId.removeValue(forKey: entry.baseAddress)
    entry.registry.unregisterTexture(textureId)
    // 注意：此处**不**调用 CVPixelBufferUnlockBaseAddress——常态下缓冲在 create/submit 内
    // 已平衡锁（提交完即解锁），close 时必为未锁态；多余解锁反而可能在与 submit 重叠时
    // 造成"提前解锁"，引发 GPU/CPU 争用缓冲。ARC 会在最后一个持有者（含 Flutter in-flight
    // 上传的 retained CVPixelBuffer）释放后回收内存。
  }

  /// 由 C 函数 `rdcore_texture_submit` 调用：把 Rust 解码出的 RGBA 拷进 CVPixelBuffer（BGRA）。
  /// 注意：本函数**只搬运像素**，不通知 Flutter——帧通知由 Dart 媒体循环统一经 `tex-frame`
  /// 触发，避免每帧双通知（submit 内通知 + Dart 内 markFrameAvailable）。
  func submit(texture addr: Int, buffer: UnsafeRawPointer, len: Int,
              width: Int, height: Int, stride: Int, format: Int) {
    submitLock.lock()
    defer { submitLock.unlock() }
    guard let id = addrToId[addr], let entry = textures[id] else {
      // 诊断：找不到纹理地址 → 多半是 resize 竞态 / 已关闭。若空白前频繁出现，说明映射断裂。
      print("[rdcore:tex] submit: 未知纹理地址 \(addr)，丢弃该帧（可能 resize 竞态或已关闭）")
      return
    }
    let dst = entry.pixelBuffer
    // 关键：每次写入时锁、写完立即解锁。绝不能永久持有 CPU 锁——底层 IOSurface
    // 被 CPU 永久锁住时 GPU 无法读取，纹理会整片空白。参考 flutter_texture_rgba_renderer。
    CVPixelBufferLockBaseAddress(dst, [])
    defer { CVPixelBufferUnlockBaseAddress(dst, []) }
    let bpr = CVPixelBufferGetBytesPerRow(dst)
    // 目标为 BGRA；源 format==1 表示 RGBA，逐像素交换 R/B。否则直接按行拷贝。
    if format == 1 {
      let src = buffer.bindMemory(to: UInt8.self, capacity: len)
      let dstPtr = UnsafeMutablePointer<UInt8>(OpaquePointer(CVPixelBufferGetBaseAddress(dst)!))
      for y in 0..<height {
        let s = src + y * width * 4
        let d = dstPtr + y * bpr
        for x in 0..<width {
          let so = s + x * 4
          let d0 = d + x * 4
          d0[0] = so[2]
          d0[1] = so[1]
          d0[2] = so[0]
          d0[3] = so[3]
        }
      }
    } else {
      memcpy(CVPixelBufferGetBaseAddress(dst), buffer, len)
    }
  }
}

/// 导出给 Dart/Rust 调用的 C 函数：原生提交入口（推送模型）。
/// 签名需与 rdcore-ffi 侧 `TextureSubmitFn` 一致。
@_cdecl("rdcore_texture_submit")
func rdcore_texture_submit(_ texture: UnsafeRawPointer?,
                            _ buffer: UnsafeRawPointer?,
                            _ len: Int32,
                            _ width: Int32,
                            _ height: Int32,
                            _ stride: Int32,
                            _ format: Int32) {
  guard let texture = texture, let buffer = buffer else { return }
  RdCoreTexturePlugin.shared.submit(
    texture: Int(bitPattern: texture), buffer: buffer, len: Int(len),
    width: Int(width), height: Int(height), stride: Int(stride), format: Int(format))
}
