# web/smoke — 浏览器 Viewer 全链路冒烟（信令 + Host + 真实 Chrome）

本目录验证 **web/app（浏览器 Viewer）从配对码输入到首帧渲染** 的完整链路：
本地信令服务 + 本机 Host Agent + vite preview + Playwright 驱动系统 Chrome。

三种模式：

| 模式 | 命令 | 说明 |
|---|---|---|
| headless（默认） | `bash web/smoke/run_smoke.sh` | `NullCaptureSource` 1280×720 纯色合成帧；无头 Chrome；CI 友好 |
| realcapture | `bash web/smoke/run_smoke.sh --realcapture` | `ScrapCaptureSource` **真实 DXGI 抓屏**（主屏原生分辨率）；有头 Chrome |
| reconnect | `bash web/smoke/run_smoke.sh --reconnect` | **断线重连**：首连 → `page.reload()` 同码重连 → 关闭页面新标签再连（headless） |
| input（M2） | `bash web/smoke/run_smoke.sh --input` | **键鼠输入**：真实注入 Host + 有头 Chrome；阶段 1 blackhole 纯映射断言，阶段 2 OS 回环断言（Host enigo 注入本机 → 本页收到真实 OS 事件） |
| autoreconnect（M2） | `bash web/smoke/run_smoke.sh --autoreconnect` | **断线自动重连**：页内钩子掐断信令+P2P 模拟断网，断言指数退避（1s→2s）、同码原地恢复、密钥轮换（headless） |

最近一次结果：五种模式均 **全部通过**（证据：`connected.png`/`result.json`、
`connected-realcapture.png`/`result-realcapture.json`、
`reconnected.png`/`result-reconnect.json` + `result-reconnect-host.json`、
`input.png`/`result-input.json`、`autoreconnected.png`/`result-autoreconnect.json`）。

## 环境前提

| 依赖 | 说明 |
|---|---|
| `target/debug/signaling-svc.exe` | 预建二进制（工作区 core/ 有未提交改动导致 Windows 重编译失败时，直接用预建） |
| `target/debug/rdcore-desktop.exe` | 预建 Host Agent |
| Node.js | `C:\Program Files\nodejs`（脚本已自动加 PATH） |
| `web/app` 已构建 | `npm install && npm run build`（dist/ 由 vite preview 直接伺服） |
| 系统 Chrome | `C:\Program Files\Google\Chrome\Application\chrome.exe`（Playwright `channel: 'chrome'`，不下载 Chromium；自带 H.264 解码器是 WebCodecs 的前提） |
| `playwright-core` | 已列为 web/app devDependency |
| realcapture 额外要求 | 交互式桌面会话（DXGI 抓屏需要显示器会话）；截图会包含本机桌面画面 |

## 运行

```bash
bash web/smoke/run_smoke.sh                  # headless 合成源
bash web/smoke/run_smoke.sh --realcapture    # 真实抓屏（本机屏幕，浏览器有头弹出）
bash web/smoke/run_smoke.sh --reconnect      # 断线重连（同一配对码连三轮）
bash web/smoke/run_smoke.sh --input          # 键鼠输入（真实注入 Host，有头浏览器）
bash web/smoke/run_smoke.sh --autoreconnect  # 断线自动重连（指数退避 + 同码恢复）
# 端口冲突时可覆盖：SIG_PORT=18091 WEB_PORT=14181 bash web/smoke/run_smoke.sh
```

脚本编排（默认端口：信令 `18081`、Web `14171`）：

1. `signaling-svc`：`SIGNALING_TOKEN_DB=<run/token_db.txt>`（per-session token 模式，生产形态）。
2. `rdcore-desktop run --loopback --no-banner --fps 15`（headless 模式追加 `--headless`）：
   `--loopback` 把 127.0.0.1 纳入 ICE 候选（同机回环）；自动下发 `view,input` 授权，**无需人工同意**。
3. `vite preview`：伺服 `web/app/dist`。
4. 从 Host 日志抓出新鲜配对码 `<32hex>:<64hex>`，Playwright 开 Chrome → 填码 → 连接 → 断言 → 截图。
5. `trap EXIT` 兜底杀掉脚本启动的全部后台进程并删除 token 库文件。

运行期日志在 `run/`（`host.log` / `signaling.log` / `web.log`），该目录已 gitignore。

## 配对与 URL 形态（关键差异）

- Host 连信令：`ws://<addr>/<session_hex>`（不带 token，凭"session 已注册"放行）。
- Viewer 连信令：`ws://<addr>/<session_hex>?token=<token>`（SHA-256 比对）。
- **配对 token 不焚毁**（`cloud/crates/signaling-svc/src/lib.rs` §B2：`TokenStore::verify`
  不消耗 token，`reload_from_file` 以文件为事实 reconcile，Host 30s 心跳保鲜）——
  同一配对码在 Host 在线期间可重复建连，这正是重连场景的语义基础。
- **signaling-svc 取 URL 路径首段作为 session_hex**。本地直连**不能**带 `/signaling` 前缀
  （带前缀会把 `"signaling"` 当 session 解析，握手期 400 拒绝）；`/signaling` 是生产网关的路径约定。
- `web/app/src/signaling.ts#urlFor` 的适配规则：
  - 输入含 `ws://`/`wss://` 全前缀 → 原样拼接（本地 smoke 用）；
  - 裸回环地址（`localhost`/`127.0.0.1`/`::1`）→ 自动 `ws://` + 无前缀；
  - 裸远程主机 → `wss://` + `/signaling` 前缀（生产约定，与部署网关对齐）。

## 断言与证据

### 通用断言（每轮连接）

1. **连接建立 + 授权下发**：`#status` 到达「已获 Host 授权，接收画面…」
   （E2E 签名握手、验签、会话密钥交换、consent 解密全部成功）。
2. **首帧渲染**：`window.__framesDecoded > 0`（媒体帧重组 → XChaCha20Poly1305 解密 →
   WebCodecs H.264 Annex-B 解码 → OffscreenCanvas 绘制全链路通）。
3. **画面非空白**：`__lastFrame.lum > 0`。探针把每帧缩采样到 32×18 网格计算亮度
   `lum`（0–255 均值）、`std`（空间标准差）、`diff`（与上一帧平均绝对差），
   主线程在 `window.__lastFrame` / `window.__frameStats{maxStd,maxDiff}` 暴露。
4. **持续流**：首帧后再等若干帧（尽力而为项）。
5. **console 零非豁免错误**：console.error/pageerror 计入 `errors`；
   唯一豁免是下述**已解释的瞬时 401 竞争**（单独计数于 `explainedTransientErrors`）。

### realcapture 追加断言

- **分辨率**：`__lastFrame.width/height` 必须等于 `Win32_VideoController` 报的主屏分辨率
  （脚本自动查询传入；查不到时退化为「不得为 1280×720 合成值」）。
- **空间多样性**：`maxStd > 15`（纯色合成帧实测 ≈0，真实桌面实测 86–115）。
- **时间多样性**：`maxDiff > 2`。真实桌面可能完全静止（整屏平均帧差实测低至 1.67），
  因此 smoke 在采样窗口内用 **CDP `Browser.setWindowBounds` 拖动可见浏览器窗口**制造
  确定性画面变化（扰动后实测 24–65）；无头/合成源模式不需要此扰动。

### reconnect 追加断言（`result-reconnect.json`）

三轮连接（首连 → 刷新重连 → 关页新标签重连），**同一配对码**：

- `roundsConsent` / `roundsFrames`：每轮 consent 下发且帧流恢复。
- **`keyRotatedAfterReload`**：三轮 `__sessionKeyFp`（会话密钥前 4 字节 hex，不暴露密钥本体）
  两两不同 —— X25519 临时密钥随每次新握手轮换，重连不复用旧密钥。
- `viewerIdentityRotated`：三轮 Viewer 指纹两两不同（内存身份每页新生；Host 按 DeviceId
  TOFU 采纳新身份，`rdcore-app::remember_peer_tofu`）。
- `hostFingerprintStable`：三轮 Host 指纹一致（Host 持久身份，TOFU 核对锚点）。
- `roundsRetriedAfter401`：经历 401 竞争但重试恢复的轮数（>0 说明容错路径真实被走过）。

### Host 侧无感证据（`result-reconnect-host.json`，场景 B）

- `hostProcessAliveAtEnd`：Host 进程全程不重启。
- `establishedCount`：Host 日志「连接已建立」次数 = 3（三轮都被接受）。
- `disconnectWaitCount`：「Viewer 已断开，等待重新连接」次数 = 2（两次断线都被原地收容）。
- `tokenFileFreshAfterReconnect`：token 库文件 mtime 在重连后 90s 内（心跳持续保鲜）。
- `pairingUnchanged`：全程同一配对码（脚本只抓取一次，三轮复用）。

### input 追加断言（`result-input.json`，M2-A）

真实注入 Host（非 headless，enigo 真注入本机）+ 有头 Chrome，两阶段：

- **阶段 1（blackhole 纯映射）**：输入发送置 blackhole（构造+记录但不发出，零 OS 副作用），
  Playwright 驱动 canvas 事件，断言 `__inputSent` 记录：坐标按「帧分辨率 ÷ canvas 显示尺寸」
  换算（中心/四分位点精确命中）、左右键 pressed 序列、滚轮格数与符号（正=向下）、
  可打印字符走 `KeyWithChar`（VK+字符）、功能键/快捷键走 `Key`（Ctrl+C 不带字符、
  modifiers 位掩码 Ctrl=2）、seq 严格单调。
- **阶段 2（OS 回环）**：关掉输入开关（防回环 echo），测试钩子真实经 E2E 通道发送 →
  Host enigo 注入本机 → 本页探针收到**真实 OS 事件**：MouseMove 精确命中目标屏幕像素、
  左/右键 + contextmenu、滚轮正负号、`'a'` 字符（enigo.text Unicode 注入）、
  Enter 物理键（VK_RETURN）。证明 Host 收到并正确处理了输入事件。
- 同意门控：consent scopes 含 Input 时输入默认开启（`inputAllowed/inputEnabledByDefault`），
  用户可一键关闭（`inputToggleOffWorks`）。

### autoreconnect 追加断言（`result-autoreconnect.json`，M2-B）

两轮「页内掐断信令+P2P（模拟断网）→ 自动恢复」，全程不刷新页面、不重输配对码：

- `backoffDelaysMs` = [1000, 2000]：指数退避增长（首轮 1s，次轮在「重连中」再掐为 2s）。
- `keyRotatedEachRound`：恢复后 `__sessionKeyFp` 与断前不同（新 X25519 握手，密钥轮换）。
- `framesResumed`：恢复后 `framesDecoded` 继续递增；`finalState` = `connected`。
- `loadIdUnchanged`：`__loadId` 不变证明原页内自愈（非刷新恢复）。
- 副产物覆盖：中途被弃的握手会让 Host 在 30s 握手等待窗口内吃掉我们的 Offer 不回
  Answer——App 的 **Answer 看门狗（15s）** 超时后按断线退避重发，直至与 Host 窗口对齐
  （实测 `attempts=4`：2 次主动掐断 + 2 次 Answer 超时自愈）。

### 产物

| 文件 | 内容 |
|---|---|
| `connected.png` / `result.json` | headless 单轮证据 |
| `connected-realcapture.png` / `result-realcapture.json` | 真实抓屏证据（canvas 内可见真实桌面） |
| `reconnected.png` / `result-reconnect.json` | 重连第三轮成功截图 + 三轮断言明细 |
| `result-reconnect-host.json` | Host 侧无感证据（进程/日志/token 文件） |
| `input.png` / `result-input.json` | 键鼠输入两阶段断言明细（M2-A） |
| `autoreconnected.png` / `result-autoreconnect.json` | 自动重连退避/轮换断言明细（M2-B） |

## 已修复的既有产品问题（留档）

### 瞬时 401 的根因与修复（2026-07-23 已闭环）

重连 smoke 曾反复出现瞬时 401（同 token 被拒后自愈）。逐层排查后的完整结论：

1. **主因：预建 `signaling-svc.exe` 是陈旧二进制**。
   `target/debug/signaling-svc.exe`（07-22 04:10）早于 `lib.rs` 最后修改（07-22 07:26）——
   二进制里是旧版鉴权逻辑，表现为「每次成功升级后下一次握手必 401」的严格交替
   （curl 连发 101/401/101/401 复现；`cargo build -p signaling-svc` 重编后 10/10 全 101，
   含跨 30s 心跳边界）。教训：**行为与源码不符时先核对预建二进制 mtime**。
2. **次因（真实但隐蔽）：token 库文件的 TOCTOU 空读窗口**。
   signaling-svc 每次握手 `reload_from_file`（读到空内容即回收全部文件来源条目），
   Host 旧实现 `std::fs::write` 截断重写，读者可撞上「截断后、写入前」的空读窗口。
   **已修复（Host 侧）**：`core/crates/rdcore-desktop/src/token_db.rs` 改为
   `atomic_replace`——同目录 `.tmp` 写全 + `sync_all` + `rename` 覆盖，读者任意时刻
   只能看到完整旧版或新版（单测覆盖：写入一致/无临时文件残留/300 次并发读写无半行/
   失败清理）。服务端「空读不回收」可作为可选加固，但 Host 原子写已消除窗口，暂不需要。
- **修复后验收**：`--reconnect` smoke `signaling401RaceCount=0`、三轮 `attempts=1,1,1`、
  console 零错误；重试容错逻辑保留作为防御（预期不再触发）。

### 已修复的关键 bug（留档）

**控制通道死锁**（`pipeline.worker.ts`）：原实现 `if (!controlPipe || !sessionKey) return;`
在会话密钥建立前丢弃**所有**控制帧，而 Host 的会话密钥响应恰恰走控制通道 ——
密钥永远到不了，握手必死锁。修复：密钥建立前照常重组控制帧，解密失败再按明文握手消息
回退主线程（`control_plain`）。媒体通道的密钥门槛保持不变（媒体本就该先解密）。

## 排障速查

| 现象 | 先看 |
|---|---|
| 卡在「Offer 已发出，等待 Host 确认…」 | `run/host.log` 是否「等待 Viewer」；配对码是否过期（Host 重启会刷新） |
| 卡在「数据通道已开，交换会话密钥…」 | 控制通道 worker 死锁类 bug 复发？（见上） |
| `信令已断开` 紧跟 400/401 | URL 是否多带 `/signaling` 前缀；持续 401 先核对 `signaling-svc.exe` 是否陈旧（见「瞬时 401 的根因与修复」） |
| VideoDecoder error | 系统 Chrome 才有 H.264。解码器配置 `avc1.42e034`（Level 5.2），覆盖 3440×1440+ |
| ICE 久连不上 | Host 必须带 `--loopback`；浏览器侧已禁 mDNS（`WebRtcHideLocalIpsWithMdns`） |
| realcapture 帧差分断言失败 | 桌面完全静止所致；CDP 窗口扰动应已覆盖，仍失败则查窗口是否被最小化 |

## 已知限制

- headless 模式画面为纯色合成帧，多样性断言仅在 realcapture 模式启用。
- 重连的「异常断连」场景覆盖的是**手动恢复路径**（关页后新开标签重新输码连接）；
  App 内**自动重连**已由 M2-B 实现（指数退避 + 同码原地重连 + 密钥轮换，见上）。
- 键盘输入法（IME）合成：M2-A 的可打印字符走 `KeyWithChar`（Host `enigo.text()`
  Unicode 注入，单字符中文友好），但浏览器 IME 合成串（composition）需要可编辑元素
  才能起合成会话，canvas 场景的中文整段输入未覆盖（Flutter 端用隐藏输入框方案，M3+ 再议）。
- 键盘按住自动重复（`e.repeat`）与 Flutter 端一致被跳过，远端长按重复未支持。
- Answer 之后的 P2P 段（ICE/datachannel  stall）主要靠 `connectionState` 监测；
  「answer 已收但通道永不开」无独立看门狗（罕见，M3+ 再议）。
- Viewer 身份为内存身份（每页新生），Host 侧 TOFU 每轮接纳新 DeviceId；
  身份持久化（IndexedDB）后重连将复用同一 Viewer 身份，行为路径不同，届时需补测。
- 信令日志中的 `Handshake not finished` 来自脚本的 TCP 存活探测（纯 TCP 不走 WS 握手），正常。
- 未测 revoke/心跳超时。
- wasm 体积 559 KB 未压缩（gzip 后约 1/3），M1 后再议。
- smoke 未接入 CI；本机脚本化一键运行。
- 脚本只清理**自己启动**的进程；若机器上有用户自行安装/自启的 rdcore-desktop
  （例如 `C:\Program Files\RdCore\` + HKCU Run 键 `RdCoreHost`），脚本不会触碰。

## 手动复现（不用脚本）

```bash
# 终端 1
SIGNALING_ADDR=127.0.0.1:18081 SIGNALING_TOKEN_DB=C:\path\to\token_db.txt \
  target/debug/signaling-svc.exe
# 终端 2（SIGNALING_TOKEN_DB 必须指向同一文件；真实抓屏去掉 --headless）
SIGNALING_TOKEN_DB=C:\path\to\token_db.txt \
  target/debug/rdcore-desktop.exe run --signal ws://127.0.0.1:18081 \
  --loopback --no-banner --identity-dir C:\path\to\identity --identity-pass test
# 终端 3
cd web/app && npx vite preview --host 127.0.0.1 --port 14171
# 浏览器打开 http://127.0.0.1:14171 ，信令主机填 ws://127.0.0.1:18081 ，
# 配对码从终端 2 输出的「配对码 : <32hex>:<64hex>」复制；
# 重连验证：直接刷新页面再点「连接」（同一配对码，无需更换）
```
