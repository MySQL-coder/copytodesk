#!/usr/bin/env node
/**
 * CopyToDesk Phase 2: Zero-Dependency Signaling + TCP Relay + Cross-Platform Web Viewer Server
 *
 * Ports:
 *  - TCP Relay Port (default 19901):
 *      Line-based control handshake before switching socket to raw transparent CTD1 binary pipe:
 *      1) Host Control Channel:
 *         Host -> Relay:  REGISTER <deviceId> <name>\n
 *         Relay -> Host:  OK REGISTERED <deviceId>\n
 *         Relay -> Host:  INCOMING <sessionId>\n   (when a client requests this deviceId)
 *      2) Host Data Channel (opened per incoming session):
 *         Host -> Relay:  ACCEPT <sessionId>\n
 *         Relay -> Host:  OK TUNNEL\n  (then raw bidirectional binary pipe!)
 *      3) Client / Viewer / MCP Channel:
 *         Client -> Relay: CONNECT <deviceId>\n
 *         Relay -> Client: OK TUNNEL\n  (once host accepts, raw bidirectional binary pipe!)
 *
 *  - HTTP Signaling & Web Viewer Port (default 19900):
 *      GET /               -> Zero-GPU HTML5 Canvas2D Cross-Platform Web Viewer
 *      GET /api/devices    -> JSON list of online registered devices
 *      GET /api/health     -> Server health & metrics
 *      POST /api/screenshot -> Quick HTTP proxy screenshot for Web Viewer polling/preview
 *      POST /api/input     -> Quick HTTP proxy mouse/key event for Web Viewer
 */
import net from 'node:net';
import http from 'node:http';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const UNIFIED_UI_PATH = path.resolve(__dirname, '..', '..', 'app', 'ui', 'index.html');
const UNIFIED_JS_PATH = path.resolve(__dirname, '..', '..', 'app', 'ui', 'app.js');

function parseArgs(argv) {
  const cfg = {
    bind: '127.0.0.1',
    httpPort: 19900,
    relayPort: 19901,
  };
  for (let i = 2; i < argv.length; i++) {
    if (argv[i] === '--bind' && argv[i + 1]) cfg.bind = argv[++i];
    else if (argv[i] === '--http-port' && argv[i + 1]) cfg.httpPort = Number(argv[++i]);
    else if (argv[i] === '--relay-port' && argv[i + 1]) cfg.relayPort = Number(argv[++i]);
  }
  return cfg;
}

const cfg = parseArgs(process.argv);

// deviceId -> { deviceId, name, controlSock, registeredAt, remoteAddr }
const hosts = new Map();
// sessionId -> { clientSock, deviceId, timer, onReady }
const pendingSessions = new Map();

let totalTunnels = 0;
let totalBytesRelayed = 0;

function readLine(socket, timeoutMs = 5000) {
  return new Promise((resolve, reject) => {
    let buf = Buffer.alloc(0);
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error('Handshake timeout'));
    }, timeoutMs);

    function onData(chunk) {
      buf = Buffer.concat([buf, chunk]);
      const nl = buf.indexOf(0x0a);
      if (nl >= 0) {
        const line = buf.subarray(0, nl).toString('utf8').replace(/\r$/, '');
        const rest = buf.subarray(nl + 1);
        cleanup();
        resolve({ line, rest });
      } else if (buf.length > 4096) {
        cleanup();
        reject(new Error('Handshake line too long'));
      }
    }
    function onErr(e) { cleanup(); reject(e); }
    function onClose() { cleanup(); reject(new Error('Socket closed during handshake')); }
    function cleanup() {
      clearTimeout(timer);
      socket.removeListener('data', onData);
      socket.removeListener('error', onErr);
      socket.removeListener('close', onClose);
    }

    socket.on('data', onData);
    socket.on('error', onErr);
    socket.on('close', onClose);
  });
}

function bridgeSockets(sockA, restA, sockB, restB) {
  totalTunnels++;
  sockA.setNoDelay(true);
  sockB.setNoDelay(true);

  if (restA && restA.length > 0) sockB.write(restA);
  if (restB && restB.length > 0) sockA.write(restB);

  sockA.on('data', (chunk) => {
    totalBytesRelayed += chunk.length;
    if (!sockB.destroyed) sockB.write(chunk);
  });
  sockB.on('data', (chunk) => {
    totalBytesRelayed += chunk.length;
    if (!sockA.destroyed) sockA.write(chunk);
  });

  const closeBoth = () => {
    if (!sockA.destroyed) sockA.destroy();
    if (!sockB.destroyed) sockB.destroy();
  };
  sockA.on('close', closeBoth);
  sockB.on('close', closeBoth);
  sockA.on('error', closeBoth);
  sockB.on('error', closeBoth);
}

const relayServer = net.createServer(async (sock) => {
  sock.setNoDelay(true);
  try {
    const { line, rest } = await readLine(sock, 8000);
    const parts = line.trim().split(/\s+/);
    const cmd = (parts[0] || '').toUpperCase();

    if (cmd === 'REGISTER' && parts[1]) {
      const deviceId = parts[1];
      const name = parts.slice(2).join(' ') || 'Host';
      const old = hosts.get(deviceId);
      if (old && !old.controlSock.destroyed) old.controlSock.destroy();

      const info = {
        deviceId,
        name,
        controlSock: sock,
        registeredAt: new Date().toISOString(),
        remoteAddr: sock.remoteAddress || '',
      };
      hosts.set(deviceId, info);
      sock.write(`OK REGISTERED ${deviceId}\n`);
      console.log(`[Relay] Host registered: deviceId=${deviceId} name=${name}`);

      // Keep control channel alive (ignore heartbeats)
      sock.on('data', () => {});
      sock.on('close', () => {
        if (hosts.get(deviceId) === info) {
          hosts.delete(deviceId);
          console.log(`[Relay] Host offline: deviceId=${deviceId}`);
        }
      });
      sock.on('error', () => {});
      return;
    }

    if (cmd === 'CONNECT' && parts[1]) {
      const deviceId = parts[1];
      const hostInfo = hosts.get(deviceId);
      if (!hostInfo || hostInfo.controlSock.destroyed) {
        sock.write('ERR OFFLINE\n');
        sock.end();
        return;
      }

      const sessionId = crypto.randomBytes(6).toString('hex');
      const timer = setTimeout(() => {
        if (pendingSessions.delete(sessionId)) {
          if (!sock.destroyed) {
            sock.write('ERR TIMEOUT\n');
            sock.end();
          }
        }
      }, 8000);

      pendingSessions.set(sessionId, { clientSock: sock, clientRest: rest, deviceId, timer });
      hostInfo.controlSock.write(`INCOMING ${sessionId}\n`);
      sock.on('close', () => {
        const p = pendingSessions.get(sessionId);
        if (p) {
          clearTimeout(p.timer);
          pendingSessions.delete(sessionId);
        }
      });
      sock.on('error', () => {});
      return;
    }

    if (cmd === 'ACCEPT' && parts[1]) {
      const sessionId = parts[1];
      const pending = pendingSessions.get(sessionId);
      if (!pending) {
        sock.write('ERR NO_SESSION\n');
        sock.end();
        return;
      }
      clearTimeout(pending.timer);
      pendingSessions.delete(sessionId);

      // Signal both sides tunnel is open, then switch to raw CTD1 binary relay!
      sock.write('OK TUNNEL\n');
      pending.clientSock.write('OK TUNNEL\n');
      console.log(`[Relay] Tunnel established: session=${sessionId} device=${pending.deviceId}`);
      bridgeSockets(pending.clientSock, pending.clientRest, sock, rest);
      return;
    }

    sock.write('ERR UNKNOWN_CMD\n');
    sock.end();
  } catch {
    if (!sock.destroyed) sock.destroy();
  }
});

// Helper: One-shot CTD1 client over local relay for the zero-GPU Cross-Platform Web Viewer
function openRelayTunnel(deviceId, token) {
  const MAGIC = 0x31445443;
  return new Promise((resolve, reject) => {
    const s = net.createConnection({ host: cfg.bind === '0.0.0.0' ? '127.0.0.1' : cfg.bind, port: cfg.relayPort }, async () => {
      try {
        s.setNoDelay(true);
        s.write(`CONNECT ${deviceId}\n`);
        const { line, rest } = await readLine(s, 5000);
        if (!line.startsWith('OK TUNNEL')) {
          s.destroy();
          return reject(new Error(line));
        }

        let rx = rest || Buffer.alloc(0);
        let seq = 0;
        const waiters = new Map();

        function sendFrame(type, ch, payload = Buffer.alloc(0)) {
          const hdr = Buffer.alloc(16);
          hdr.writeUInt32LE(MAGIC, 0);
          hdr.writeUInt8(1, 4);
          hdr.writeUInt8(type, 5);
          hdr.writeUInt8(0, 6);
          hdr.writeUInt8(ch, 7);
          hdr.writeUInt32LE(payload.length, 8);
          hdr.writeUInt32LE(++seq, 12);
          s.write(Buffer.concat([hdr, payload]));
        }

        function waitMsg(type, ms = 5000) {
          return new Promise((res, rej) => {
            const t = setTimeout(() => rej(new Error('Timeout')), ms);
            waiters.set(type, (p) => { clearTimeout(t); res(p); });
          });
        }

        function drain() {
          while (rx.length >= 16) {
            if (rx.readUInt32LE(0) !== MAGIC) { s.destroy(); return; }
            const t = rx.readUInt8(5);
            const len = rx.readUInt32LE(8);
            if (rx.length < 16 + len) return;
            const p = Buffer.from(rx.subarray(16, 16 + len));
            rx = rx.subarray(16 + len);
            const cb = waiters.get(t);
            if (cb) { waiters.delete(t); cb(p); }
          }
        }

        s.on('data', (c) => { rx = Buffer.concat([rx, c]); drain(); });
        const helloWait = waitMsg(0x02, 5000);
        sendFrame(0x01, 0, Buffer.from(JSON.stringify({ clientName: 'WebViewer-Canvas2D', token }), 'utf8'));
        const ackBuf = await helloWait;
        const ack = JSON.parse(ackBuf.toString('utf8'));
        resolve({ sock: s, ack, sendFrame, waitMsg, close: () => s.destroy() });
      } catch (e) {
        s.destroy();
        reject(e);
      }
    });
    s.on('error', reject);
  });
}

const WEB_VIEWER_HTML = `<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>CopyToDesk Cross-Platform Web Viewer (Zero-GPU Canvas2D)</title>
<style>
  body { margin:0; background:#11161f; color:#e6edf3; font-family:system-ui,sans-serif; display:flex; flex-direction:column; height:100vh; }
  header { padding:10px 16px; background:#18202c; display:flex; gap:10px; align-items:center; flex-wrap:wrap; border-bottom:1px solid #2b3545; }
  input, button, select { background:#0d1117; color:#e6edf3; border:1px solid #30363d; border-radius:6px; padding:6px 10px; }
  button { background:#2563eb; border-color:#2563eb; cursor:pointer; }
  main { flex:1; display:flex; justify-content:center; align-items:center; overflow:auto; padding:12px; }
  canvas { max-width:100%; max-height:100%; border:1px solid #2b3545; background:#000; cursor:crosshair; }
  .badge { font-size:12px; color:#8b949e; }
</style>
</head>
<body>
<header>
  <strong>CopyToDesk Web Viewer</strong>
  <select id="devSelect"></select>
  <input id="token" placeholder="Session Token" value="" />
  <button id="connectBtn">连接</button>
  <button id="refreshBtn">刷新设备</button>
  <span id="status" class="badge">就绪（支持任意无显卡终端浏览器渲染）</span>
</header>
<main><canvas id="screen" width="1280" height="720"></canvas></main>
<script>
const sel = document.getElementById('devSelect');
const tok = document.getElementById('token');
const st = document.getElementById('status');
const cvs = document.getElementById('screen');
const ctx = cvs.getContext('2d');
let timer = null, origW = 1280, origH = 720;

async function loadDevices() {
  const res = await fetch('/api/devices');
  const data = await res.json();
  sel.innerHTML = '';
  for (const d of data.devices) {
    const o = document.createElement('option');
    o.value = d.deviceId;
    o.textContent = d.deviceId + ' (' + d.name + ')';
    sel.appendChild(o);
  }
  st.textContent = '在线设备: ' + data.devices.length;
}
async function pollFrame() {
  if (!sel.value) return;
  try {
    const r = await fetch('/api/screenshot', {
      method: 'POST', headers: {'content-type':'application/json'},
      body: JSON.stringify({ deviceId: sel.value, token: tok.value, maxWidth: 1280, quality: 70 })
    });
    if (!r.ok) { st.textContent = '错误: ' + await r.text(); return; }
    origW = Number(r.headers.get('x-orig-w') || 1280);
    origH = Number(r.headers.get('x-orig-h') || 720);
    const blob = await r.blob();
    const img = await createImageBitmap(blob);
    if (cvs.width !== img.width || cvs.height !== img.height) { cvs.width = img.width; cvs.height = img.height; }
    ctx.drawImage(img, 0, 0);
    st.textContent = '已连接 ' + sel.value + ' | ' + origW + 'x' + origH + ' | Canvas2D 零显卡模式';
  } catch (e) { st.textContent = '断开: ' + e.message; }
}
cvs.addEventListener('click', async (ev) => {
  if (!sel.value) return;
  const r = cvs.getBoundingClientRect();
  const x = Math.round(((ev.clientX - r.left) / r.width) * origW);
  const y = Math.round(((ev.clientY - r.top) / r.height) * origH);
  await fetch('/api/input', {
    method: 'POST', headers: {'content-type':'application/json'},
    body: JSON.stringify({ deviceId: sel.value, token: tok.value, x, y, origW, origH })
  });
});
document.getElementById('refreshBtn').onclick = loadDevices;
document.getElementById('connectBtn').onclick = () => {
  if (timer) clearInterval(timer);
  pollFrame();
  timer = setInterval(pollFrame, 300);
};
loadDevices();
</script>
</body>
</html>`;

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = '';
    req.on('data', (c) => { data += c; if (data.length > 1024 * 1024) reject(new Error('Body too large')); });
    req.on('end', () => resolve(data));
    req.on('error', reject);
  });
}

const httpServer = http.createServer(async (req, res) => {
  try {
    if (req.method === 'GET' && req.url === '/') {
      const html = fs.existsSync(UNIFIED_UI_PATH) ? fs.readFileSync(UNIFIED_UI_PATH, 'utf8') : WEB_VIEWER_HTML;
      res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      res.end(html);
      return;
    }
    if (req.method === 'GET' && req.url === '/app.js') {
      const js = fs.existsSync(UNIFIED_JS_PATH) ? fs.readFileSync(UNIFIED_JS_PATH, 'utf8') : '';
      res.writeHead(200, { 'content-type': 'application/javascript; charset=utf-8' });
      res.end(js);
      return;
    }
    if (req.method === 'GET' && req.url === '/api/devices') {
      const devices = Array.from(hosts.values()).map((h) => ({
        deviceId: h.deviceId,
        name: h.name,
        registeredAt: h.registeredAt,
      }));
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ devices }));
      return;
    }
    if (req.method === 'GET' && req.url === '/api/health') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ status: 'ok', onlineHosts: hosts.size, totalTunnels, totalBytesRelayed }));
      return;
    }
    if (req.method === 'POST' && req.url === '/api/screenshot') {
      const body = JSON.parse(await readBody(req));
      const t = await openRelayTunnel(body.deviceId, body.token || '');
      try {
        const p = Buffer.alloc(4);
        p.writeUInt16LE(Number(body.maxWidth || 1280), 0);
        p.writeUInt8(Number(body.quality || 70), 2);
        p.writeUInt8(0, 3);
        const w = t.waitMsg(0x41, 5000);
        t.sendFrame(0x40, 0, p);
        const resp = await w;
        const origW = resp.readUInt16LE(0);
        const origH = resp.readUInt16LE(2);
        const len = resp.readUInt32LE(9);
        const img = resp.subarray(13, 13 + len);
        res.writeHead(200, {
          'content-type': 'image/jpeg',
          'x-orig-w': String(origW),
          'x-orig-h': String(origH),
        });
        res.end(img);
      } finally {
        t.close();
      }
      return;
    }
    if (req.method === 'POST' && req.url === '/api/input') {
      const body = JSON.parse(await readBody(req));
      const t = await openRelayTunnel(body.deviceId, body.token || '');
      try {
        const nx = Math.max(0, Math.min(65535, Math.round((body.x * 65535) / Math.max(1, body.origW || t.ack.width))));
        const ny = Math.max(0, Math.min(65535, Math.round((body.y * 65535) / Math.max(1, body.origH || t.ack.height))));
        for (const act of [1, 2]) {
          const b = Buffer.alloc(8);
          b.writeUInt8(act, 0); b.writeUInt8(1, 1); b.writeUInt16LE(nx, 2); b.writeUInt16LE(ny, 4); b.writeInt16LE(0, 6);
          t.sendFrame(0x30, 3, b);
        }
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end('{"ok":true}');
      } finally {
        t.close();
      }
      return;
    }
    if (req.method === 'POST' && req.url === '/api/clipboard') {
      const body = JSON.parse(await readBody(req));
      const t = await openRelayTunnel(body.deviceId, body.token || '');
      try {
        const w = t.waitMsg(0x51, 5000);
        if (body.action === 'set') t.sendFrame(0x51, 0, Buffer.from(String(body.text || ''), 'utf8'));
        else t.sendFrame(0x50, 0);
        const resp = await w;
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ ok: true, text: resp.toString('utf8') }));
      } finally { t.close(); }
      return;
    }
    if (req.method === 'POST' && req.url === '/api/files') {
      const body = JSON.parse(await readBody(req));
      const t = await openRelayTunnel(body.deviceId, body.token || '');
      try {
        if (body.action === 'upload') {
          const rel = String(body.remotePath || '');
          const data = Buffer.from(String(body.contentBase64 || ''), 'base64');
          const sha = crypto.createHash('sha256').update(data).digest('hex');
          const pb = Buffer.from(rel, 'utf8');
          const hdr = Buffer.alloc(2 + pb.length + 64);
          hdr.writeUInt16LE(pb.length, 0);
          pb.copy(hdr, 2);
          Buffer.from(sha, 'ascii').copy(hdr, 2 + pb.length);
          const w = t.waitMsg(0x63, 8000);
          t.sendFrame(0x62, 5, Buffer.concat([hdr, data]));
          const resp = await w;
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end(resp.toString('utf8'));
        } else {
          const w = t.waitMsg(0x61, 5000);
          t.sendFrame(0x60, 5, Buffer.from(String(body.remotePath || ''), 'utf8'));
          const resp = await w;
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end(resp.toString('utf8'));
        }
      } finally { t.close(); }
      return;
    }
    if (req.method === 'POST' && req.url === '/api/exec') {
      const body = JSON.parse(await readBody(req));
      const t = await openRelayTunnel(body.deviceId, body.token || '');
      try {
        const w = t.waitMsg(0x71, 10000);
        t.sendFrame(0x70, 0, Buffer.from(JSON.stringify({ command: String(body.command || ''), timeoutMs: 5000 }), 'utf8'));
        const resp = await w;
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(resp.toString('utf8'));
      } finally { t.close(); }
      return;
    }
    res.writeHead(404);
    res.end('Not Found');
  } catch (e) {
    res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8' });
    res.end(e.message);
  }
});

relayServer.listen(cfg.relayPort, cfg.bind, () => {
  console.log(`[SignalingRelay] TCP Relay listening on ${cfg.bind}:${cfg.relayPort}`);
});
httpServer.listen(cfg.httpPort, cfg.bind, () => {
  console.log(`[SignalingRelay] HTTP Signaling & Web Viewer listening on http://${cfg.bind}:${cfg.httpPort}`);
});
