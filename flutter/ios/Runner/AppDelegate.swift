import Flutter
import UIKit

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
    // 注册真零拷贝纹理插件（rdcore.texture MethodChannel + 导出的 C 函数 rdcore_texture_submit）。
    if let registrar = engineBridge.pluginRegistry.registrar(forPlugin: "rdcore_texture") {
      RdCoreTexturePlugin.register(with: registrar)
    }
  }
}
