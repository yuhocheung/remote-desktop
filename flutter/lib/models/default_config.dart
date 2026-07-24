// GENERATED FILE — DO NOT EDIT BY HAND.
// 由 tool/sync_flutter_config.py 从
// core/crates/rdcore-desktop/src/config.rs 的 `pub const` 生成。
// 改默认值请改 config.rs，然后重跑脚本（或 `just build-flutter`）。
//
// 这是联调用 VPS 的默认信令/STUN/TURN 配置；生产环境应在 App「设置」页
// 覆盖，或经安全配置通道下发，切勿依赖此硬编码值。

// 信令服务器基址（不含路径与查询）。
const String kDefaultSignalingBaseUrl = 'ws://8.138.237.243:8080';
// STUN 服务器 URL。
const String kDefaultStunUrl = 'stun:8.138.237.243:3478';
// TURN 中继 URL。
const String kDefaultTurnUrl = 'turn:8.138.237.243:3478?transport=udp';
// TURN 用户名。
const String kDefaultTurnUser = 'rdcore';
// TURN 凭据（联调静态共享凭据；生产应改为动态凭据）。
const String kDefaultTurnPass = '84d9e822b2be47739710013bfd15aec91b5cd4363c61b78c';
