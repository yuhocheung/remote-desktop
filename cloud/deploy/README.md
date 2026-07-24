# Remote Desktop 控制平面公网部署（Track B · M0）

> 目标：让 iPhone（蜂窝/公网）经 **wss 信令 + TURN 中继** 连上 Windows（家庭 NAT 后）。
> 信任模型：控制平面**半可信**——只见"谁连谁"的元数据；屏幕/键鼠走 P2P 或 TURN，
> 全程 E2E 加密（DTLS-SRTP / AEAD），TURN 中继**仅见密文**（架构文档 §1/§5）。

## 组件

| 组件 | 职责 | 端口 |
|---|---|---|
| **coturn** | STUN + TURN 中继（对称 NAT 兜底） | 3478 (udp), 49152-65535 (relay udp) |
| **signaling-svc** | 信令中继（仅转发 SDP/ICE） | 8080（内网明文，caddy 对外 wss） |
| **caddy** | TLS 终结 + wss 反代 | 80/443 |

> ⚠️ **本部署打通「网络链路」层 + 客户端 Peer/TURN 注入。** signaling + TURN 中继已就绪，
> Viewer 真实 WebRTC Peer（缺口 M）与移动端 TURN 注入（缺口 P0）均已闭环；但 **iOS App 尚未构建**
> （需 macOS + Xcode），真机端到端验证待补——见文末「已知限制与剩余待办」。

## 快速开始

```bash
cd cloud/deploy
cp .env.example .env      # 填 DOMAIN / TURN_EXTERNAL_IP / TURN_USER / TURN_PASS
docker compose up -d
```

默认 signaling 为**开放模式**（不设 `SIGNALING_TOKEN_DB` / `SIGNALING_AUTH_TOKEN`），
适合自托管演示。鉴权模式的取舍见下节。

## 无域名纯 IP 部署（联调专用）

无域名时无法签发受信 TLS 证书，用 `docker-compose.nodomain.yml`：去掉 caddy，signaling
直接对外发布明文 `ws://<IP>:8080`；coturn 配置不变。信令仅承载 SDP/ICE 元数据，媒体/输入
仍全程 E2E 加密（P2P 或 TURN 中继，TURN 只见密文），明文 ws 仅限联调/自托管演示。

```bash
cd cloud/deploy
cp .env.example .env      # 填 TURN_EXTERNAL_IP / TURN_USER / TURN_PASS；DOMAIN 可留空
docker compose -f docker-compose.nodomain.yml up -d --build
```

- 安全组放行：`3478/udp`、`49152-65535/udp`、`8080/tcp`（8080 建议限源到自用 IP 段）。
- Host 侧信令地址改为 `ws://<IP>:8080`（`RDCORE_SIGNALING` 或 `--signal`）。
- iOS 端明文 ws 需 ATS 例外（`flutter/ios/Runner/Info.plist` 的 `NSAllowsArbitraryLoads`，
  联调专用，切 wss 后移除）；Viewer 在 App「设置」页填 STUN/TURN/用户名/密码。
- 验证：`nc -zvu <IP> 3478`；`curl -i http://<IP>:8080/` 返回 400/426 即 WS 服务在监听。
- **实测记录（2026-07-21，阿里云）**：国内 VPS 拉不动 Docker Hub 大镜像（`rust:bookworm` 卡死），
  已改为：signaling-svc 在 VPS 原生编译（rustup 走 `https://rsproxy.cn` 镜像 + dnf 装 gcc/openssl-devel，
  dnf 自带 cargo 1.75 太旧读不了 lock v4，须 rustup 装新版），以 systemd 服务 `rdcore-signaling` 运行；
  coturn 镜像小可直拉，用 `docker run --network host` 按 nodomain compose 中同参数启动。
  注意阿里云 VPC 的公网 IP 是 NAT 映射，coturn 只绑内网网卡属正常，`--external-ip` 负责广播公网地址。

## signaling 鉴权模式（重要）

`signaling-svc` 的鉴权由环境变量切换，**三种互斥**：

| 模式 | 启用方式 | 适用 | 注意 |
|---|---|---|---|
| **开放**（默认） | 不设下面两个变量 | 自托管演示 | session 为 16 字节随机值不可猜，但**无凭据校验**；公网多租户勿用 |
| **同机 token 文件** | `SIGNALING_TOKEN_DB=/data/tokens.jsonl` | Host 与信令**同机** | Host 的 `create_pairing()` 把 session 写入该文件，握手校验并焚毁一次性 token。**远程 VPS 部署时 Host 无法写该文件 → 每个连接 401，勿用** |
| **shared-secret** | `SIGNALING_AUTH_TOKEN=<secret>` | 跨机（理论） | ⚠️ 当前 Host 连接**不携带** token（`build_signaling_url` 不追加），会被 401；暂不实用 |

**结论：远程 VPS 部署请用开放模式 + 依赖 E2E 签名/同意层做真实鉴权。** 生产多租户应改用
网关注册 + 信令 token 校验（gateway crate 已具备，但 host↔gateway↔signaling 的注册链路尚未接通）。

## Host（Windows）侧配置

在 Windows 主机启动 `rdcore-desktop` 前设置（指向你的公网域名）：

```bash
# 信令基址（不含尾部斜杠）；服务会拼成 wss://<DOMAIN>/signaling/<32hex session>
set RDCORE_SIGNALING=wss://<DOMAIN>/signaling
# ICE 服务器（rdcore-rtc 从环境变量读取）
set RDCORE_STUN=stun:<DOMAIN>:3478
set RDCORE_TURN_URL=turn:<DOMAIN>:3478?transport=udp
set RDCORE_TURN_USER=<TURN_USER>
set RDCORE_TURN_PASS=<TURN_PASS>
```

Viewer（Flutter/iPhone）侧通过配对邀请里的 `signal` 字段自动拿到 `wss://<DOMAIN>/signaling/...`；
TURN 凭据现可在 App「设置」里配置（STUN / TURN / 用户名 / 密码），配对时自动经 FFI `ice_servers` 参数注入
（缺口 P0 已闭环）；桌面端仍可用 `RDCORE_TURN_*` 环境变量。信令 URL 形态：

```
wss://<DOMAIN>/signaling/<32hex session_id>?token=<一次性 token>
```

`token` 由 Windows Host 的 `create_pairing()` 生成、经带外（扫码/输码）交给 iPhone；
**开放模式下服务端不校验该 token**（仅按 session 是否注册放行）。

## 防火墙

- TURN 只需**入站** 3478(udp) + relay 段 49152-65535(udp)；客户端侧只需**出站**（NAT 后默认允许）。
- caddy 需 80/443 入站（ACME + wss）。

## 验证

```bash
# TURN 端口
nc -zvu <DOMAIN> 3478
# wss 健康检查
curl https://<DOMAIN>/healthz
```

## 已知限制与剩余待办

本部署的「网络链路」层（signaling + TURN 中继）已就绪，且客户端已具备：

- **Viewer 真实 WebRTC Peer（缺口 M，已闭环）**：`rdcore-ffi` 导出 `rdcore_connection_new_viewer` /
  `rdcore_connection_new_host`，Flutter 经 `NativeRtcConnection` 调用；底层是 **Rust `webrtc-rs`**
  （**非** `flutter_webrtc`），握手（Ed25519 验签 → E2E 密钥 → 同意门控）由 Rust 在 `establish()` 内完成。
- **移动端 TURN 注入（缺口 P0，已闭环）**：`AppSettings` 新增 STUN / TURN / 用户名 / 密码字段，
  配对时经 FFI `ice_servers` 参数注入（见上「Viewer 侧」）；桌面端仍可用 `RDCORE_TURN_*` 环境变量。

因此 **端到端代码链路已具备条件**。剩余未闭环项：

1. **iOS App 尚未构建。** `ffi` 已是 `staticlib`，`flutter/ios/Runner` 脚手架在，但需在 **macOS + Xcode**
   上 `flutter build ios`（当前 Windows 沙箱无法完成）。构建后真机端到端验证待补。
2. **移动端输入不完整。** `remote_screen.dart` 只有 `KeyboardListener` + 指针区分，无长按=右键、
   捏合缩放、滚轮滚动等；远端键鼠事件经 WebRTC DataChannel 送达（DataChannel 已通，输入处理逻辑待补）。
3. **coturn 静态凭据（缺口 G2，待做）。** `turnserver.conf` 当前用静态 `--user` 共享凭据，建议改为
   `use-auth-secret` + 动态短凭据（TURN REST API），避免单点凭据泄露导致带宽滥用。
4. **信令鉴权（缺口 L / G3，待做）。** 远程 VPS 部署目前只能用开放模式 + E2E 兜底；生产多租户需接通
   gateway↔signaling 注册链路 + per-session token 校验。

**最近的可落地下一步**：在 macOS + Xcode 上 `flutter build ios` 完成首次真机构建并跑通端到端，
随后补「移动端输入」与「coturn 动态凭据」。
