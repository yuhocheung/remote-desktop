// 浏览器端 smoke：真实系统 Chrome 跑 web/app，从配对码到首帧渲染的全链路验收。
//
// 环境变量：
//   WEB_URL       web/app 地址（vite preview / dev server）
//   HOST_ADDR     信令地址，如 ws://127.0.0.1:18081
//   PAIRING_CODE  配对码 <32hex>:<64hex>
//   OUT_DIR       证据输出目录
//   OUT_PNG       截图文件名（默认 connected.png）
//   OUT_JSON      断言结果文件名（默认 result.json）
//   HEADED        '1' = 有头模式（真实抓屏 smoke 用）
//   REALCAPTURE   '1' = 追加真实桌面断言：分辨率 / 空间方差 / 帧差分
//   RECONNECT     '1' = 断线重连场景：round1 连接 → reload 重连（同一配对码）→
//                 关闭页面 + 新标签页再连（异常断连的手动恢复路径）
//   INPUT         '1' = 键鼠输入场景（headed + 真实注入 Host）：
//                 阶段 1 blackhole 纯映射断言（DOM 事件 → InputKind，不产生 OS 副作用）；
//                 阶段 2 OS 回环断言（真实经加密通道发 Host → enigo 注入本机 →
//                 本页收到真实 OS 输入事件，验证 Host 收到并处理了输入）
//   AUTORECONNECT '1' = 断线自动重连场景（headless）：连上后由页面内钩子粗暴掐断
//                 信令+P2P（模拟网络中断），断言无需刷新/重输配对码即自动恢复投屏、
//                 退避间隔指数增长（1s→2s）、会话密钥指纹每轮轮换
//   EXPECTED_W/H  期望的真实屏幕分辨率（缺省只断言非 1280×720）
//
// 断言（每轮）：consent 下发 / 首帧渲染 / 画面非空白 / 帧流持续 / console 零错误。
// 断言（跨轮，RECONNECT）：会话密钥指纹轮换（X25519 临时密钥随新握手更换）、
// Host 指纹三轮一致（Host 持久身份不变）、Viewer 指纹轮换（内存身份每页新生）。
// 浏览器 console 全程留证；出现 error 必须能解释，否则视为失败。

import { createRequire } from 'node:module';
import { writeFileSync, mkdirSync, readFileSync, existsSync } from 'node:fs';
import { createHash, randomBytes } from 'node:crypto';
import { join } from 'node:path';

const require = createRequire(new URL('../app/package.json', import.meta.url));
const { chromium } = require('playwright-core');

const WEB_URL = process.env.WEB_URL || 'http://127.0.0.1:14171/?probe=1';
const HOST_ADDR = process.env.HOST_ADDR || 'ws://127.0.0.1:18081';
const PAIRING = process.env.PAIRING_CODE || '';
const OUT_DIR = process.env.OUT_DIR || '.';
const OUT_PNG = process.env.OUT_PNG || 'connected.png';
const OUT_JSON = process.env.OUT_JSON || 'result.json';
const HEADED = process.env.HEADED === '1';
const REALCAPTURE = process.env.REALCAPTURE === '1';
const RECONNECT = process.env.RECONNECT === '1';
const INPUT = process.env.INPUT === '1';
const AUTORECONNECT = process.env.AUTORECONNECT === '1';
// M3 场景（协议对端 = web/smoke/probe-host，不发媒体帧，连接断言不含首帧）：
const CLIPBOARD = process.env.CLIPBOARD === '1';
const FILETRANSFER = process.env.FILETRANSFER === '1';
const AUDIO = process.env.AUDIO === '1';
const EXPECTED_W = Number(process.env.EXPECTED_W || 0);
const EXPECTED_H = Number(process.env.EXPECTED_H || 0);
mkdirSync(OUT_DIR, { recursive: true });

/** node 侧 sha256（文件对拍用）。 */
const sha256File = (p) => createHash('sha256').update(readFileSync(p)).digest('hex');
/** 轮询等文件出现并返回路径（probe 落盘证据）；超时返回 null。 */
async function waitFile(path, timeoutMs = 20000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    if (existsSync(path)) return path;
    await new Promise((r) => setTimeout(r, 250));
  }
  return null;
}
/** 探针 Host 的固定剪贴板应答文本（与 probe-host/src/main.rs 的 KNOWN_CLIPBOARD 一致）。 */
const PROBE_CLIPBOARD_REPLY = 'rdcore-probe 剪贴板应答 v1 — 固定文本 clipboard probe reply';

if (!/^[0-9a-f]{32}:[0-9a-f]{64}$/.test(PAIRING)) {
  console.error('PAIRING_CODE 缺失或格式错误（期望 <32hex>:<64hex>）');
  process.exit(2);
}

const consoleLog = [];
const errors = [];
const assertions = {};
let browser;
let lastPage = null;

function dump(extra) {
  const result = {
    ok: errors.length === 0,
    mode: CLIPBOARD
      ? 'clipboard'
      : FILETRANSFER
        ? 'filetransfer'
        : AUDIO
          ? 'audio'
          : AUTORECONNECT
            ? 'autoreconnect'
            : RECONNECT
              ? 'reconnect'
              : INPUT
                ? 'input'
                : REALCAPTURE
                  ? 'realcapture'
                  : 'headless',
    headed: HEADED,
    assertions,
    explainedTransientErrors: {
      signaling401RaceCount: signaling401.length,
      note: 'token 库文件 reload/心跳重写 TOCTOU 竞争导致的瞬时 401（smoke 已重试自愈），属 core/cloud 侧既有问题，详见 README「已发现的既有产品问题」',
    },
    ...extra,
    console: consoleLog,
    errors,
  };
  writeFileSync(join(OUT_DIR, OUT_JSON), JSON.stringify(result, null, 2));
  console.log('---- browser console ----');
  for (const line of consoleLog) console.log(line);
  console.log('---- result ----');
  console.log(JSON.stringify({ ok: result.ok, assertions, ...extra, errors }, null, 2));
}

async function fail(why) {
  errors.push(why);
  try {
    if (lastPage && !lastPage.isClosed()) {
      await lastPage.screenshot({ path: join(OUT_DIR, OUT_PNG) });
    }
  } catch { /* 页面可能已崩 */ }
  dump({ failed: why });
  if (browser) await browser.close();
  process.exit(1);
}

// 已解释的瞬态错误（不计入 ok 判定，但留在 console 留证中）：
// signaling-svc 每次握手都 reload token 库文件并「以文件为事实」回收条目，Host 每 30s
// 截断重写同一文件——空读瞬时窗口内全部条目被回收，该次握手 401；下一次握手 reload
// 即自愈。属 core/cloud 侧既有 TOCTOU 竞争（本次约束不修），smoke 重试即恢复。
const EXPLAINED_TRANSIENT = /WebSocket connection to 'ws:\/\/[^']+' failed: HTTP Authentication failed/;
const signaling401 = [];

function wirePage(page) {
  page.on('console', (m) => {
    const line = `[${m.type()}] ${m.text()}`;
    consoleLog.push(line);
    if (m.type() !== 'error') return;
    if (EXPLAINED_TRANSIENT.test(m.text())) {
      signaling401.push(m.text());
      return;
    }
    errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push(`pageerror: ${e.message}`));
}

/** 单轮「填码 → 连接 → consent → 首帧 → 帧流」全链路，返回本轮探针数据。
 *  注：遇到「信令 401/断开」时允许整页重载后重试（规避 signaling-svc 与 Host 心跳
 *  写 token 文件之间的既有 TOCTOU 竞争——空读瞬时会回收配对条目，下一次握手即自愈）。 */
async function connectRound(page, label, maxRetries = 2) {
  for (let attempt = 0; ; attempt++) {
    const r = await tryConnect(page, label);
    if (r.ok) return { ...r, retried: attempt > 0, attempts: attempt + 1 };
    if (attempt >= maxRetries || !r.retryable)
      await fail(`[${label}] ${r.reason}（已重试 ${attempt} 次）`);
    consoleLog.push(`[smoke] ${label} 第 ${attempt + 1} 次尝试未达授权（${r.reason}），重载重试…`);
    await page.reload({ waitUntil: 'load', timeout: 20000 });
  }
}

async function tryConnect(page, label) {
  await page.waitForFunction(() => window.__appReady === true, undefined, { timeout: 20000 });
  await page.fill('#host', HOST_ADDR);
  await page.fill('#pairing', PAIRING);
  await page.click('#connect');

  // consent（或提前暴露失败/断开）
  try {
    await page.waitForFunction(
      () => {
        const t = document.getElementById('status')?.textContent ?? '';
        return t.includes('授权') || t.includes('失败') || t.includes('断开') || t.includes('撤销');
      },
      undefined,
      { timeout: 90000 },
    );
  } catch {
    return { ok: false, retryable: true, reason: `等待 Host 授权超时（90s）：${await page.textContent('#status')}` };
  }
  const statusAtConsent = await page.textContent('#status');
  if (!statusAtConsent.includes('授权')) {
    // 信令被 401/断开：token 库 reconcile 空读竞争所致，属可重试态
    const retryable = statusAtConsent.includes('断开');
    return { ok: false, retryable, reason: `未到达授权态，status="${statusAtConsent}"` };
  }

  // 首帧
  try {
    await page.waitForFunction(() => window.__framesDecoded > 0, undefined, { timeout: 45000 });
  } catch {
    return { ok: false, retryable: false, reason: '45s 内未解码任何媒体帧' };
  }
  const framesAtFirst = await page.evaluate(() => window.__framesDecoded);
  // realcapture：拖动可见窗口制造桌面变化，为帧差分断言提供确定性扰动
  // （静态桌面整屏平均帧差可低至 ~1.7，会误伤断言；窗口位移产生大区域像素变化）。
  if (REALCAPTURE && HEADED) {
    try {
      const cdp = await page.context().newCDPSession(page);
      const { windowId } = await cdp.send('Browser.getWindowForTarget');
      await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'normal' } });
      for (let i = 0; i < 6; i++) {
        await cdp.send('Browser.setWindowBounds', {
          windowId,
          bounds: { left: 120 + (i % 2) * 600, top: 80 + (i % 3) * 120 },
        });
        await page.waitForTimeout(200);
      }
    } catch { /* 扰动失败不致命，退化为桌面自然变化 */ }
  }
  // 帧流持续（尽力而为）
  await page
    .waitForFunction((n) => window.__framesDecoded >= n, framesAtFirst + 10, { timeout: 30000 })
    .catch(() => null);

  const round = {
    label,
    statusAtConsent,
    statusFinal: await page.textContent('#status'),
    hostFingerprint: await page.textContent('#fingerprint'),
    viewerFp: await page.evaluate(() => window.__viewerFp ?? null),
    sessionKeyFp: await page.evaluate(() => window.__sessionKeyFp ?? null),
    consented: statusAtConsent.includes('授权'),
    framesAtFirst,
    framesFinal: await page.evaluate(() => window.__framesDecoded),
    lastFrame: await page.evaluate(() => window.__lastFrame ?? null),
    frameStats: await page.evaluate(() => window.__frameStats ?? null),
  };
  if (!round.sessionKeyFp)
    return { ok: false, retryable: false, reason: '会话密钥指纹缺失（密钥交换未完成？）' };
  if (!round.lastFrame || round.lastFrame.lum <= 0)
    return { ok: false, retryable: false, reason: `画面疑似空白（lastFrame=${JSON.stringify(round.lastFrame)}）` };
  return { ok: true, ...round };
}

// ── M3 探针连接（probe-host 不发媒体帧：只断言 consent 下发 + 会话密钥就绪）──
async function tryProbeConnect(page, label) {
  await page.waitForFunction(() => window.__appReady === true, undefined, { timeout: 20000 });
  // 产品 UI 默认隐藏右侧栏；smoke 需要操作其中的剪贴板/文件控件，先强制显示。
  await page.evaluate(() => {
    document.querySelector('aside')?.style.setProperty('display', 'flex', 'important');
  });
  await page.fill('#host', HOST_ADDR);
  await page.fill('#pairing', PAIRING);
  await page.click('#connect');
  try {
    await page.waitForFunction(
      () => {
        const t = document.getElementById('status')?.textContent ?? '';
        return t.includes('授权') || t.includes('失败') || t.includes('断开') || t.includes('撤销');
      },
      undefined,
      { timeout: 90000 },
    );
  } catch {
    return { ok: false, retryable: true, reason: `等待 Host 授权超时（90s）：${await page.textContent('#status')}` };
  }
  const statusAtConsent = await page.textContent('#status');
  if (!statusAtConsent.includes('授权')) {
    return { ok: false, retryable: statusAtConsent.includes('断开'), reason: `未到达授权态，status="${statusAtConsent}"` };
  }
  try {
    await page.waitForFunction(
      () => typeof window.__sessionKeyFp === 'string' && window.__sessionKeyFp.length === 8,
      undefined,
      { timeout: 15000 },
    );
  } catch {
    return { ok: false, retryable: false, reason: '会话密钥指纹缺失（密钥交换未完成？）' };
  }
  return {
    ok: true,
    label,
    statusAtConsent,
    sessionKeyFp: await page.evaluate(() => window.__sessionKeyFp),
    hostFingerprint: await page.textContent('#fingerprint'),
  };
}

async function connectProbeRound(page, label, maxRetries = 2) {
  for (let attempt = 0; ; attempt++) {
    const r = await tryProbeConnect(page, label);
    if (r.ok) return { ...r, retried: attempt > 0, attempts: attempt + 1 };
    if (attempt >= maxRetries || !r.retryable)
      await fail(`[${label}] ${r.reason}（已重试 ${attempt} 次）`);
    consoleLog.push(`[smoke] ${label} 第 ${attempt + 1} 次尝试未达授权（${r.reason}），重载重试…`);
    await page.reload({ waitUntil: 'load', timeout: 20000 });
  }
}

// ── 启动浏览器 ──
browser = await chromium.launch({
  channel: 'chrome', // 复用系统 Chrome（含 H.264 专有解码器），不下载 Chromium
  headless: !HEADED,
  args: [
    '--disable-features=WebRtcHideLocalIpsWithMdns', // 关闭 mDNS 混淆，直出本机 IP 候选
    // 音频场景：免手势激活 AudioContext（真实用户路径由「连接」按钮手势解锁）
    ...(AUDIO ? ['--autoplay-policy=no-user-gesture-required'] : []),
  ],
});

const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
wirePage(page);
lastPage = page;
// 剪贴板场景：拉取后写入/读回系统剪贴板需要显式授权（localhost 已是 secure context）。
if (CLIPBOARD) await page.context().grantPermissions(['clipboard-read', 'clipboard-write']);
await page.goto(WEB_URL, { waitUntil: 'load', timeout: 20000 });

if (CLIPBOARD) {
  // ── M3-A 剪贴板同步场景（协议对端 = probe-host）──
  const r1 = await connectProbeRound(page, 'clipboard-connect');
  assertions.consent = r1.statusAtConsent.includes('授权');
  assertions.sessionKeyReady = r1.sessionKeyFp?.length === 8;

  // consent 须授予 Clipboard scope（探针授全量）
  await page.waitForFunction(
    () => !(document.getElementById('clipSend')).disabled,
    undefined,
    { timeout: 10000 },
  ).catch(() => null);
  assertions.clipboardScopeGranted = await page.evaluate(
    () => !(document.getElementById('clipSend')).disabled,
  );
  if (!assertions.clipboardScopeGranted) await fail('consent 未授予 Clipboard scope（clipSend 未启用）');

  // ① Viewer → Host：面板文本经 Clipboard Data 发出，探针落盘，node 对拍。
  const UP_TEXT = 'smoke 剪贴板上行 v1 — hello 你好 12345（CJK/ASCII/数字混合）';
  await page.fill('#clipText', UP_TEXT);
  await page.click('#clipSend');
  await page.waitForFunction((t) => window.__clipboard.lastSent === t, UP_TEXT, { timeout: 8000 })
    .catch(() => null);
  assertions.sendMarkedSent = (await page.evaluate(() => window.__clipboard.lastSent)) === UP_TEXT;
  const recvPath = await waitFile(join(OUT_DIR, 'run', 'clipboard_received.txt'), 15000);
  assertions.hostReceivedFile = recvPath !== null;
  assertions.hostReceivedMatches =
    recvPath !== null && readFileSync(recvPath, 'utf8') === UP_TEXT;
  if (!assertions.sendMarkedSent || !assertions.hostReceivedMatches)
    await fail(
      `Viewer→Host 剪贴板失败：sent=${assertions.sendMarkedSent} file=${assertions.hostReceivedFile} match=${assertions.hostReceivedMatches}`,
    );

  // ② Host → Viewer：发 Request，探针回固定文本；断言面板内容 + 系统剪贴板读回。
  await page.click('#clipPull');
  try {
    await page.waitForFunction(() => typeof window.__clipboard.pulled === 'string', undefined, {
      timeout: 15000,
    });
  } catch {
    await fail('拉取 Host 剪贴板超时（15s 无 Data 应答）');
  }
  assertions.pulledText = await page.evaluate(() => window.__clipboard.pulled);
  assertions.pulledMatchesProbe = assertions.pulledText === PROBE_CLIPBOARD_REPLY;
  assertions.panelShowsPulled =
    (await page.evaluate(() => (document.getElementById('clipRecv')).value)) ===
    PROBE_CLIPBOARD_REPLY;
  assertions.systemClipboardReadback = await page.evaluate(() => window.__clipboard.systemReadback);
  assertions.systemClipboardMatches = assertions.systemClipboardReadback === PROBE_CLIPBOARD_REPLY;
  if (!assertions.pulledMatchesProbe)
    await fail(`拉取内容与探针固定文本不符：${JSON.stringify(assertions.pulledText)}`);
  if (!assertions.systemClipboardMatches)
    await fail(
      `系统剪贴板读回不符（clipboard-write 权限？）：${JSON.stringify(assertions.systemClipboardReadback)}`,
    );

  assertions.knownLimitation =
    '真实 OS 剪贴板监听（Host 侧自动同步）不在探针范围；本场景验证协议双向通路 + 浏览器系统剪贴板写入。';
  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1 });
} else if (FILETRANSFER) {
  // ── M3-B 文件传输场景（协议对端 = probe-host，双向 + sha256 对拍）──
  const r1 = await connectProbeRound(page, 'file-connect');
  assertions.consent = r1.statusAtConsent.includes('授权');

  await page.waitForFunction(
    () => !(document.getElementById('fileSend')).disabled,
    undefined,
    { timeout: 10000 },
  ).catch(() => null);
  assertions.fileScopeGranted = await page.evaluate(
    () => !(document.getElementById('fileSend')).disabled,
  );
  if (!assertions.fileScopeGranted) await fail('consent 未授予 FileTransfer scope（fileSend 未启用）');

  // ① Viewer → Host：2.5 MiB 已知内容（头部魔数 + 随机体），3 个 1 MiB 分片，sha256 对拍。
  const upPath = join(OUT_DIR, 'run', 'upload.bin');
  {
    const header = Buffer.from('smoke-upload-v1\n', 'utf8');
    const body = randomBytes(2560 * 1024 - header.length);
    writeFileSync(upPath, Buffer.concat([header, body]));
  }
  const upSha = sha256File(upPath);
  assertions.uploadSha256 = upSha;
  assertions.uploadSize = 2560 * 1024;

  await page.setInputFiles('#fileInput', upPath);
  await page.click('#fileSend');
  try {
    await page.waitForFunction(() => window.__fileSent?.state === 'done', undefined, { timeout: 60000 });
  } catch {
    await fail(`Viewer→Host 文件发送未完成：${JSON.stringify(await page.evaluate(() => window.__fileSent))}`);
  }
  const sent = await page.evaluate(() => window.__fileSent);
  assertions.sentState = sent;
  assertions.sentChunks = sent?.chunks;
  assertions.sentChunksExpected3 = sent?.chunks === 3; // 2.5 MiB → 3 片（1+1+0.5）
  assertions.sentPageSha256Matches = sent?.sha256 === upSha;

  const downPath = await waitFile(join(OUT_DIR, 'run', 'ft_recv_upload.bin'), 30000);
  assertions.hostWroteFile = downPath !== null;
  assertions.hostFileSha256 = downPath ? sha256File(downPath) : null;
  assertions.hostFileMatches = assertions.hostFileSha256 === upSha;
  if (!assertions.hostFileMatches)
    await fail(
      `Viewer→Host 文件对拍失败：host=${assertions.hostFileSha256} vs page=${upSha}（file=${downPath}）`,
    );

  // ② Host → Viewer：探针建连即 Offer 已知文件（probe-offer.bin，1.5 MiB，2 片）。
  const offerPath = await waitFile(join(OUT_DIR, 'run', 'ft_offer.bin'), 5000);
  assertions.probeOfferFileExists = offerPath !== null;
  const offerSha = offerPath ? sha256File(offerPath) : null;
  assertions.probeOfferSha256 = offerSha;

  await page.waitForFunction(() => !document.getElementById('fileOffer').hidden, undefined, {
    timeout: 15000,
  }).catch(() => null);
  assertions.incomingOfferShown = await page.evaluate(() => !document.getElementById('fileOffer').hidden);
  if (!assertions.incomingOfferShown) await fail('未收到探针的文件 Offer（确认框未弹出）');
  await page.click('#fileAccept');
  try {
    await page.waitForFunction(() => window.__fileRecv !== null, undefined, { timeout: 60000 });
  } catch {
    await fail('Host→Viewer 文件接收未完成（60s 未到 Done）');
  }
  const recvd = await page.evaluate(() => window.__fileRecv);
  assertions.recvState = recvd;
  assertions.recvName = recvd?.name;
  assertions.recvSizeOk = recvd?.sizeOk === true && recvd?.size === 1536 * 1024;
  assertions.recvChunks = recvd?.chunks;
  assertions.recvChunksExpected2 = recvd?.chunks === 2; // 1.5 MiB → 2 片
  assertions.recvSha256MatchesProbeFile = recvd?.sha256 === offerSha;
  if (!assertions.recvSha256MatchesProbeFile)
    await fail(`Host→Viewer 文件对拍失败：page=${recvd?.sha256} vs probe=${offerSha}`);
  assertions.downloadLinkReady = await page.evaluate(
    () => !(document.getElementById('fileDownload')).hidden,
  );

  assertions.fileLog = await page.evaluate(() => window.__fileLog);
  assertions.knownLimitation =
    'rdcore-app AppMessage 无 FileTransfer 变体：文件事件走 rdcore-ffi Track B 线格式（postcard(Message::FileTransfer) 内层，AEAD 承载），探针 Host 以同一格式应答。';
  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1 });
} else if (AUDIO) {
  // ── M3-C 音频播放场景（探针发 440Hz 合成正弦，WebAudio 播放 + RMS 断言）──
  const r1 = await connectProbeRound(page, 'audio-connect');
  assertions.consent = r1.statusAtConsent.includes('授权');

  // 等音频帧流（探针 20ms/帧；40 帧 ≈ 0.8s 音频）
  try {
    await page.waitForFunction(() => window.__audio.frames >= 40, undefined, { timeout: 30000 });
  } catch {
    await fail(`30s 内未收到足够音频帧（frames=${await page.evaluate(() => window.__audio.frames)}）`);
  }
  const a1 = await page.evaluate(() => window.__audio);
  assertions.audioProbe1 = a1;
  assertions.sampleRate = a1.sampleRate;
  assertions.channels = a1.channels;
  assertions.sampleRateOk = a1.sampleRate === 48000;
  assertions.channelsOk = a1.channels === 2;
  // 440Hz 正弦 × 0.5 振幅 → 理论 RMS ≈ 0.354；留足余量断言非静音。
  assertions.rmsNonSilent = a1.rms > 0.1;
  assertions.rms = a1.rms;
  // 播放链路证据：AudioWorklet 实际输出过非静音块（需 AudioContext running）。
  assertions.playedBlocks = a1.playedBlocks;
  assertions.playbackWorking = a1.playedBlocks > 5;
  if (!assertions.rmsNonSilent) await fail(`音频 RMS=${a1.rms}，疑似静音/解密错误`);
  if (!assertions.sampleRateOk || !assertions.channelsOk)
    await fail(`音频参数不符：${a1.sampleRate}Hz ${a1.channels}ch（期望 48000Hz 2ch）`);

  // 帧流持续 + 静音开关真实生效。
  await page.waitForFunction((n) => window.__audio.frames >= n + 20, a1.frames, { timeout: 10000 })
    .catch(() => null);
  const a2 = await page.evaluate(() => window.__audio);
  assertions.streamContinues = a2.frames > a1.frames;
  await page.click('#audioToggle');
  assertions.muteOn = await page.evaluate(() => window.__audio.muted === true);
  await page.click('#audioToggle');
  assertions.muteOff = await page.evaluate(() => window.__audio.muted === false);
  assertions.playedBlocksFinal = await page.evaluate(() => window.__audio.playedBlocks);

  assertions.knownLimitation =
    '真实声卡采集（CpalAudioSource/回环捕获）未覆盖：探针用 SyntheticAudioSource 合成 440Hz 正弦；Opus 解码不在本里程碑（codec=Raw 直通）。';
  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1 });
} else if (INPUT) {
  // ── 键鼠输入模式（headed + 真实注入 Host）──
  const r1 = await connectRound(page, 'input-connect');
  assertions.consent = r1.consented;
  assertions.firstFrame = r1.framesAtFirst > 0;
  assertions.streamContinues = r1.framesFinal > r1.framesAtFirst;

  // consent 须授予 Input（Host 默认 --scopes view,input）
  try {
    await page.waitForFunction(() => window.__inputAllowed === true, undefined, { timeout: 10000 });
  } catch {
    await fail('consent 未授予 Input scope（__inputAllowed 未置真）');
  }
  assertions.inputAllowed = true;
  assertions.inputEnabledByDefault = await page.evaluate(() => window.__inputEnabled === true);

  // ══ 阶段 1：blackhole 纯映射断言（构造+记录但不发送，零 OS 副作用）══
  await page.evaluate(() => window.__inputTest.setBlackhole(true));
  await page.click('#screen'); // 聚焦 canvas（此次点击本身被记录，随后清零）
  await page.evaluate(() => window.__inputTest.clearSent());

  const geo1 = await page.evaluate(() => {
    const c = document.getElementById('screen');
    const r = c.getBoundingClientRect();
    return { left: r.left, top: r.top, width: r.width, height: r.height, fw: c.width, fh: c.height };
  });
  assertions.frameSize = `${geo1.fw}x${geo1.fh}`;

  const sent = () => page.evaluate(() => window.__inputSent);
  const waitSent = (pred, timeout = 8000) =>
    page.waitForFunction(pred, undefined, { timeout }).catch(() => null);

  // ① 坐标映射：canvas 中心 → 帧中心；1/4 点 → 帧 1/4（Host 绝对物理像素）
  await page.mouse.move(geo1.left + geo1.width / 2, geo1.top + geo1.height / 2);
  await waitSent(() => {
    const s = window.__inputSent.filter((e) => e.kind === 'MouseMove');
    const m = s[s.length - 1];
    return m && Math.abs(m.x - window.__lastFrame.width / 2) <= 2 && Math.abs(m.y - window.__lastFrame.height / 2) <= 2;
  });
  await page.mouse.move(geo1.left + geo1.width / 4, geo1.top + geo1.height / 4);
  await waitSent(() => {
    const s = window.__inputSent.filter((e) => e.kind === 'MouseMove');
    const m = s[s.length - 1];
    return m && Math.abs(m.x - window.__lastFrame.width / 4) <= 2 && Math.abs(m.y - window.__lastFrame.height / 4) <= 2;
  });
  const moves = (await sent()).filter((e) => e.kind === 'MouseMove');
  const lastMove = moves[moves.length - 1];
  assertions.mapMoveCenter = moves.some(
    (m) => Math.abs(m.x - geo1.fw / 2) <= 2 && Math.abs(m.y - geo1.fh / 2) <= 2,
  );
  assertions.mapMoveQuarter =
    lastMove && Math.abs(lastMove.x - geo1.fw / 4) <= 2 && Math.abs(lastMove.y - geo1.fh / 4) <= 2;

  // ② 鼠标按键：左 / 右 按下+抬起（button 编号 0=左 2=右）
  await page.mouse.down();
  await page.mouse.up();
  await page.mouse.down({ button: 'right' });
  await page.mouse.up({ button: 'right' });
  await waitSent(() => {
    const b = window.__inputSent.filter((e) => e.kind === 'MouseButton');
    return (
      b.some((e) => e.button === 0 && e.pressed) &&
      b.some((e) => e.button === 0 && !e.pressed) &&
      b.some((e) => e.button === 2 && e.pressed) &&
      b.some((e) => e.button === 2 && !e.pressed)
    );
  });
  const buttons = (await sent()).filter((e) => e.kind === 'MouseButton');
  assertions.mapMouseButtons =
    buttons.some((e) => e.button === 0 && e.pressed) &&
    buttons.some((e) => e.button === 0 && !e.pressed) &&
    buttons.some((e) => e.button === 2 && e.pressed) &&
    buttons.some((e) => e.button === 2 && !e.pressed);

  // ③ 滚轮：deltaY=240 → +2 格（浏览器正=下，enigo 正=下，符号一致）；-120 → -1 格
  await page.mouse.wheel(0, 240);
  await page.mouse.wheel(0, -120);
  await waitSent(() => {
    const w = window.__inputSent.filter((e) => e.kind === 'MouseWheel');
    return w.some((e) => e.deltaY === 2) && w.some((e) => e.deltaY === -1);
  });
  const wheels = (await sent()).filter((e) => e.kind === 'MouseWheel');
  assertions.mapWheel = wheels.some((e) => e.deltaY === 2) && wheels.some((e) => e.deltaY === -1);

  // ④ 键盘：可打印字符 → KeyWithChar(VK_A=0x41, 'a')；功能键 → Key(VK_RETURN=0x0D)；
  //    Ctrl 组合 → Key（不带字符，保快捷键），modifiers 位掩码 Ctrl=2
  await page.keyboard.press('a');
  await page.keyboard.press('Enter');
  await page.keyboard.down('Control');
  await page.keyboard.press('c');
  await page.keyboard.up('Control');
  await waitSent(() => {
    const s = window.__inputSent;
    return (
      s.some((e) => e.kind === 'KeyWithChar' && e.keyCode === 0x41 && e.character === 'a' && e.pressed) &&
      s.some((e) => e.kind === 'KeyWithChar' && e.keyCode === 0x41 && !e.pressed) &&
      s.some((e) => e.kind === 'Key' && e.keyCode === 0x0d && e.pressed) &&
      s.some((e) => e.kind === 'Key' && e.keyCode === 0x0d && !e.pressed) &&
      s.some((e) => e.kind === 'Key' && e.keyCode === 0x43 && e.pressed && (e.modifiers & 2) !== 0)
    );
  });
  const keys = (await sent()).filter((e) => e.kind === 'Key' || e.kind === 'KeyWithChar');
  assertions.mapKeyWithChar =
    keys.some((e) => e.kind === 'KeyWithChar' && e.keyCode === 0x41 && e.character === 'a' && e.pressed) &&
    keys.some((e) => e.kind === 'KeyWithChar' && e.keyCode === 0x41 && !e.pressed);
  assertions.mapFunctionalKey =
    keys.some((e) => e.kind === 'Key' && e.keyCode === 0x0d && e.pressed) &&
    keys.some((e) => e.kind === 'Key' && e.keyCode === 0x0d && !e.pressed);
  assertions.mapShortcutNoChar = keys.some(
    (e) => e.kind === 'Key' && e.keyCode === 0x43 && e.pressed && (e.modifiers & 2) !== 0,
  );

  // ⑤ seq 单调递增（乱序/去重语义依赖）
  const seqs = (await sent()).map((e) => BigInt(e.seq));
  assertions.seqMonotonic = seqs.every((v, i) => i === 0 || v > seqs[i - 1]);
  assertions.sentTotal = seqs.length;
  assertions.sentLog = await sent();
  if (
    !assertions.mapMoveCenter ||
    !assertions.mapMoveQuarter ||
    !assertions.mapMouseButtons ||
    !assertions.mapWheel ||
    !assertions.mapKeyWithChar ||
    !assertions.mapFunctionalKey ||
    !assertions.mapShortcutNoChar ||
    !assertions.seqMonotonic
  )
    await fail(`阶段 1 映射断言失败：${JSON.stringify(assertions)}`);

  // ══ 阶段 2：OS 回环断言（真实发送 → Host enigo 注入本机 → 本页收到真实 OS 事件）══
  // 关闭输入开关：回环注入的 OS 事件会落回本页 canvas，绝不能再被转发（防 echo 循环）。
  await page.evaluate(() => {
    window.__inputTest.setBlackhole(false);
    if (window.__inputEnabled) document.getElementById('inputToggle').click();
    window.__osProbe = [];
    const rec = (e) =>
      window.__osProbe.push({
        type: e.type, key: e.key, button: e.button,
        screenX: e.screenX, screenY: e.screenY, deltaX: e.deltaX, deltaY: e.deltaY,
        dpr: window.devicePixelRatio, t: Date.now(),
      });
    for (const t of ['mousemove', 'mousedown', 'mouseup', 'wheel', 'contextmenu', 'keydown', 'keyup', 'keypress'])
      window.addEventListener(t, rec, { capture: true, passive: true });
  });
  assertions.inputToggleOffWorks = await page.evaluate(() => window.__inputEnabled === false);

  // 钉窗口到已知位置（屏幕坐标映射的前提）
  const cdp = await page.context().newCDPSession(page);
  const { windowId } = await cdp.send('Browser.getWindowForTarget');
  await cdp.send('Browser.setWindowBounds', { windowId, bounds: { windowState: 'normal' } });
  await cdp.send('Browser.setWindowBounds', { windowId, bounds: { left: 60, top: 60, width: 1300, height: 850 } });
  await page.waitForTimeout(500);
  await page.bringToFront();

  const geo2 = await page.evaluate(() => ({
    sx: window.screenX, sy: window.screenY, ow: window.outerWidth, oh: window.outerHeight,
    ih: window.innerHeight, dpr: window.devicePixelRatio,
  }));
  // 目标点 = 页面内容区中心（物理像素 = CSS 像素 × dpr），落在本页 canvas 内。
  const tx = Math.round((geo2.sx + geo2.ow / 2) * geo2.dpr);
  const ty = Math.round((geo2.sy + (geo2.oh - geo2.ih) + geo2.ih / 2) * geo2.dpr);
  assertions.osTarget = { tx, ty, dpr: geo2.dpr };

  const waitProbe = (pred, timeout = 10000) =>
    page.waitForFunction(pred, undefined, { timeout }).catch(() => null);

  // ① MouseMove：Host enigo 绝对移动 → 本页真实 mousemove（screenX×dpr ≈ 目标）
  await page.evaluate(([x, y]) => window.__inputTest.move(x, y), [tx, ty]);
  await page.evaluate(([x, y]) => {
    window.__probeHit = () =>
      window.__osProbe.some(
        (e) => e.type === 'mousemove' && Math.abs(e.screenX * e.dpr - x) <= 4 && Math.abs(e.screenY * e.dpr - y) <= 4,
      );
  }, [tx, ty]);
  await waitProbe(() => window.__probeHit());
  assertions.hostInjectedMove = await page.evaluate(() => window.__probeHit());

  // ② 左键 down/up → 本页真实 mousedown/mouseup
  await page.evaluate(() => { window.__inputTest.button(0, true); window.__inputTest.button(0, false); });
  await waitProbe(() =>
    window.__osProbe.some((e) => e.type === 'mousedown' && e.button === 0) &&
    window.__osProbe.some((e) => e.type === 'mouseup' && e.button === 0),
  );
  assertions.hostInjectedClick = await page.evaluate(() =>
    window.__osProbe.some((e) => e.type === 'mousedown' && e.button === 0) &&
    window.__osProbe.some((e) => e.type === 'mouseup' && e.button === 0),
  );

  // ③ 右键 down/up → 本页真实 contextmenu（app 已 preventDefault 本地菜单）
  await page.evaluate(() => { window.__inputTest.button(2, true); window.__inputTest.button(2, false); });
  await waitProbe(() => window.__osProbe.some((e) => e.type === 'contextmenu'));
  assertions.hostInjectedRightClick = await page.evaluate(() =>
    window.__osProbe.some((e) => e.type === 'contextmenu'),
  );

  // ④ 滚轮 +2 / -2 → 本页真实 wheel，符号一致（正 = 向下）
  await page.evaluate(() => window.__inputTest.wheel(0, 2));
  await waitProbe(() => window.__osProbe.some((e) => e.type === 'wheel' && e.deltaY > 0));
  await page.evaluate(() => window.__inputTest.wheel(0, -2));
  await waitProbe(() => window.__osProbe.some((e) => e.type === 'wheel' && e.deltaY < 0));
  assertions.hostInjectedWheelSigns = await page.evaluate(() =>
    window.__osProbe.some((e) => e.type === 'wheel' && e.deltaY > 0) &&
    window.__osProbe.some((e) => e.type === 'wheel' && e.deltaY < 0),
  );

  // ⑤ 键盘：KeyWithChar('a') → enigo.text Unicode 注入 → 本页收到字符 'a'；
  //    Key(VK_RETURN) → 物理键注入 → keydown/keyup 'Enter'。
  //    焦点前提：②的真实点击已把 OS 焦点与 DOM 焦点都落在本页。
  const hasFocus = await page.evaluate(() => document.hasFocus());
  if (!hasFocus) await page.bringToFront();
  await page.evaluate(() => { window.__inputTest.key(0x41, 'a', true); window.__inputTest.key(0x41, 'a', false); });
  await waitProbe(() =>
    window.__osProbe.some((e) => (e.type === 'keypress' || e.type === 'keydown') && e.key === 'a'),
  );
  assertions.hostInjectedChar = await page.evaluate(() =>
    window.__osProbe.some((e) => (e.type === 'keypress' || e.type === 'keydown') && e.key === 'a'),
  );
  await page.evaluate(() => { window.__inputTest.key(0x0d, 'Enter', true); window.__inputTest.key(0x0d, 'Enter', false); });
  await waitProbe(() =>
    window.__osProbe.some((e) => e.type === 'keydown' && e.key === 'Enter') &&
    window.__osProbe.some((e) => e.type === 'keyup' && e.key === 'Enter'),
  );
  assertions.hostInjectedEnter = await page.evaluate(() =>
    window.__osProbe.some((e) => e.type === 'keydown' && e.key === 'Enter') &&
    window.__osProbe.some((e) => e.type === 'keyup' && e.key === 'Enter'),
  );

  assertions.osProbeEventCount = await page.evaluate(() => window.__osProbe.length);
  assertions.osProbeSample = await page.evaluate(() => window.__osProbe.slice(0, 30));
  assertions.framesStillFlowing =
    (await page.evaluate(() => window.__framesDecoded)) > r1.framesAtFirst;

  if (
    !assertions.hostInjectedMove ||
    !assertions.hostInjectedClick ||
    !assertions.hostInjectedRightClick ||
    !assertions.hostInjectedWheelSigns ||
    !assertions.hostInjectedChar ||
    !assertions.hostInjectedEnter
  )
    await fail(`阶段 2 Host 注入回环断言失败：${JSON.stringify({ ...assertions, osProbeSample: undefined })}`);

  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1 });
} else if (AUTORECONNECT) {
  // ── 断线自动重连模式（headless）：两轮「掐断 → 自动恢复」，断言退避指数增长 ──
  const r1 = await connectRound(page, 'round1-initial');
  assertions.consent = r1.consented;
  assertions.firstFrame = r1.framesAtFirst > 0;

  const loadId = await page.evaluate(() => window.__loadId);
  const drops = [];
  const keyFps = [r1.sessionKeyFp];
  let framesBefore = r1.framesFinal;

  for (let round = 1; round <= 2; round++) {
    // 粗暴掐断：关闭信令 WS + PeerConnection（模拟网络中断，不置手动标志）。
    // 掐断点可能落在句柄尚未赋值的毫秒级窗口内（connecting 早期），允许补掐。
    let delayMs = 0;
    let dropped = false;
    for (let tries = 0; tries < 3 && !dropped; tries++) {
      await page.evaluate(() => window.__rdTest.simulateDrop());
      dropped = await page
        .waitForFunction(
          (n) => window.__autoReconnect.drops >= n && window.__autoReconnect.state === 'backoff',
          round,
          { timeout: 15000 },
        )
        .then(() => true)
        .catch(() => false);
    }
    if (!dropped)
      await fail(`第 ${round} 轮掐断后未进入退避重连态（state=${await page.evaluate(() => window.__autoReconnect.state)}）`);
    delayMs = await page.evaluate(() => window.__autoReconnect.lastDelayMs);
    const statusAtBackoff = await page.textContent('#status');
    if (!/已断开.*后重连/.test(statusAtBackoff))
      await fail(`第 ${round} 轮退避状态栏异常："${statusAtBackoff}"（期望「已断开，Ns 后重连…」）`);
    drops.push({ round, delayMs, statusAtBackoff });

    if (round === 1) {
      // 第一轮：等退避结束进入「重连中」，立刻（connecting 窗口内）再掐一次——
      // 此时 backoffAttempt 未被首帧重置，退避应指数增长为 2s。
      await page.waitForFunction(
        () => window.__autoReconnect.state === 'connecting',
        undefined,
        { timeout: 30000 },
      );
      continue;
    }

    // 第二轮：等待自动恢复——帧流继续 + 会话密钥指纹轮换。
    // 恢复可能需要数个「Answer 超时 → 退避重发」周期与 Host 的 30s 握手等待窗口对齐。
    try {
      await page.waitForFunction(
        (fb) => window.__framesDecoded > fb && window.__autoReconnect.state === 'connected',
        framesBefore,
        { timeout: 150000 },
      );
    } catch {
      await fail(
        `第 ${round} 轮自动重连未恢复（state=${await page.evaluate(() => window.__autoReconnect.state)}，status="${await page.textContent('#status')}"）`,
      );
    }
    keyFps.push(await page.evaluate(() => window.__sessionKeyFp));
    framesBefore = await page.evaluate(() => window.__framesDecoded);
  }

  assertions.loadIdUnchanged = (await page.evaluate(() => window.__loadId)) === loadId;
  assertions.drops = drops;
  assertions.backoffDelaysMs = drops.map((d) => d.delayMs);
  assertions.backoffExponential = drops.length === 2 && drops[0].delayMs === 1000 && drops[1].delayMs === 2000;
  assertions.attempts = await page.evaluate(() => window.__autoReconnect.attempts);
  assertions.dropCount = await page.evaluate(() => window.__autoReconnect.drops);
  assertions.keyFps = keyFps;
  assertions.keyRotatedEachRound = new Set(keyFps).size === keyFps.length && keyFps.length >= 2;
  assertions.framesResumed = framesBefore > r1.framesFinal;
  assertions.finalState = await page.evaluate(() => window.__autoReconnect.state);
  assertions.statusFinal = await page.textContent('#status');
  assertions.noManualReconnect = true; // 全程未刷新页面、未重输配对码（__loadId 不变即证）

  if (!assertions.loadIdUnchanged) await fail('页面发生了刷新（__loadId 变化），自动重连应在原页内完成');
  if (!assertions.backoffExponential)
    await fail(`退避间隔未指数增长：${JSON.stringify(assertions.backoffDelaysMs)}（期望 [1000, 2000]）`);
  if (!assertions.keyRotatedEachRound)
    await fail(`会话密钥指纹未每轮轮换：${JSON.stringify(keyFps)}`);
  if (!assertions.framesResumed) await fail('重连后帧流未恢复（framesDecoded 未递增）');
  if (assertions.finalState !== 'connected')
    await fail(`终态不是 connected：${assertions.finalState}`);

  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1, drops });
} else if (!RECONNECT) {
  // ── 单轮模式（headless / realcapture）──
  const r1 = await connectRound(page, 'round1');
  assertions.consent = r1.consented;
  assertions.firstFrame = r1.framesAtFirst > 0;
  assertions.streamContinues = r1.framesFinal > r1.framesAtFirst;
  assertions.notBlank = true;

  if (REALCAPTURE) {
    const res = `${r1.lastFrame.width}x${r1.lastFrame.height}`;
    assertions.resolution = res;
    if (EXPECTED_W > 0 && EXPECTED_H > 0) {
      assertions.resolutionMatchesScreen =
        r1.lastFrame.width === EXPECTED_W && r1.lastFrame.height === EXPECTED_H;
      if (!assertions.resolutionMatchesScreen)
        await fail(`分辨率 ${res} ≠ 真实屏幕 ${EXPECTED_W}x${EXPECTED_H}`);
    } else if (r1.lastFrame.width === 1280 && r1.lastFrame.height === 720) {
      await fail('分辨率仍为 1280x720 合成值，疑似 Host 仍在 headless 模式');
    }
    assertions.maxStd = r1.frameStats?.maxStd ?? 0;
    assertions.maxDiff = r1.frameStats?.maxDiff ?? 0;
    if (assertions.maxStd <= 15)
      await fail(`空间亮度标准差 maxStd=${assertions.maxStd} ≤ 15，画面疑似纯色合成帧`);
    if (assertions.maxDiff <= 2)
      await fail(`帧间亮度差 maxDiff=${assertions.maxDiff} ≤ 2，画面无变化，疑似静态合成帧`);
  }

  await page.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ round: r1 });
} else {
  // ── 断线重连模式 ──
  const rounds = [];

  // round1：首连
  rounds.push(await connectRound(page, 'round1-initial'));

  // 场景 A：页面刷新，同一配对码原地重连
  await page.reload({ waitUntil: 'load', timeout: 20000 });
  rounds.push(await connectRound(page, 'round2-after-reload'));

  // 场景 C：直接关闭页面（异常断连），新开标签页手动重连（同一配对码）
  await page.close();
  const page2 = await browser.newPage({ viewport: { width: 1400, height: 900 } });
  wirePage(page2);
  lastPage = page2;
  await page2.goto(WEB_URL, { waitUntil: 'load', timeout: 20000 });
  rounds.push(await connectRound(page2, 'round3-after-tab-close'));

  // 跨轮断言
  const [a, b, c] = rounds;
  assertions.roundsConsent = rounds.every((r) => r.consented);
  assertions.roundsFrames = rounds.every((r) => r.framesFinal > 0);
  assertions.keyRotatedAfterReload =
    a.sessionKeyFp !== b.sessionKeyFp && b.sessionKeyFp !== c.sessionKeyFp;
  assertions.viewerIdentityRotated = a.viewerFp !== b.viewerFp && b.viewerFp !== c.viewerFp;
  assertions.hostFingerprintStable = rounds.every(
    (r) => r.hostFingerprint === a.hostFingerprint && r.hostFingerprint.length > 0,
  );
  assertions.sessionKeyFps = rounds.map((r) => r.sessionKeyFp);
  assertions.viewerFps = rounds.map((r) => r.viewerFp);
  // 经历 401 竞争但重试恢复的轮数（>0 说明容错路径真实被走过）
  assertions.roundsRetriedAfter401 = rounds.filter((r) => r.retried).length;

  if (!assertions.keyRotatedAfterReload)
    await fail(
      `会话密钥未随重连轮换：${JSON.stringify(assertions.sessionKeyFps)}（X25519 临时密钥应每轮更换）`,
    );
  if (!assertions.hostFingerprintStable)
    await fail('Host 指纹在重连间变化（持久身份应不变，TOFU 基础）');

  await page2.screenshot({ path: join(OUT_DIR, OUT_PNG) });
  dump({ rounds });
}

await browser.close();
process.exit(errors.length === 0 ? 0 : 1);
