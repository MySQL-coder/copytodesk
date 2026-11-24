#!/usr/bin/env node
/**
 * CopyToDesk Phase 2 MCP Server (Zero-Dependency Node.js 24)
 * Speaks JSON-RPC 2.0 (MCP over stdio).
 * Supports direct LAN/loopback connection OR NAT-traversing Relay tunnel (--relay-host/--relay-port/--device-id).
 * Exposes 13 tools: GUI Computer Use (7) + Clipboard (2) + File Transfer w/ SHA-256 (3) + Guarded Exec (1).
 */
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import readline from 'node:readline';

const MAGIC = 0x31445443; // "CTD1"
const MSG = {
  HELLO: 0x01,
  HELLO_ACK: 0x02,
  PING: 0x03,
  PONG: 0x04,
  MOUSE: 0x30,
  KEY: 0x31,
  SCREENSHOT_REQ: 0x40,
  SCREENSHOT_RESP: 0x41,
  CLIPBOARD_GET: 0x50,
  CLIPBOARD_DATA: 0x51,
  FILE_LIST_REQ: 0x60,
  FILE_LIST_RESP: 0x61,
  FILE_UPLOAD_REQ: 0x62,
  FILE_UPLOAD_RESP: 0x63,
  FILE_DOWNLOAD_REQ: 0x64,
  FILE_DOWNLOAD_RESP: 0x65,
  EXEC_REQ: 0x70,
  EXEC_RESP: 0x71,
};
const CH = { CONTROL: 0, INPUT: 3, FILE: 5 };

function parseArgs(argv) {
  const cfg = {
    host: process.env.COPYTODESK_HOST || '127.0.0.1',
    port: Number(process.env.COPYTODESK_PORT || 19800),
    relayHost: process.env.COPYTODESK_RELAY_HOST || '',
    relayPort: Number(process.env.COPYTODESK_RELAY_PORT || 19901),
    deviceId: process.env.COPYTODESK_DEVICE_ID || '',
    token: process.env.COPYTODESK_TOKEN || '',
    auditPath: process.env.COPYTODESK_AUDIT || path.resolve(process.cwd(), 'logs', 'mcp-audit.jsonl'),
  };
  for (let i = 2; i < argv.length; i++) {
    if (argv[i] === '--host' && argv[i + 1]) cfg.host = argv[++i];
    else if (argv[i] === '--port' && argv[i + 1]) cfg.port = Number(argv[++i]);
    else if (argv[i] === '--relay-host' && argv[i + 1]) cfg.relayHost = argv[++i];
    else if (argv[i] === '--relay-port' && argv[i + 1]) cfg.relayPort = Number(argv[++i]);
    else if (argv[i] === '--device-id' && argv[i + 1]) cfg.deviceId = argv[++i];
    else if (argv[i] === '--token' && argv[i + 1]) cfg.token = argv[++i];
    else if (argv[i] === '--audit' && argv[i + 1]) cfg.auditPath = argv[++i];
  }
  return cfg;
}

const cfg = parseArgs(process.argv);

function appendAudit(entry) {
  try {
    fs.mkdirSync(path.dirname(cfg.auditPath), { recursive: true });
    fs.appendFileSync(cfg.auditPath, JSON.stringify({ ts: new Date().toISOString(), ...entry }) + '\n', 'utf8');
  } catch {
    // non-fatal
  }
}

class HostClient {
  constructor() {
    this.sock = null;
    this.connecting = null;
    this.seq = 0;
    this.rxBuf = Buffer.alloc(0);
    this.ack = null;
    this.waiters = new Map();
  }

  async ensureConnected() {
    if (this.sock && !this.sock.destroyed && this.ack) return this.ack;
    if (this.connecting) return this.connecting;

    this.connecting = new Promise((resolve, reject) => {
      const targetHost = cfg.relayHost ? cfg.relayHost : cfg.host;
      const targetPort = cfg.relayHost ? cfg.relayPort : cfg.port;
      let tunnelReady = !cfg.relayHost;
      let handshakeBuf = Buffer.alloc(0);

      const s = net.createConnection({ host: targetHost, port: targetPort }, () => {
        s.setNoDelay(true);
        this.sock = s;
        this.rxBuf = Buffer.alloc(0);
        if (cfg.relayHost) {
          s.write(`CONNECT ${cfg.deviceId}\n`);
        } else {
          this.sendFrame(MSG.HELLO, CH.CONTROL, Buffer.from(JSON.stringify({ clientName: 'CopyToDesk-MCP', token: cfg.token }), 'utf8'));
        }
      });

      s.on('error', (err) => {
        this.connecting = null;
        reject(err);
      });
      s.on('close', () => {
        this.sock = null;
        this.ack = null;
        this.connecting = null;
      });
      s.on('data', (chunk) => {
        if (!tunnelReady) {
          handshakeBuf = Buffer.concat([handshakeBuf, chunk]);
          const nl = handshakeBuf.indexOf(0x0a);
          if (nl < 0) return;
          const line = handshakeBuf.subarray(0, nl).toString('utf8').replace(/\r$/, '');
          const rest = handshakeBuf.subarray(nl + 1);
          if (!line.startsWith('OK TUNNEL')) {
            this.connecting = null;
            s.destroy();
            reject(new Error(`Relay rejected: ${line}`));
            return;
          }
          tunnelReady = true;
          this.sendFrame(MSG.HELLO, CH.CONTROL, Buffer.from(JSON.stringify({ clientName: 'CopyToDesk-MCP', token: cfg.token }), 'utf8'));
          if (rest.length > 0) {
            this.rxBuf = Buffer.concat([this.rxBuf, rest]);
            this.drainFrames();
          }
          return;
        }
        this.rxBuf = Buffer.concat([this.rxBuf, chunk]);
        this.drainFrames();
      });

      this.waitFor(MSG.HELLO_ACK, 8000).then((payload) => {
        this.ack = JSON.parse(payload.toString('utf8'));
        this.connecting = null;
        resolve(this.ack);
      }).catch((err) => {
        this.connecting = null;
        reject(err);
      });
    });

    return this.connecting;
  }

  sendFrame(type, ch, payload = Buffer.alloc(0), flags = 0) {
    const hdr = Buffer.alloc(16);
    hdr.writeUInt32LE(MAGIC, 0);
    hdr.writeUInt8(1, 4);
    hdr.writeUInt8(type, 5);
    hdr.writeUInt8(flags, 6);
    hdr.writeUInt8(ch, 7);
    hdr.writeUInt32LE(payload.length, 8);
    hdr.writeUInt32LE(++this.seq, 12);
    this.sock.write(Buffer.concat([hdr, payload]));
  }

  waitFor(msgType, timeoutMs = 8000) {
    return new Promise((resolve, reject) => {
      const list = this.waiters.get(msgType) || [];
      const timer = setTimeout(() => {
        const cur = this.waiters.get(msgType) || [];
        this.waiters.set(msgType, cur.filter((x) => x.resolve !== resolve));
        reject(new Error(`Timeout waiting for msg 0x${msgType.toString(16)}`));
      }, timeoutMs);
      list.push({ resolve: (p) => { clearTimeout(timer); resolve(p); } });
      this.waiters.set(msgType, list);
    });
  }

  drainFrames() {
    while (this.rxBuf.length >= 16) {
      const magic = this.rxBuf.readUInt32LE(0);
      if (magic !== MAGIC) {
        if (this.sock) this.sock.destroy();
        return;
      }
      const type = this.rxBuf.readUInt8(5);
      const len = this.rxBuf.readUInt32LE(8);
      if (this.rxBuf.length < 16 + len) return;
      const payload = this.rxBuf.subarray(16, 16 + len);
      this.rxBuf = this.rxBuf.subarray(16 + len);
      const list = this.waiters.get(type);
      if (list && list.length > 0) {
        const w = list.shift();
        w.resolve(Buffer.from(payload));
      }
    }
  }

  normCoord(x, y) {
    const w = Math.max(1, this.ack ? this.ack.width : 1280);
    const h = Math.max(1, this.ack ? this.ack.height : 720);
    return [
      Math.max(0, Math.min(65535, Math.round((x * 65535) / w))),
      Math.max(0, Math.min(65535, Math.round((y * 65535) / h))),
    ];
  }

  sendMouse(action, button, x, y, wheelDelta = 0) {
    const [nx, ny] = this.normCoord(x, y);
    const b = Buffer.alloc(8);
    b.writeUInt8(action, 0);
    b.writeUInt8(button, 1);
    b.writeUInt16LE(nx, 2);
    b.writeUInt16LE(ny, 4);
    b.writeInt16LE(wheelDelta, 6);
    this.sendFrame(MSG.MOUSE, CH.INPUT, b);
  }

  sendKey(action, vk = 0, unicodeChar = 0) {
    const b = Buffer.alloc(11);
    b.writeUInt8(action, 0);
    b.writeUInt16LE(vk, 1);
    b.writeUInt16LE(0, 3);
    b.writeUInt16LE(0, 5);
    b.writeUInt32LE(unicodeChar, 7);
    this.sendFrame(MSG.KEY, CH.INPUT, b);
  }
}

const VK_MAP = {
  enter: 0x0d, return: 0x0d, tab: 0x09, esc: 0x1b, escape: 0x1b, space: 0x20,
  backspace: 0x08, delete: 0x2e, del: 0x2e, up: 0x26, down: 0x28, left: 0x25, right: 0x27,
  ctrl: 0x11, control: 0x11, alt: 0x12, shift: 0x10, win: 0x5b, meta: 0x5b,
  home: 0x24, end: 0x23, pageup: 0x21, pagedown: 0x22,
};

function keyNameToVk(name) {
  const k = String(name || '').trim().toLowerCase();
  if (VK_MAP[k]) return VK_MAP[k];
  if (k.length === 1) {
    const code = k.toUpperCase().charCodeAt(0);
    if ((code >= 0x30 && code <= 0x39) || (code >= 0x41 && code <= 0x5a)) return code;
  }
  if (/^f([1-9]|1[0-2])$/.test(k)) return 0x6f + Number(k.slice(1));
  return 0;
}

const TOOLS = [
  {
    name: 'remote_screen_info',
    description: 'Get remote desktop resolution, capture/input backend, exec policy, and GPU codec probe info.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'remote_screenshot',
    description: 'Capture current remote desktop frame as JPEG or PNG image.',
    inputSchema: {
      type: 'object',
      properties: {
        maxWidth: { type: 'integer', default: 1280 },
        quality: { type: 'integer', default: 75 },
        format: { type: 'string', enum: ['jpeg', 'png'], default: 'jpeg' },
      },
    },
  },
  {
    name: 'remote_click',
    description: 'Move mouse to (x, y) in remote desktop pixels and click.',
    inputSchema: {
      type: 'object',
      required: ['x', 'y'],
      properties: {
        x: { type: 'integer' },
        y: { type: 'integer' },
        button: { type: 'string', enum: ['left', 'right', 'middle'], default: 'left' },
        clicks: { type: 'integer', default: 1 },
      },
    },
  },
  {
    name: 'remote_move_drag',
    description: 'Move cursor or drag from (fromX, fromY) to (toX, toY).',
    inputSchema: {
      type: 'object',
      required: ['toX', 'toY'],
      properties: {
        fromX: { type: 'integer' },
        fromY: { type: 'integer' },
        toX: { type: 'integer' },
        toY: { type: 'integer' },
        drag: { type: 'boolean', default: false },
      },
    },
  },
  {
    name: 'remote_scroll',
    description: 'Scroll mouse wheel at (x, y). Positive delta scrolls up, negative scrolls down.',
    inputSchema: {
      type: 'object',
      required: ['x', 'y', 'delta'],
      properties: { x: { type: 'integer' }, y: { type: 'integer' }, delta: { type: 'integer' } },
    },
  },
  {
    name: 'remote_type',
    description: 'Type Unicode text into the active remote window.',
    inputSchema: { type: 'object', required: ['text'], properties: { text: { type: 'string' } } },
  },
  {
    name: 'remote_hotkey',
    description: 'Press and release a hotkey chord, e.g. ["ctrl", "c"].',
    inputSchema: { type: 'object', required: ['keys'], properties: { keys: { type: 'array', items: { type: 'string' } } } },
  },
  {
    name: 'remote_clipboard_get',
    description: 'Read current UTF-8 text from the remote session clipboard.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'remote_clipboard_set',
    description: 'Write UTF-8 text to the remote session clipboard.',
    inputSchema: { type: 'object', required: ['text'], properties: { text: { type: 'string' } } },
  },
  {
    name: 'remote_list_dir',
    description: 'List files and subdirectories inside the remote Host sandboxed shared-files root.',
    inputSchema: { type: 'object', properties: { remotePath: { type: 'string', default: '' } } },
  },
  {
    name: 'remote_file_upload',
    description: 'Upload a local file (or base64 content) to the remote Host sandboxed root with SHA-256 verification.',
    inputSchema: {
      type: 'object',
      required: ['remotePath'],
      properties: {
        localPath: { type: 'string' },
        contentBase64: { type: 'string' },
        remotePath: { type: 'string' },
      },
    },
  },
  {
    name: 'remote_file_download',
    description: 'Download a file from the remote Host sandboxed root with SHA-256 verification.',
    inputSchema: {
      type: 'object',
      required: ['remotePath'],
      properties: {
        remotePath: { type: 'string' },
        localSavePath: { type: 'string' },
      },
    },
  },
  {
    name: 'remote_exec',
    description: 'Execute a guarded allowlisted command on the remote Host (blocked unless Host enables --allow-exec and command matches allowlist).',
    inputSchema: {
      type: 'object',
      required: ['command'],
      properties: {
        command: { type: 'string' },
        timeoutMs: { type: 'integer', default: 5000 },
      },
    },
  },
];

const client = new HostClient();

async function handleToolCall(name, args = {}) {
  const t0 = Date.now();
  try {
    const ack = await client.ensureConnected();

    if (name === 'remote_screen_info') {
      appendAudit({ tool: name, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: JSON.stringify(ack, null, 2) }] };
    }
    if (name === 'remote_screenshot') {
      const req = Buffer.alloc(4);
      req.writeUInt16LE(Number(args.maxWidth || 1280), 0);
      req.writeUInt8(Number(args.quality || 75), 2);
      req.writeUInt8(args.format === 'png' ? 1 : 0, 3);
      const waiter = client.waitFor(MSG.SCREENSHOT_RESP, 8000);
      client.sendFrame(MSG.SCREENSHOT_REQ, CH.CONTROL, req);
      const resp = await waiter;
      const origW = resp.readUInt16LE(0);
      const origH = resp.readUInt16LE(2);
      const outW = resp.readUInt16LE(4);
      const outH = resp.readUInt16LE(6);
      const fmt = resp.readUInt8(8) === 1 ? 'image/png' : 'image/jpeg';
      const len = resp.readUInt32LE(9);
      const img = resp.subarray(13, 13 + len);
      appendAudit({ tool: name, ok: true, bytes: img.length, ms: Date.now() - t0 });
      return {
        content: [
          { type: 'text', text: JSON.stringify({ origW, origH, outW, outH, mimeType: fmt, bytes: img.length }) },
          { type: 'image', data: img.toString('base64'), mimeType: fmt },
        ],
      };
    }
    if (name === 'remote_click') {
      const btn = args.button === 'right' ? 2 : args.button === 'middle' ? 3 : 1;
      const clicks = Math.max(1, Math.min(3, Number(args.clicks || 1)));
      for (let i = 0; i < clicks; i++) {
        client.sendMouse(1, btn, args.x, args.y);
        client.sendMouse(2, btn, args.x, args.y);
      }
      appendAudit({ tool: name, args, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Clicked (${args.x}, ${args.y}) button=${args.button || 'left'} x${clicks}` }] };
    }
    if (name === 'remote_move_drag') {
      if (args.drag && typeof args.fromX === 'number' && typeof args.fromY === 'number') {
        client.sendMouse(1, 1, args.fromX, args.fromY);
        client.sendMouse(0, 0, args.toX, args.toY);
        client.sendMouse(2, 1, args.toX, args.toY);
      } else {
        client.sendMouse(0, 0, args.toX, args.toY);
      }
      appendAudit({ tool: name, args, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Moved to (${args.toX}, ${args.toY}) drag=${Boolean(args.drag)}` }] };
    }
    if (name === 'remote_scroll') {
      client.sendMouse(3, 0, args.x, args.y, Number(args.delta || 0));
      appendAudit({ tool: name, args, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Scrolled delta=${args.delta} at (${args.x}, ${args.y})` }] };
    }
    if (name === 'remote_type') {
      const text = String(args.text || '');
      for (const ch of text) client.sendKey(2, 0, ch.codePointAt(0));
      appendAudit({ tool: name, len: text.length, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Typed ${text.length} chars` }] };
    }
    if (name === 'remote_hotkey') {
      const vks = (args.keys || []).map(keyNameToVk).filter((v) => v > 0);
      for (const vk of vks) client.sendKey(0, vk, 0);
      for (let i = vks.length - 1; i >= 0; i--) client.sendKey(1, vks[i], 0);
      appendAudit({ tool: name, keys: args.keys, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Sent hotkey [${(args.keys || []).join('+')}]` }] };
    }
    if (name === 'remote_clipboard_get') {
      const w = client.waitFor(MSG.CLIPBOARD_DATA, 5000);
      client.sendFrame(MSG.CLIPBOARD_GET, CH.CONTROL);
      const txt = (await w).toString('utf8');
      appendAudit({ tool: name, len: txt.length, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: txt }] };
    }
    if (name === 'remote_clipboard_set') {
      const txt = String(args.text || '');
      const w = client.waitFor(MSG.CLIPBOARD_DATA, 5000);
      client.sendFrame(MSG.CLIPBOARD_DATA, CH.CONTROL, Buffer.from(txt, 'utf8'));
      await w;
      appendAudit({ tool: name, len: txt.length, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: `Clipboard updated (${txt.length} chars)` }] };
    }
    if (name === 'remote_list_dir') {
      const rel = String(args.remotePath || '');
      const w = client.waitFor(MSG.FILE_LIST_RESP, 5000);
      client.sendFrame(MSG.FILE_LIST_REQ, CH.FILE, Buffer.from(rel, 'utf8'));
      const resp = (await w).toString('utf8');
      appendAudit({ tool: name, remotePath: rel, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: resp }] };
    }
    if (name === 'remote_file_upload') {
      const rel = String(args.remotePath || '');
      const data = args.localPath ? fs.readFileSync(args.localPath) : Buffer.from(String(args.contentBase64 || ''), 'base64');
      const sha = crypto.createHash('sha256').update(data).digest('hex');
      const pathBuf = Buffer.from(rel, 'utf8');
      const hdr = Buffer.alloc(2 + pathBuf.length + 64);
      hdr.writeUInt16LE(pathBuf.length, 0);
      pathBuf.copy(hdr, 2);
      Buffer.from(sha, 'ascii').copy(hdr, 2 + pathBuf.length);
      const w = client.waitFor(MSG.FILE_UPLOAD_RESP, 10000);
      client.sendFrame(MSG.FILE_UPLOAD_REQ, CH.FILE, Buffer.concat([hdr, data]));
      const resp = (await w).toString('utf8');
      appendAudit({ tool: name, remotePath: rel, bytes: data.length, sha256: sha, ok: true, ms: Date.now() - t0 });
      return { content: [{ type: 'text', text: resp }] };
    }
    if (name === 'remote_file_download') {
      const rel = String(args.remotePath || '');
      const w = client.waitFor(MSG.FILE_DOWNLOAD_RESP, 10000);
      client.sendFrame(MSG.FILE_DOWNLOAD_REQ, CH.FILE, Buffer.from(rel, 'utf8'));
      const resp = await w;
      const ok = resp.readUInt8(0) === 1;
      const sha = resp.subarray(1, 65).toString('ascii');
      const errLen = resp.readUInt16LE(65);
      if (!ok) {
        const err = resp.subarray(67, 67 + errLen).toString('utf8');
        appendAudit({ tool: name, remotePath: rel, ok: false, error: err, ms: Date.now() - t0 });
        return { isError: true, content: [{ type: 'text', text: `Download failed: ${err}` }] };
      }
      const fileBytes = resp.subarray(67 + errLen);
      const actualSha = crypto.createHash('sha256').update(fileBytes).digest('hex');
      if (actualSha !== sha) throw new Error(`SHA-256 verification failed: expected ${sha} got ${actualSha}`);
      if (args.localSavePath) {
        fs.mkdirSync(path.dirname(path.resolve(args.localSavePath)), { recursive: true });
        fs.writeFileSync(args.localSavePath, fileBytes);
      }
      appendAudit({ tool: name, remotePath: rel, bytes: fileBytes.length, sha256: actualSha, ok: true, ms: Date.now() - t0 });
      return {
        content: [{
          type: 'text',
          text: JSON.stringify({
            ok: true,
            remotePath: rel,
            bytes: fileBytes.length,
            sha256: actualSha,
            savedTo: args.localSavePath || null,
            contentBase64: args.localSavePath ? undefined : fileBytes.toString('base64'),
          }),
        }],
      };
    }
    if (name === 'remote_exec') {
      const payload = JSON.stringify({ command: String(args.command || ''), timeoutMs: Number(args.timeoutMs || 5000) });
      const w = client.waitFor(MSG.EXEC_RESP, 15000);
      client.sendFrame(MSG.EXEC_REQ, CH.CONTROL, Buffer.from(payload, 'utf8'));
      const respStr = (await w).toString('utf8');
      const parsed = JSON.parse(respStr);
      appendAudit({ tool: name, command: args.command, allowed: parsed.allowed, ok: parsed.ok, ms: Date.now() - t0 });
      return { isError: !parsed.ok, content: [{ type: 'text', text: respStr }] };
    }
    throw new Error(`Unknown tool: ${name}`);
  } catch (err) {
    appendAudit({ tool: name, ok: false, error: err.message, ms: Date.now() - t0 });
    return { isError: true, content: [{ type: 'text', text: `Error: ${err.message}` }] };
  }
}

function writeMsg(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
let pending = 0;
let stdinClosed = false;

function maybeExit() {
  if (stdinClosed && pending === 0) {
    if (client.sock) client.sock.end();
    process.exit(0);
  }
}

rl.on('close', () => {
  stdinClosed = true;
  maybeExit();
});

rl.on('line', async (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let req;
  try {
    req = JSON.parse(trimmed);
  } catch {
    return;
  }
  pending++;
  try {
    const { id, method, params } = req;
    if (method === 'initialize') {
      writeMsg({
        jsonrpc: '2.0',
        id,
        result: {
          protocolVersion: '2024-11-05',
          capabilities: { tools: {} },
          serverInfo: { name: 'copytodesk-mcp', version: '0.2.0' },
        },
      });
    } else if (method === 'notifications/initialized') {
      // no response required
    } else if (method === 'tools/list') {
      writeMsg({ jsonrpc: '2.0', id, result: { tools: TOOLS } });
    } else if (method === 'tools/call') {
      const res = await handleToolCall(params?.name, params?.arguments);
      writeMsg({ jsonrpc: '2.0', id, result: res });
    } else if (id !== undefined) {
      writeMsg({ jsonrpc: '2.0', id, error: { code: -32601, message: `Method not found: ${method}` } });
    }
  } finally {
    pending--;
    maybeExit();
  }
});
