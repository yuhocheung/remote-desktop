package com.example.remote_desktop

import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine

class MainActivity : FlutterActivity() {
    // 注册真零拷贝纹理插件（rdcore.texture MethodChannel + 导出 C 函数）。
    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        flutterEngine.plugins.add(TexturePlugin())
    }
}
