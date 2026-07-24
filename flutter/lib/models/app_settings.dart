import 'consent_scope.dart';

// 联调 VPS 的默认信令/STUN/TURN 配置由 tool/sync_flutter_config.py 从
// core/crates/rdcore-desktop/src/config.rs 的 `pub const` 生成（单一来源，避免漂移）。
import 'default_config.dart';

/// App 级配置（归 Track A 的 app 主壳）。
///
/// 目前保存在内存（[ConnectionManager] 持有），未做持久化；后续可接
/// `shared_preferences` / 配置文件。信令基址约定为 `ws://host:port` 或
/// `wss://host:port`，不含路径与查询（路径第一段由会话 id 填充，见
/// [PairingInvite.signalingUrlFromBase]）。
class AppSettings {
  const AppSettings({
    this.deviceName = '我的设备',
    this.signalingBaseUrl = kDefaultSignalingBaseUrl,
    this.defaultScopes = const {ConsentScope.view, ConsentScope.input},
    this.allowInsecureSignaling = false,
    this.stunUrl = kDefaultStunUrl,
    this.turnUrl = kDefaultTurnUrl,
    this.turnUser = kDefaultTurnUser,
    this.turnPass = kDefaultTurnPass,
  });

  /// 本机显示名称（配对/横幅展示用）。
  final String deviceName;

  /// 信令服务器基址（不含路径与查询）。默认指向联调 VPS（`ws://8.138.237.243:8080`，
  /// 纯 IP 无域名明文部署，见 `cloud/deploy/README.md`「无域名纯 IP 部署」）。
  final String signalingBaseUrl;

  /// 新建连接时默认请求的权限范围。
  final Set<ConsentScope> defaultScopes;

  /// 开发模式：接受自签/无效 TLS 证书（配合 `wss://` 自签信令服务）。
  /// 仅用于本地开发，生产必须为 false（由系统信任链校验）。
  final bool allowInsecureSignaling;

  /// STUN 服务器 URL（如 `stun:example.com:3478`）。为空则回退到 Rust 端默认公共 STUN。
  final String? stunUrl;

  /// TURN 中继 URL（如 `turn:example.com:3478?transport=udp`）。对称 NAT / 蜂窝网络必填，
  /// 否则直连失败且无中继兜底。媒体经 TURN 转发但仍由端到端密钥加密，TURN 只见密文。
  final String? turnUrl;

  /// TURN 用户名（与 [turnUrl] 配套）。
  final String? turnUser;

  /// TURN 凭据（密码或长效密钥）。
  final String? turnPass;

  AppSettings copyWith({
    String? deviceName,
    String? signalingBaseUrl,
    Set<ConsentScope>? defaultScopes,
    bool? allowInsecureSignaling,
    String? stunUrl,
    String? turnUrl,
    String? turnUser,
    String? turnPass,
  }) =>
      AppSettings(
        deviceName: deviceName ?? this.deviceName,
        signalingBaseUrl: signalingBaseUrl ?? this.signalingBaseUrl,
        defaultScopes: defaultScopes ?? this.defaultScopes,
        allowInsecureSignaling:
            allowInsecureSignaling ?? this.allowInsecureSignaling,
        stunUrl: stunUrl ?? this.stunUrl,
        turnUrl: turnUrl ?? this.turnUrl,
        turnUser: turnUser ?? this.turnUser,
        turnPass: turnPass ?? this.turnPass,
      );

  /// 生成传给 Rust FFI 的 ICE 服务器清单（管道分隔串，见 `parse_ice_servers`）。
  ///
  /// - STUN：`stun:host:port`
  /// - TURN：`turn:host:port?transport=udp|user|pass`
  ///
  /// 全空时返回 `null`，交由 Rust 端用 `RDCORE_TURN_*` 环境变量兜底（桌面端常用）。
  String? get iceServersSpec {
    final lines = <String>[];
    if (stunUrl != null && stunUrl!.trim().isNotEmpty) {
      lines.add(stunUrl!.trim());
    }
    if (turnUrl != null && turnUrl!.trim().isNotEmpty) {
      final buf = StringBuffer(turnUrl!.trim());
      if (turnUser != null && turnUser!.trim().isNotEmpty) {
        buf.write('|');
        buf.write(turnUser!.trim());
        if (turnPass != null && turnPass!.trim().isNotEmpty) {
          buf.write('|');
          buf.write(turnPass!.trim());
        }
      }
      lines.add(buf.toString());
    }
    return lines.isEmpty ? null : lines.join('\n');
  }
}
