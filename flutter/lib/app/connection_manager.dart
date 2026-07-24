import 'dart:math';

import 'package:flutter/foundation.dart';

import '../connection/connection_controller.dart';
import '../connection/native_rtc_connection.dart';
import '../connection/pairing.dart';
import '../connection/signaling.dart';
import '../ffi/rdcore_bindings.dart';
import '../models/app_settings.dart';
import '../util/host_label.dart';

/// 一条已建立的连接会话在 UI 侧的记录。
///
/// [controller] 即驱动该会话的 [ConnectionController]（Viewer 侧，供 RemoteScreen 渲染
/// 与捕获输入）。[onDispose] 用于释放该会话背后额外需要清理的资源（例如演示会话里的
/// Host 端控制器）；真实配对会话无需额外清理（Viewer 控制器 dispose 时会释放自身的
/// Rust 会话与本地身份句柄）。
class SessionEntry {
  SessionEntry({
    required this.id,
    required this.controller,
    this.peerName,
    this.onDispose,
    this.invite,
    this.isHost = false,
  });

  /// 会话唯一 id（真实配对用 sessionHex；演示用随机 id）。
  final String id;

  /// 供 RemoteScreen 使用的控制器。
  final ConnectionController controller;

  /// 对端展示名（来自设置或连接后刷新）。
  final String? peerName;

  /// 额外清理回调（可为空）。
  final void Function()? onDispose;

  /// 真实配对的邀请（含 sessionHex / token / baseUrl）。保留它即可在连接关闭/失败后
  /// 免输码一键重连，复用同一 session 与 token；演示 / 手动会话为 null，不支持一键重连。
  final PairingInvite? invite;

  /// 本会话是否为 Host 侧（重新连接时决定走 startHost 还是 connectViaPairing）。
  final bool isHost;
}

/// App 级连接管理器（归 Track A 的 app 主壳）。
///
/// 持有 [AppSettings] 与一组 [SessionEntry]，提供多会话所需的增删查与工厂：
/// - [connectViaPairing]：由 B3 配对邀请开一个经 WebSocket 信令的真实 Viewer 会话。
/// 状态变化通过 [ChangeNotifier] 通知 UI（HomeScreen 用 ListenableBuilder 订阅）。
class ConnectionManager extends ChangeNotifier {
  ConnectionManager({AppSettings settings = const AppSettings()}) : _settings = settings;

  AppSettings _settings;
  AppSettings get settings => _settings;
  set settings(AppSettings v) {
    _settings = v;
    notifyListeners();
  }

  final List<SessionEntry> _sessions = [];
  List<SessionEntry> get sessions => List.unmodifiable(_sessions);

  String _randomId() {
    final r = Random.secure();
    final b = Uint8List(8);
    for (var i = 0; i < b.length; i++) {
      b[i] = r.nextInt(256);
    }
    return b.map((x) => x.toRadixString(16).padLeft(2, '0')).join();
  }

  /// 新增一条会话（测试与工厂方法共用）。返回会话 id。
  ///
  /// 若 [id] 已存在（重连复用同一 sessionHex），先移除旧条目——调用方需确保旧
  /// controller 已释放，避免重复 id 让 [sessionById] 命中陈旧的已关闭会话、
  /// 表现为「点击重连却仍显示已关闭」。
  String addSession(
    ConnectionController controller, {
    String? peerName,
    void Function()? onDispose,
    String? id,
    PairingInvite? invite,
    bool isHost = false,
  }) {
    final sid = id ?? _randomId();
    _sessions.removeWhere((s) => s.id == sid);
    _sessions.add(SessionEntry(
      id: sid,
      controller: controller,
      peerName: peerName,
      onDispose: onDispose,
      invite: invite,
      isHost: isHost,
    ));
    // 桥接：任一 controller 的状态变化（如连接失败）都驱动 manager 通知首页重建，
    // 否则失败只通知到 RemoteScreen，首页会话列表仍停留在「连接中」。
    controller.addListener(notifyListeners);
    notifyListeners();
    return sid;
  }

  /// 真实连接（Viewer 侧）：由配对邀请开一个经真实 WebRTC（Rust 端自管理信令）的会话。
  /// 返回新会话 id；调用方据此导航到 RemoteScreen。
  ///
  /// 注意：本方法需要原生核心（FFI）可用，且信令服务器可达。Host 是 Windows 上的
  /// `rdcore-desktop` Agent，Viewer 即本 Flutter 应用。Rust 端（`rdcore-app::Connection`）
  /// 自己持有信令客户端，跑完签名 Offer/Answer + ICE + E2E 密钥 + 同意，Flutter 侧不再开
  /// 第二路 WebSocket 信令（否则会与 Rust 端同会话重复消费一次性 token）。
  ///
  /// 异步：建连 + 握手（含等 Host 同意）都跑在后台 isolate，本方法只负责 spawn 它并立即
  /// 返回会话 id；实际连接状态由 [ConnectionController] 经端口事件驱动 UI 更新。
  Future<String> connectViaPairing(PairingInvite invite) async {
    final baseUrl = _settings.signalingBaseUrl;
    // 本地开发基址（127.0.0.1 / localhost）走回环 ICE 候选，便于同机 Host 直连。
    final loopback =
        baseUrl.contains('127.0.0.1') || baseUrl.contains('localhost');
    final backend = await NativeRtcConnection.connect(
      _settings.deviceName,
      isHost: false,
      baseUrl: baseUrl,
      sessionHex: invite.sessionHex,
      token: invite.token,
      includeLoopback: loopback,
      iceServers: _settings.iceServersSpec,
    );
    final transport = NoopSignalingTransport(); // 信令由 Rust 端自持
    final controller = ConnectionController(
      backend: backend,
      transport: transport,
      isHost: false,
    );
    // 纹理 id / 激活态变化（resize 换新 id、首帧激活、回退字节）时驱动 UI 重建。
    backend.onTextureChanged = () => controller.notifyListeners();
    final sid = addSession(controller,
        id: invite.sessionHex,
        peerName: _settings.deviceName,
        invite: invite,
        isHost: false);
    controller.start();
    return sid;
  }

  /// 真实连接（Host 侧）：由配对邀请开一个经真实 WebRTC（Rust 端自管理信令）的 Host 会话，
  /// 等待控制端输码连入。返回新会话 id；调用方据此导航到 RemoteScreen。
  ///
  /// 与 [connectViaPairing] 对称：被控端角色既可由 Windows 上的 `rdcore-desktop` Agent 长期承担，
  /// 也可由本 Flutter 应用临时扮演（开发 / 演示场景）。Rust 端（`rdcore-app::Connection`）自己持有
  /// 信令客户端并以 Host 身份跑完握手，Flutter 侧不再开第二路 WebSocket 信令。
  Future<String> startHost(PairingInvite invite) async {
    final baseUrl = _settings.signalingBaseUrl;
    // 本地开发基址（127.0.0.1 / localhost）走回环 ICE 候选，便于同机 Viewer 直连。
    final loopback =
        baseUrl.contains('127.0.0.1') || baseUrl.contains('localhost');
    final backend = await NativeRtcConnection.connect(
      await hostPeerLabel(),
      isHost: true,
      baseUrl: baseUrl,
      sessionHex: invite.sessionHex,
      token: invite.token,
      includeLoopback: loopback,
      iceServers: _settings.iceServersSpec,
    );
    final transport = NoopSignalingTransport(); // 信令由 Rust 端自持
    final controller = ConnectionController(
      backend: backend,
      transport: transport,
      isHost: true,
    );
    // 纹理 id / 激活态变化（resize 换新 id、首帧激活、回退字节）时驱动 UI 重建。
    backend.onTextureChanged = () => controller.notifyListeners();
    final sid = addSession(controller,
        id: invite.sessionHex,
        peerName: _settings.deviceName,
        invite: invite,
        isHost: true);
    controller.start();
    return sid;
  }

  SessionEntry? sessionById(String id) {
    for (final s in _sessions) {
      if (s.id == id) return s;
    }
    return null;
  }

  /// 对已关闭（或失败）的真实配对会话一键重连：复用其保留的 [PairingInvite]，
  /// 先释放旧 controller（不撤销配对：Viewer 重连复用同一 token，Host 重连继续监听
  /// 同一房间），再以同一 id（sessionHex）开一条全新连接顶替，首页列表无缝刷新。
  /// 返回新会话 id。
  ///
  /// 适用：会话处于 [ConnectionPhase.closed]/[ConnectionPhase.failed] 且 [SessionEntry.invite]
  /// 非 null（真实配对）。演示 / 手动会话无 invite，调用方应先判空。
  ///
  /// 与 [removeSession] 的区别：删除是用户主动移除（Host 侧会撤销发布），而重连
  /// 要保留配对让对端仍可接入。
  ///
  /// 注意：配对 token 为一次性，若信令侧已失效（如 Host 已撤销发布）会建连失败——此时
  /// 本方法抛异常，由调用方在 RemoteScreen / SnackBar 展示错误并引导重新扫码。
  Future<String> reconnect(String id) async {
    final entry = sessionById(id);
    if (entry == null) {
      throw StateError('会话不存在，无法重连');
    }
    if (entry.invite == null) {
      throw StateError('该会话未保留配对信息，无法一键重连，请重新配对');
    }
    final invite = entry.invite!;
    final isHost = entry.isHost;
    _disposeSession(id); // 释放旧 controller，不撤销配对
    if (isHost) return startHost(invite);
    return connectViaPairing(invite);
  }

  /// 仅释放会话（dispose controller），不撤销配对、不通知配对服务。
  /// 供 [reconnect] 复用——重连时旧的已关闭会话应就地换出而非撤销对端发布。
  void _disposeSession(String id) {
    final i = _sessions.indexWhere((s) => s.id == id);
    if (i < 0) return;
    final e = _sessions.removeAt(i);
    e.controller.removeListener(notifyListeners);
    if (e.onDispose != null) {
      e.onDispose!();
    } else {
      e.controller.dispose();
    }
    notifyListeners();
  }

  void removeSession(String id) {
    final i = _sessions.indexWhere((s) => s.id == id);
    if (i < 0) return;
    final e = _sessions.removeAt(i);
    // 受控端会话结束 = 受控端停止提供该配对：撤销发布（停心跳 + 删 token 库文件），
    // 配对码在下一次握手时失效。幂等，重复撤销无副作用。
    if (e.controller.isHost) {
      PairingClient(RdCoreLib()).revokeInvite();
    }
    e.controller.removeListener(notifyListeners);
    if (e.onDispose != null) {
      e.onDispose!();
    } else {
      e.controller.dispose();
    }
    notifyListeners();
  }
}
