# Web Viewer 服务器部署

把浏览器 Viewer 部署到服务器，通过域名或公网 IP 直接访问。
产物是纯静态文件（`web/app/dist`），运行时只需要：**静态托管 + 信令 WebSocket 反代**。

```
浏览器 (https://<host>) ──► Caddy :443 ──┬─ 静态 dist（本目录构建产物）
                                        └─ /signaling/* → signaling-svc :8080（内网明文）
                                                       coturn :3478（跨 NAT 时 TURN 中继）
```

## 0. 硬性前提：HTTPS ↔ wss 必须同协议族

| 页面访问方式 | 可用信令地址 | 功能 |
|---|---|---|
| `https://域名或IP` | `wss://…`（裸主机自动补 `/signaling`） | **全功能**（WebCodecs 解码 / 剪贴板） |
| `http://…`（含裸 IP+HTTP） | `ws://…` | 仅连通性测试：**视频无法解码、剪贴板不可用**（secure context 限制） |

浏览器拦截混合内容：HTTPS 页面**禁止** `ws://` 明文 WebSocket（App 内已加护栏提示）。
localhost / 127.0.0.1 除外（本地开发不受影响）。

## 1. 构建 dist

在仓库根执行（产物在 `web/app/dist`）：

```bash
# A) HTTPS 生产（推荐）：裸主机 → App 自动走 wss://<host>/signaling/<session>
VITE_SIGNALING_DEFAULT='8.138.237.243' bash web/deploy/build-dist.sh

# B) HTTP 测试：明文 ws 直连 signaling-svc 端口
VITE_SIGNALING_DEFAULT='ws://8.138.237.243:8080' bash web/deploy/build-dist.sh
```

脚本会：① 缺省时构建 `rdcore-web` WASM 绑定（需 Rust + wasm32 target + wasm-bindgen CLI）② `tsc` 类型检查 + `vite build`。`VITE_SIGNALING_DEFAULT` 只是输入框默认值，页面仍可手改。

## 2. 服务器配置（Caddy）

`web/deploy/Caddyfile` 提供两个变体，按需并入服务器现有 Caddyfile：

- **变体 A（域名，推荐）**：把 `rd.example.com` 换成你的域名，Caddy 自动签发/续期证书。
- **变体 B（公网 IP，无域名）**：Let's Encrypt IP 证书（shortlived，160h）。
  用 acme.sh 申请（命令已写在 Caddyfile 注释里），需要 80 端口公网可达；acme.sh 会负责自动续签。

然后：

```bash
sudo mkdir -p /srv/rdcore-web
sudo rsync -a --delete web/app/dist/ /srv/rdcore-web/dist/
sudo systemctl reload caddy
```

signaling-svc 保持监听 `127.0.0.1:8080`（与 `cloud/deploy` 现有约定一致，`handle_path` 剥离 `/signaling` 前缀）。

## 3. TURN（跨 NAT 必配）

App 默认只带公共 STUN。Viewer 与 Host 处于不同 NAT 后时 P2P 可能打不通，需要 TURN 中继：

```bash
VITE_ICE_SERVERS='[{"urls":["stun:8.138.237.243:3478","turn:8.138.237.243:3478"],"username":"rdcore","credential":"<TURN_PASS>"}]' \
  VITE_SIGNALING_DEFAULT='8.138.237.243' bash web/deploy/build-dist.sh
```

> ⚠️ **凭据可见性**：`VITE_*` 会被内联进公开 bundle，任何能打开页面的人都能读到 TURN 凭据。
> 桌面端内置联调凭据本就在仓库里，暴露面相同；**生产环境应改为动态凭据**
> （如 coturn REST API 临时凭据经鉴权接口下发），届时把注入改为运行时 fetch 即可。

## 4. 验收清单

1. 浏览器打开 `https://<域名或IP>` → 信令主机已预填默认值；
2. Host 端取配对码（`<32hex>:<64hex>`）粘贴 → 连接；
3. 状态栏依次：Offer 已发出 → Answer 验签通过 → E2E 会话密钥已建立 → 已获 Host 授权 → 首帧已渲染；
4. 显示对端指纹，与 Host 端带外核对；
5. DevTools Console 无 mixed content 报错、无 WebSocket 失败。

## 5. 排障

| 现象 | 排查 |
|---|---|
| 状态栏提示"HTTPS 页面无法用 ws://" | 信令主机改填裸主机（自动 wss）或 `wss://…` |
| WS 立刻 400/401 | Caddy 未剥离 `/signaling` 前缀（须用 `handle_path`）；或配对码过期（Host 心跳 3 分钟 TTL） |
| 停在"等待 Host 确认" | Host 未在线 / session 不匹配；15s 看门狗会自动重发 |
| P2P 连不上（answer 通过但无通道） | 跨 NAT 缺 TURN → 回到第 3 节注入 `VITE_ICE_SERVERS` |
| 证书错误（IP 证书） | acme.sh 续签日志；确认 80 端口可达；证书 160h 到期前会自动换 |
| 浏览器打不开但 `openssl s_client -servername` 正常 | IP 站点缺 `default_sni`：浏览器对 IP 字面量不发 SNI，Caddy 选不中证书会中止握手；在变体 B 全局块加 `default_sni <IP>` |
| 能连接但无画面（仅 HTTP 部署） | 预期内：WebCodecs 需要 secure context，改 HTTPS |
