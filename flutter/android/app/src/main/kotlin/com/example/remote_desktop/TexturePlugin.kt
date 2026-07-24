package com.example.remote_desktop

import android.graphics.Bitmap
import android.graphics.Canvas
import android.view.Surface
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.view.TextureRegistry
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * 真零拷贝纹理插件（Android）。
 *
 * 与 RustDesk `flutter_texture_rgba_renderer` 同构：经 Flutter `TextureRegistry` 创建
 * `SurfaceTexture` 纹理，把其底层共享 `ByteBuffer`(Direct) 的本地基址作为纹理句柄经
 * `getTexturePtr` 返回给 Dart；并导出 C 函数 `rdcore_texture_submit`（见 cpp/rdcore_texture.cpp），
 * 由 Rust 每解码一帧直接调用，把 RGBA 拷进共享缓冲。随后 Dart 的 `markFrameAvailable`
 * 把缓冲绘入 `Surface` 交给 Flutter 合成——像素从不进入 Dart 堆。
 *
 * 已知限制 / 待设备验证：
 *  - 当前为 CPU 合成路径（Bitmap→Surface Canvas），可正确显示但非 GPU 直出；生产应改用
 *    `flutter_gpu_texture_renderer` 的 EGL/GL(`glTexSubImage2D`) 路径以获得最佳性能。
 *  - C 函数按 Android `Bitmap.Config.ARGB_8888` 的本地字节序(小端= B,G,R,A) 写入，
 *    与 Rust 源 RGBA 交换了 R/B；该字节序需在真机验证（不同设备/Android 版本理论上一致）。
 *  - Rust 写缓冲与 `markFrameAvailable` 读缓冲存在竞态（撕裂），生产应加互斥 / 双缓冲。
 */
class TexturePlugin : FlutterPlugin, MethodChannel.MethodCallHandler {
    private var textureRegistry: TextureRegistry? = null
    private val entries = HashMap<Long, Entry>()
    private var nextId: Long = 1

    data class Entry(
        val surfaceEntry: TextureRegistry.SurfaceTextureEntry,
        val surface: Surface,
        var bitmap: Bitmap,
        var buffer: ByteBuffer,
        var width: Int,
        var height: Int,
    )

    override fun onAttachedToEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        val channel = MethodChannel(binding.binaryMessenger, "rdcore.texture")
        channel.setMethodCallHandler(this)
        textureRegistry = binding.textureRegistry
    }

    override fun onDetachedFromEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        textureRegistry = null
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            "create" -> {
                val w = call.argument<Int>("width") ?: return result.error("bad", "width", null)
                val h = call.argument<Int>("height") ?: return result.error("bad", "height", null)
                create(w, h, result)
            }
            "resize" -> {
                val id = call.argument<Long>("textureId") ?: return result.error("bad", "textureId", null)
                val w = call.argument<Int>("width") ?: return result.error("bad", "width", null)
                val h = call.argument<Int>("height") ?: return result.error("bad", "height", null)
                resize(id, w, h, result)
            }
            "getTexturePtr" -> {
                val id = call.argument<Long>("textureId") ?: return result.error("bad", "textureId", null)
                val e = entries[id] ?: return result.error("no", "unknown textureId", null)
                result.success(hashMapOf("ptr" to bufferAddress(e.buffer)))
            }
            "markFrameAvailable" -> {
                val id = call.argument<Long>("textureId") ?: return result.error("bad", "textureId", null)
                markFrameAvailable(id, result)
            }
            "close" -> {
                val id = call.argument<Long>("textureId") ?: return result.error("bad", "textureId", null)
                close(id)
                result.success(null)
            }
            else -> result.notImplemented()
        }
    }

    private fun create(width: Int, height: Int, result: MethodChannel.Result) {
        val registry = textureRegistry ?: return result.error("no", "textureRegistry", null)
        val surfaceEntry = registry.createSurfaceTexture()
        val surfaceTexture = surfaceEntry.surfaceTexture()
        surfaceTexture.setDefaultBufferSize(width, height)
        val surface = Surface(surfaceTexture)
        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        val buffer = ByteBuffer.allocateDirect(width * height * 4).order(ByteOrder.nativeOrder())
        val id = surfaceEntry.id()
        entries[id] = Entry(surfaceEntry, surface, bitmap, buffer, width, height)
        result.success(
            hashMapOf(
                "textureId" to id,
                "ptr" to bufferAddress(buffer),
                "stride" to width * 4,
                "format" to 1, // 源 RGBA；C 函数按 Android 本地字节序(B,G,R,A)写入
            )
        )
    }

    private fun resize(id: Long, width: Int, height: Int, result: MethodChannel.Result) {
        val e = entries[id] ?: return result.error("no", "unknown textureId", null)
        e.surfaceEntry.surfaceTexture().setDefaultBufferSize(width, height)
        e.bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        e.buffer = ByteBuffer.allocateDirect(width * height * 4).order(ByteOrder.nativeOrder())
        e.width = width
        e.height = height
        result.success(
            hashMapOf(
                "textureId" to id,
                "ptr" to bufferAddress(e.buffer),
                "stride" to width * 4,
                "format" to 1,
            )
        )
    }

    private fun markFrameAvailable(id: Long, result: MethodChannel.Result) {
        val e = entries[id] ?: return result.error("no", "unknown textureId", null)
        e.buffer.rewind()
        e.bitmap.copyPixelsFromBuffer(e.buffer)
        val canvas: Canvas = e.surface.lockCanvas(null) ?: return result.error("surf", "lockCanvas null", null)
        canvas.drawBitmap(e.bitmap, 0f, 0f, null)
        e.surface.unlockCanvasAndPost(canvas)
        result.success(null)
    }

    private fun close(id: Long) {
        val e = entries.remove(id) ?: return
        e.surface.release()
        e.surfaceEntry.surfaceTexture().release()
        e.surfaceEntry.release()
    }

    // 经 JNI 取 DirectByteBuffer 的本地基址（C 函数 rdcore_texture_submit 需要）。
    external fun bufferAddress(buffer: ByteBuffer): Long

    companion object {
        init {
            System.loadLibrary("rdcore_texture")
        }
    }
}
