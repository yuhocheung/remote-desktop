// 诊断探针 v2（一次性）：部署态 https://8.138.237.243 全链路通道诊断。
// v1 发现：headless 下 P2P connected 后 consent/媒体均无数据，90s 后 ICE 死亡。
// v2：headed Chrome（排除 headless 网络栈差异），不串行阻塞——
//   连接后并行观察 30s：周期性 __rtcDebug() 快照 + __framesDecoded + 授权状态，
//   然后发真实 DOM 输入（移动+点击），最后汇总 __inputSent / Host 侧表现。
// 环境变量：PAIRING_CODE（必填）、WEB_URL、HOST_ADDR、HEADLESS=1 可切回无头。
import { createRequire } from 'node:module';
import { execSync } from 'node:child_process';

const require = createRequire(new URL('../app/package.json', import.meta.url));
const { chromium } = require('playwright-core');

const PAIRING = process.env.PAIRING_CODE || '';
const WEB_URL = process.env.WEB_URL || 'https://8.138.237.243/';
const HOST_ADDR = process.env.HOST_ADDR || '8.138.237.243';
const HEADLESS = process.env.HEADLESS === '1';

const cursorPos = () =>
  execSync(
    `python -c "import ctypes; from ctypes import wintypes as w; p=w.POINT(); ctypes.windll.user32.GetCursorPos(ctypes.byref(p)); print(f'{p.x},{p.y}')"`,
  )
    .toString()
    .trim();

const out = { ok: false, breakpoints: [], snapshots: [], console: [], errors: [] };
const bp = (s) => out.breakpoints.push(s);

let browser;
let page;
try {
  browser = await chromium.launch({
    channel: 'chrome',
    headless: HEADLESS,
    args: ['--autoplay-policy=no-user-gesture-required', '--no-proxy-server'],
  });
  page = await browser.newPage();
  page.on('console', (m) => out.console.push(`[${m.type()}] ${m.text()}`));
  page.on('pageerror', (e) => out.errors.push(`pageerror: ${e.message}`));

  await page.goto(WEB_URL, { waitUntil: 'load', timeout: 30000 });
  await page.waitForFunction(() => window.__appReady === true, undefined, { timeout: 20000 });
  await page.fill('#host', HOST_ADDR);
  await page.fill('#pairing', PAIRING);
  await page.click('#connect');

  // 观察窗口：30s，每 3s 一次快照
  const t0 = Date.now();
  while (Date.now() - t0 < 30000) {
    await page.waitForTimeout(3000);
    const snap = await page.evaluate(() => ({
      t: Math.round(performance.now() / 1000),
      status: document.getElementById('status')?.textContent,
      rtc: window.__rtcDebug ? window.__rtcDebug() : null,
      frames: window.__framesDecoded,
      allowed: window.__inputAllowed,
      enabled: window.__inputEnabled,
      canvas: (() => {
        const c = document.getElementById('screen');
        return c ? `${c.width}x${c.height}` : null;
      })(),
    }));
    out.snapshots.push(snap);
    if (snap.allowed && snap.frames > 20) break; // 全就绪提前结束观察
  }

  const last = out.snapshots[out.snapshots.length - 1];
  if (!last.allowed) bp('30s 内未获 Input 授权');
  if (last.frames === 0) bp('30s 内未解码任何媒体帧');
  if (last.rtc?.channels && last.rtc.channels.control !== 'open')
    bp(`control 通道未 open：${last.rtc.channels.control}`);
  if (last.rtc?.channels && last.rtc.channels.media !== 'open')
    bp(`media 通道未 open：${last.rtc.channels.media}`);

  // 输入探针（无论前面状态如何都试一次，便于对照）
  if (last.allowed) {
    const geo = await page.evaluate(() => {
      const r = document.getElementById('screen').getBoundingClientRect();
      return { left: r.left, top: r.top, width: r.width, height: r.height };
    });
    out.cursorBefore = cursorPos();
    await page.mouse.move(geo.left + geo.width * 0.5, geo.top + geo.height * 0.5);
    await page.waitForTimeout(600);
    out.cursorAfterMove = cursorPos();
    await page.mouse.down();
    await page.waitForTimeout(80);
    await page.mouse.up();
    await page.waitForTimeout(600);
    out.inputSent = await page.evaluate(() => window.__inputSent.slice(-10));
    const [cw, ch] = [last.rtc ? 0 : 0, 0]; // 占位
    const [fx, fy] = (last.canvas || '0x0').split('x').map(Number);
    const [cx, cy] = out.cursorAfterMove.split(',').map(Number);
    out.cursorExpect = `${Math.round(fx / 2)},${Math.round(fy / 2)}`;
    if (Math.abs(cx - Math.round(fx / 2)) > 4 || Math.abs(cy - Math.round(fy / 2)) > 4)
      bp(`OS 光标未跟随到帧中心（期望 ${out.cursorExpect} 实际 ${cx},${cy}）`);
    const btns = (out.inputSent || []).filter((e) => e.kind === 'MouseButton');
    if (!btns.some((e) => e.pressed) || !btns.some((e) => !e.pressed))
      bp('缺 MouseButton down/up 遥测');
  }

  out.ok = out.breakpoints.length === 0 && out.errors.length === 0;
} catch (e) {
  out.errors.push(`driver: ${e.message}`);
  try {
    out.lastStatus = await page?.textContent('#status');
  } catch {}
} finally {
  console.log(JSON.stringify(out, null, 2));
  await browser?.close();
}
process.exit(out.ok ? 0 : 1);
