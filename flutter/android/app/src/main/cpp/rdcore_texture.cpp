// 真零拷贝纹理提交（Android NDK）。
//
// 导出两份符号：
//  1) rdcore_texture_submit —— 与 RustDesk `FlutterRgbaRendererPluginOnRgba` 同构，由 Rust
//     每解码一帧直接调用（推送模型）。`texture` 为 Kotlin 侧共享 DirectByteBuffer 的本地基址，
//     `buffer` 为 Rust 解码出的 RGBA。像素从不进入 Dart 堆。
//  2) Java_com_example_remote_1desktop_TexturePlugin_bufferAddress —— JNI 助手，返回
//     DirectByteBuffer 的本地基址，供 Kotlin 经 getTexturePtr 回传给 Dart/Rust。
//
// 字节序：Android Bitmap(ARGB_8888) 在小端设备上的内存布局为 (B,G,R,A)。源 format==1 表示
// RGBA，故此处交换 R/B 并保留 A。该假设需在真机验证。

#include <jni.h>
#include <cstring>
#include <cstdint>

extern "C" {

void rdcore_texture_submit(void* texture, const uint8_t* buffer, int len,
                           int width, int height, int stride, int format) {
  if (texture == nullptr || buffer == nullptr) return;
  uint8_t* dst = static_cast<uint8_t*>(texture);
  if (format == 1) {
    // 源 RGBA -> 目标 (B,G,R,A)
    const int row = width * 4;
    for (int y = 0; y < height; ++y) {
      const uint8_t* s = buffer + static_cast<size_t>(y) * row;
      uint8_t* d = dst + static_cast<size_t>(y) * row;
      for (int x = 0; x < width; ++x) {
        const uint8_t* so = s + x * 4;
        uint8_t* d0 = d + x * 4;
        d0[0] = so[2];  // B
        d0[1] = so[1];  // G
        d0[2] = so[0];  // R
        d0[3] = so[3];  // A
      }
    }
  } else {
    std::memcpy(dst, buffer, static_cast<size_t>(len));
  }
}

extern "C" JNIEXPORT jlong JNICALL
Java_com_example_remote_1desktop_TexturePlugin_bufferAddress(JNIEnv* env,
                                                             jclass,
                                                             jobject buffer) {
  return static_cast<jlong>(reinterpret_cast<intptr_t>(env->GetDirectBufferAddress(buffer)));
}

}  // extern "C"
