// 401 竞争探针：跨心跳边界（T+30/T+60s）密集发起 WS 握手 + 高频监视 token 库文件。
// 回答：401 时文件内容到底是空/半行/缺失，还是文件完好但服务端仍拒（指向 mtime 等其他路径）。
// 用法：SIGNALING_PORT=.. PAIRING=<32hex>:<64hex> TOKEN_DB=<path> DURATION_S=75 node probe-token-race.mjs
import http from 'node:http';
import fs from 'node:fs';
import crypto from 'node:crypto';

const PORT = Number(process.env.SIGNALING_PORT || 18081);
const PAIRING = process.env.PAIRING_CODE || '';
const TOKEN_DB = process.env.SIGNALING_TOKEN_DB;
const DURATION_S = Number(process.env.DURATION_S || 75);
const [session, token] = PAIRING.split(':');

const t0 = Date.now();
const el = () => ((Date.now() - t0) / 1000).toFixed(1);

let handshakes = 0, ok = 0, fail401 = 0, otherErr = 0;
const failEvents = [];

function probeOnce() {
  handshakes++;
  const req = http.get({
    host: '127.0.0.1',
    port: PORT,
    path: `/${session}?token=${token}`,
    headers: {
      Connection: 'Upgrade',
      Upgrade: 'websocket',
      'Sec-WebSocket-Version': '13',
      'Sec-WebSocket-Key': crypto.randomBytes(16).toString('base64'),
    },
    timeout: 3000,
  });
  req.on('upgrade', (res, socket) => { ok++; socket.destroy(); });
  req.on('response', (res) => {
    if (res.statusCode === 401) {
      fail401++;
      // 401 瞬间立即读文件取证
      let snap = '<unreadable>';
      try { snap = JSON.stringify(fs.readFileSync(TOKEN_DB, 'utf8')); } catch (e) { snap = `<${e.code}>`; }
      let mt = '<n/a>';
      try { const m = fs.statSync(TOKEN_DB).mtimeMs; mt = `mtime=${m.toFixed(0)} now=${Date.now()} skew=${(m - Date.now()).toFixed(0)}ms`; } catch {}
      failEvents.push(`t=${el()}s 401 snap=${snap} ${mt}`);
      res.resume();
    } else { otherErr++; failEvents.push(`t=${el()}s HTTP ${res.statusCode}`); res.resume(); }
  });
  req.on('error', () => { otherErr++; });
  req.on('timeout', () => { req.destroy(); otherErr++; });
}

// 文件监视：50ms 间隔，记录任何空读/半行/缺失/未来 mtime
let fileReads = 0, fileAnomalies = 0;
const anomalies = [];
const EXPECTED = new RegExp(`^${session}\\t${token}\\n$`);
const fileTimer = setInterval(() => {
  fileReads++;
  try {
    const c = fs.readFileSync(TOKEN_DB, 'utf8');
    if (!EXPECTED.test(c)) {
      fileAnomalies++;
      if (anomalies.length < 10) anomalies.push(`t=${el()}s content=${JSON.stringify(c)}`);
    }
    const skew = fs.statSync(TOKEN_DB).mtimeMs - Date.now();
    if (skew > 5) {
      fileAnomalies++;
      if (anomalies.length < 10) anomalies.push(`t=${el()}s FUTURE MTIME skew=${skew.toFixed(0)}ms`);
    }
  } catch (e) {
    fileAnomalies++;
    if (anomalies.length < 10) anomalies.push(`t=${el()}s read ${e.code}`);
  }
}, 50);

const hsTimer = setInterval(probeOnce, 250);
setTimeout(() => {
  clearInterval(hsTimer);
  clearInterval(fileTimer);
  setTimeout(() => {
    console.log(JSON.stringify({
      durationS: DURATION_S,
      handshakes, ok, fail401, otherErr,
      fileReads, fileAnomalies,
      failEvents: failEvents.slice(0, 12),
      anomalies,
    }, null, 1));
    process.exit(0);
  }, 1500);
}, DURATION_S * 1000);
