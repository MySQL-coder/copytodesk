'use strict';

window.addEventListener('DOMContentLoaded', () => {
  const $ = (id) => document.getElementById(id);
  const STORAGE_KEY_V2 = 'copytodesk_settings_v2';
  const STORAGE_KEY_V1 = 'copytodesk_settings_v1';

  function log(msg) {
    const el = $('eventLog');
    if (!el) return;
    el.textContent = `[${new Date().toLocaleTimeString()}] ${msg}\n` + el.textContent.slice(0, 3500);
  }

  function toast(msg, type = '') {
    const wrap = $('toastWrap');
    if (!wrap) return;
    const d = document.createElement('div');
    d.className = 'toast ' + type;
    d.textContent = msg;
    wrap.appendChild(d);
    setTimeout(() => d.remove(), 3200);
  }

  window.addEventListener('error', (ev) => {
    const m = ev.message || 'Unknown script error';
    if ($('engineText')) $('engineText').textContent = 'JS 错误: ' + m.slice(0, 24);
    log('JS Error: ' + m);
    toast('脚本异常: ' + m, 'err');
  });

  function getTauriInvoke() {
    if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {
      return window.__TAURI__.core.invoke;
    }
    if (window.__TAURI_INTERNALS__ && typeof window.__TAURI_INTERNALS__.invoke === 'function') {
      return window.__TAURI_INTERNALS__.invoke;
    }
    return null;
  }

  function getTauriListen() {
    if (window.__TAURI__ && window.__TAURI__.event && typeof window.__TAURI__.event.listen === 'function') {
      return window.__TAURI__.event.listen;
    }
    return null;
  }

  let invoke = getTauriInvoke();
  let isTauri = Boolean(invoke);
  let connMode = 'relay';
  let connected = false;
  let webLoopToken = 0;
  let origW = 1280;
  let origH = 720;
  let streamW = 1280;
  let streamH = 720;
  let lastMoveSent = 0;

  // Rolling 1s HUD telemetry
  let secFrames = 0;
  let secBytes = 0;
  let curFps = 0;
  let curKbps = 0;
  let lastRttMs = 0;
  let losslessState = false;

  const cvs = $('screen');
  const ctx = cvs.getContext('2d');

  function formatDeviceCode(raw) {
    const s = String(raw || '').replace(/\s+/g, '');
    if (/^\d{6,12}$/.test(s)) return s.replace(/(\d{3})(?=\d)/g, '$1 ');
    return s;
  }

  function updateHud() {
    if (!connected) return;
    const rttPart = lastRttMs > 0 ? ` · ${lastRttMs.toFixed(1)}ms` : '';
    const refPart = losslessState ? ' · 无损✓' : '';
    $('hud').textContent = `${streamW}×${streamH} · ${curFps} FPS · ${curKbps.toFixed(0)} KB/s${rttPart}${refPart}`;
  }

  setInterval(() => {
    if (!connected || !isTauri) return;
    curFps = secFrames;
    curKbps = secBytes / 1024;
    secFrames = 0;
    secBytes = 0;
    updateHud();
  }, 1000);

  function saveSettings() {
    try {
      const data = {
        hostDevId: $('hostDevId').value,
        hostToken: $('hostToken').value,
        hostBind: $('hostBind').value,
        hostRelay: $('hostRelay').value,
        hostAllowInput: $('hostAllowInput').checked,
        hostExec: $('hostExec').checked,
        connMode,
        connDirect: $('connDirect').value,
        connDevId: $('connDevId').value,
        connToken: $('connToken').value,
        viewerQuality: $('viewerQuality').value,
      };
      localStorage.setItem(STORAGE_KEY_V2, JSON.stringify(data));
    } catch {
      // ignore storage errors
    }
  }

  function loadSettings() {
    try {
      let raw = localStorage.getItem(STORAGE_KEY_V2);
      let fromV1 = false;
      if (!raw) {
        raw = localStorage.getItem(STORAGE_KEY_V1);
        fromV1 = Boolean(raw);
      }
      if (!raw) return;
      const d = JSON.parse(raw);
      if (typeof d.hostDevId === 'string' && d.hostDevId.trim()) $('hostDevId').value = d.hostDevId;
      if (typeof d.hostToken === 'string') $('hostToken').value = d.hostToken;
      // Migrate v1 loopback bind to 0.0.0.0:19800 so LAN P2P direct race works automatically!
      if (typeof d.hostBind === 'string' && d.hostBind.trim()) {
        $('hostBind').value = (fromV1 && d.hostBind.trim() === '127.0.0.1:19800') ? '0.0.0.0:19800' : d.hostBind;
      }
      if (typeof d.hostRelay === 'string') $('hostRelay').value = d.hostRelay;
      if (typeof d.hostAllowInput === 'boolean') $('hostAllowInput').checked = d.hostAllowInput;
      if (typeof d.hostExec === 'boolean') $('hostExec').checked = d.hostExec;
      if (typeof d.connDirect === 'string') $('connDirect').value = d.connDirect;
      if (typeof d.connDevId === 'string') $('connDevId').value = d.connDevId;
      if (typeof d.connToken === 'string') $('connToken').value = d.connToken;
      if (typeof d.viewerQuality === 'string') $('viewerQuality').value = d.viewerQuality;
      if (d.connMode === 'direct' || d.connMode === 'relay') connMode = d.connMode;
      if (fromV1) saveSettings();
    } catch {
      // ignore
    }
  }

  function applyConnMode(mode) {
    connMode = mode;
    document.querySelectorAll('.mode-btn').forEach((x) => x.classList.toggle('active', x.dataset.connmode === mode));
    $('fieldDirect').style.display = mode === 'direct' ? 'block' : 'none';
    $('fieldRelay').style.display = mode === 'relay' ? 'block' : 'none';
    $('relayQuickBar').style.display = mode === 'relay' ? 'flex' : 'none';
    $('relayAddrPreview').textContent = $('hostRelay').value.trim() || '38.34.13.214:19901';
  }

  function switchTab(name) {
    document.querySelectorAll('.nav-item').forEach((b) => b.classList.toggle('active', b.dataset.tab === name));
    document.querySelectorAll('.tab-page').forEach((p) => p.classList.toggle('active', p.id === 'tab-' + name));
  }

  document.querySelectorAll('.nav-item').forEach((b) => {
    b.addEventListener('click', () => switchTab(b.dataset.tab));
  });
  if ($('btnBackHome')) $('btnBackHome').addEventListener('click', () => switchTab('home'));
  if ($('btnClearLog')) $('btnClearLog').addEventListener('click', () => { $('eventLog').textContent = ''; });

  document.querySelectorAll('.mode-btn').forEach((b) => {
    b.addEventListener('click', () => {
      applyConnMode(b.dataset.connmode);
      saveSettings();
    });
  });

  function currentQuality() {
    const parts = ($('viewerQuality').value || '1280:68').split(':');
    return { maxWidth: Number(parts[0]) || 0, quality: Number(parts[1]) || 68 };
  }

  for (const id of ['hostDevId', 'hostToken', 'hostBind', 'hostRelay', 'connDirect', 'connDevId', 'connToken']) {
    $(id).addEventListener('input', () => {
      if (id === 'hostDevId') $('displayCode').textContent = formatDeviceCode($('hostDevId').value);
      if (id === 'hostBind') $('hostAddrHint').textContent = `局域网直连监听：${$('hostBind').value.trim() || '0.0.0.0:19800'}`;
      if (id === 'hostRelay') $('relayAddrPreview').textContent = $('hostRelay').value.trim() || '未配置中继';
      saveSettings();
    });
    $(id).addEventListener('change', saveSettings);
  }

  $('viewerQuality').addEventListener('change', () => {
    saveSettings();
    if (connected && isTauri && invoke) {
      const { maxWidth, quality } = currentQuality();
      invoke('remote_set_stream_quality', { maxWidth, quality }).catch(() => {});
      toast('已切换画质并请求关键帧');
    }
  });

  for (const id of ['hostAllowInput', 'hostExec']) {
    $(id).addEventListener('change', saveSettings);
  }

  $('btnCopyCode').addEventListener('click', () => {
    if (navigator.clipboard) navigator.clipboard.writeText($('hostDevId').value.trim()).catch(() => {});
    toast('已复制设备代码', 'ok');
  });
  $('btnCopyPwd').addEventListener('click', () => {
    if (navigator.clipboard) navigator.clipboard.writeText($('hostToken').value).catch(() => {});
    toast('已复制临时密码', 'ok');
  });
  $('btnTogglePwd').addEventListener('click', () => {
    $('hostToken').type = $('hostToken').type === 'password' ? 'text' : 'password';
  });
  $('btnGenPwd').addEventListener('click', () => {
    const chars = 'abcdefghjkmnpqrstuvwxyz23456789';
    let p = '';
    for (let i = 0; i < 6; i++) p += chars[Math.floor(Math.random() * chars.length)];
    $('hostToken').value = p;
    saveSettings();
    toast('已刷新临时密码：' + p);
  });

  async function refreshDevices() {
    if (isTauri) {
      toast('当前中继：' + ($('hostRelay').value.trim() || '未配置'));
      return;
    }
    try {
      const r = await fetch('/api/devices');
      const data = await r.json();
      const wrap = $('devCards');
      wrap.innerHTML = '';
      for (const d of data.devices || []) {
        const card = document.createElement('div');
        card.className = 'dev-card';
        card.innerHTML = `<span class="dot ok"></span><div><div style="font-weight:600;font-size:13.5px">${formatDeviceCode(d.deviceId)}</div><div style="font-size:11.5px;color:var(--muted)">${d.name}</div></div>`;
        card.addEventListener('click', () => {
          $('connDevId').value = d.deviceId;
          saveSettings();
          toast('已选中设备 ' + d.deviceId);
        });
        wrap.appendChild(card);
      }
      if ((data.devices || []).length > 0 && !$('connDevId').value) $('connDevId').value = data.devices[0].deviceId;
      log(`在线设备数: ${(data.devices || []).length}`);
    } catch (e) {
      log('刷新设备失败: ' + e.message);
    }
  }
  $('btnRefreshDevs').addEventListener('click', refreshDevices);

  $('btnStartHost').addEventListener('click', async () => {
    invoke = getTauriInvoke();
    if (!invoke) {
      toast('当前非桌面安装包环境，无法启动本机被控', 'err');
      return;
    }
    saveSettings();
    $('btnStartHost').disabled = true;
    $('btnStartHost').textContent = '启动中...';
    try {
      const bindAddr = $('hostBind').value.trim() || '0.0.0.0:19800';
      const relayVal = $('hostRelay').value.trim();
      const st = await invoke('start_host', {
        config: {
          bindAddr,
          token: $('hostToken').value,
          deviceId: $('hostDevId').value.replace(/\s+/g, '') || '888100200',
          deviceName: 'CopyToDesk-Desktop',
          relayAddr: relayVal ? relayVal : null,
          fps: 25,
          jpegQuality: 72,
          forceSynthetic: false,
          fileRoot: '',
          allowInput: $('hostAllowInput').checked,
          allowExec: $('hostExec').checked,
          execAllowlist: ['echo', 'hostname', 'whoami'],
        },
      });
      $('hostBadge').textContent = '被控运行中';
      $('hostBadge').className = 'badge ok';
      $('captureInfoBadge').textContent = `${st.captureBackend} · ${st.width}×${st.height}`;
      const lanHint = (st.lanAddrs && st.lanAddrs.length > 0) ? `局域网IP: ${st.lanAddrs.join(', ')}` : st.bindAddr;
      $('hostAddrHint').textContent = `${lanHint}（20s 中继保活已启用）`;
      $('hostQuickInfo').textContent = `被控运行中 (${relayVal ? '中继+内网双栈' : st.bindAddr})`;
      $('hostNavDot').className = 'nav-dot on';
      $('btnStartHost').style.display = 'none';
      $('btnStopHost').style.display = 'inline-flex';
      log(`被控启动成功: ${st.captureBackend} ${st.width}x${st.height} | LAN=${(st.lanAddrs || []).join(',')} | 中继=${relayVal || '无'}`);
      toast(`被控已启动 (${st.width}×${st.height})`, 'ok');
    } catch (e) {
      $('hostBadge').textContent = '启动失败';
      $('hostBadge').className = 'badge err';
      log('被控启动失败: ' + e);
      toast('启动失败: ' + e, 'err');
    } finally {
      $('btnStartHost').disabled = false;
      $('btnStartHost').textContent = '启动被控服务';
    }
  });

  $('btnStopHost').addEventListener('click', async () => {
    invoke = getTauriInvoke();
    if (!invoke) return;
    await invoke('stop_host');
    $('hostBadge').textContent = '被控已停止';
    $('hostBadge').className = 'badge';
    $('captureInfoBadge').textContent = '待启动';
    $('hostQuickInfo').textContent = '被控服务：已停止';
    $('hostNavDot').className = 'nav-dot';
    $('btnStopHost').style.display = 'none';
    $('btnStartHost').style.display = 'inline-flex';
    log('被控服务已秒停释放端口');
    toast('已停止本机被控服务');
  });

  // ---- True Incremental Tile Push Renderer (Tauri Native Event Stream) ----
  let renderChain = Promise.resolve();

  function handleTilePushEvent(ev) {
    if (!connected) return;
    secFrames++;
    secBytes += ev.byteLen || 0;
    streamW = ev.screenW;
    streamH = ev.screenH;
    if (ev.isKeyFrame) losslessState = false;
    if (ev.hasLossless) losslessState = true;

    renderChain = renderChain.then(async () => {
      if (!connected) return;
      if (cvs.width !== ev.screenW || cvs.height !== ev.screenH) {
        cvs.width = ev.screenW;
        cvs.height = ev.screenH;
      }
      const decoded = await Promise.all(
        (ev.tiles || []).map(
          (t) =>
            new Promise((resolve) => {
              const img = new Image();
              img.onload = () => resolve({ img, x: t.x, y: t.y, w: t.w, h: t.h });
              img.onerror = () => resolve(null);
              img.src = `data:${t.mime};base64,${t.dataBase64}`;
            })
        )
      );
      for (const item of decoded) {
        if (item) ctx.drawImage(item.img, item.x, item.y, item.w, item.h);
      }
      updateHud();
    }).catch(() => {});
  }

  // Browser-only fallback polling
  async function runWebFallbackLoop(myToken) {
    while (connected && webLoopToken === myToken) {
      const t0 = performance.now();
      const { maxWidth, quality } = currentQuality();
      try {
        const r = await fetch('/api/screenshot', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            deviceId: $('connDevId').value.replace(/\s+/g, ''),
            token: $('connToken').value,
            maxWidth: maxWidth || 1280,
            quality,
          }),
        });
        if (!r.ok) throw new Error(await r.text());
        origW = Number(r.headers.get('x-orig-w') || 1280);
        origH = Number(r.headers.get('x-orig-h') || 720);
        const bmp = await createImageBitmap(await r.blob());
        if (cvs.width !== bmp.width || cvs.height !== bmp.height) {
          cvs.width = bmp.width;
          cvs.height = bmp.height;
        }
        ctx.drawImage(bmp, 0, 0);
        const dt = (performance.now() - t0).toFixed(0);
        $('hud').textContent = `${origW}×${origH} · ${dt}ms · Web Canvas2D`;
      } catch (e) {
        $('hud').textContent = '拉帧失败: ' + e;
      }
      await new Promise((r) => setTimeout(r, 260));
    }
  }

  $('btnConnect').addEventListener('click', async () => {
    webLoopToken++;
    connected = false;
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);
    saveSettings();
    $('btnConnect').disabled = true;
    $('btnConnect').textContent = '连接中...';
    try {
      const { maxWidth, quality } = currentQuality();
      if (isTauri) {
        const useRelay = connMode === 'relay';
        const relayAddr = $('hostRelay').value.trim() || '38.34.13.214:19901';
        const info = await invoke('connect_remote', {
          params: {
            directAddr: useRelay ? null : ($('connDirect').value.trim() || '127.0.0.1:19800'),
            relayAddr: useRelay ? relayAddr : null,
            deviceId: useRelay ? ($('connDevId').value.replace(/\s+/g, '') || '888100200') : null,
            token: $('connToken').value,
            clientName: 'CopyToDesk-Desktop',
            initialMaxWidth: maxWidth,
            initialQuality: quality,
          },
        });
        const ack = info.ack;
        origW = ack.width;
        origH = ack.height;
        streamW = ack.width;
        streamH = ack.height;
        const rb = $('routeBadge');
        rb.style.display = 'inline-block';
        if (info.route === 'lan-p2p') {
          rb.textContent = `⚡ 局域网直连 (${info.endpoint})`;
          rb.style.color = '#86efac';
        } else if (info.route === 'relay') {
          rb.textContent = '🌐 公网中继穿透';
          rb.style.color = '#93c5fd';
        } else {
          rb.textContent = `⚡ 直连 (${info.endpoint})`;
          rb.style.color = '#86efac';
        }
        log(`连接成功 [${info.route} -> ${info.endpoint}]: ${ack.captureBackend} ${ack.width}x${ack.height} (输入: ${ack.inputBackend})`);
        toast(info.route === 'lan-p2p' ? `已自动走局域网 P2P 直连 (${info.endpoint})` : `已连接 (${ack.inputBackend})`, 'ok');
      } else {
        toast('已建立 Web 远程连接', 'ok');
      }
      connected = true;
      secFrames = 0;
      secBytes = 0;
      lastRttMs = 0;
      losslessState = false;
      $('hud').textContent = '等待首帧推流...';
      $('connNavDot').className = 'nav-dot on';
      $('emptyStage').style.display = 'none';
      cvs.style.display = 'block';
      switchTab('viewer');
      cvs.focus();
      if (!isTauri) {
        const myToken = ++webLoopToken;
        runWebFallbackLoop(myToken);
      }
    } catch (e) {
      log('连接失败: ' + e);
      toast('连接失败: ' + e, 'err');
    } finally {
      $('btnConnect').disabled = false;
      $('btnConnect').textContent = '立即连接';
    }
  });

  $('btnDisconnect').addEventListener('click', () => {
    webLoopToken++;
    connected = false;
    $('connNavDot').className = 'nav-dot';
    $('routeBadge').style.display = 'none';
    $('hud').textContent = '已断开';
    cvs.style.display = 'none';
    $('emptyStage').style.display = 'flex';
    switchTab('home');
    toast('已断开远程连接');
    if (isTauri && invoke) {
      invoke('disconnect_remote').catch(() => {});
    }
  });

  $('btnSnap').addEventListener('click', () => {
    if (!connected) return;
    if (isTauri && invoke) {
      const { maxWidth, quality } = currentQuality();
      invoke('remote_set_stream_quality', { maxWidth, quality }).catch(() => {});
      toast('已请求完整关键帧');
    }
  });

  // ---- Zero-Blocking Normalized Mouse Control (0..65535) ----
  function mapNormXY(ev) {
    const r = cvs.getBoundingClientRect();
    const nx = Math.max(0, Math.min(65535, Math.round(((ev.clientX - r.left) / Math.max(1, r.width)) * 65535)));
    const ny = Math.max(0, Math.min(65535, Math.round(((ev.clientY - r.top) / Math.max(1, r.height)) * 65535)));
    return { nx, ny };
  }

  function mapBtn(evButton) {
    if (evButton === 2) return 2;
    if (evButton === 1) return 3;
    return 1;
  }

  function sendMouseNorm(action, button, nx, ny, wheelDelta = 0) {
    if (!connected) return;
    if (isTauri && invoke) {
      invoke('remote_mouse_norm', { action, button, normX: nx, normY: ny, wheelDelta }).catch(() => {});
    } else if (action === 1) {
      const x = Math.round((nx / 65535) * origW);
      const y = Math.round((ny / 65535) * origH);
      fetch('/api/input', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          deviceId: $('connDevId').value.replace(/\s+/g, ''),
          token: $('connToken').value,
          x,
          y,
          origW,
          origH,
        }),
      }).catch(() => {});
    }
  }

  cvs.addEventListener('contextmenu', (ev) => ev.preventDefault());

  cvs.addEventListener('mousedown', (ev) => {
    ev.preventDefault();
    cvs.focus();
    const { nx, ny } = mapNormXY(ev);
    sendMouseNorm(1, mapBtn(ev.button), nx, ny, 0);
  });

  cvs.addEventListener('mouseup', (ev) => {
    ev.preventDefault();
    const { nx, ny } = mapNormXY(ev);
    sendMouseNorm(2, mapBtn(ev.button), nx, ny, 0);
  });

  cvs.addEventListener('mousemove', (ev) => {
    if (!connected || !isTauri) return;
    const now = performance.now();
    if (now - lastMoveSent < 28) return;
    lastMoveSent = now;
    const { nx, ny } = mapNormXY(ev);
    sendMouseNorm(0, 0, nx, ny, 0);
  });

  cvs.addEventListener('wheel', (ev) => {
    if (!connected || !isTauri) return;
    ev.preventDefault();
    const { nx, ny } = mapNormXY(ev);
    const delta = ev.deltaY < 0 ? 120 : -120;
    sendMouseNorm(3, 0, nx, ny, delta);
  }, { passive: false });

  // ---- Direct Keyboard Input on Focused Canvas ----
  const SPECIAL_VK = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0d, Escape: 0x1b, ' ': 0x20,
    ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
    Delete: 0x2e, Shift: 0x10, Control: 0x11, Alt: 0x12, Meta: 0x5b,
  };

  cvs.addEventListener('keydown', (ev) => {
    if (!connected || !isTauri || !invoke) return;
    ev.preventDefault();
    if (SPECIAL_VK[ev.key]) {
      invoke('remote_key_event', { action: 0, vk: SPECIAL_VK[ev.key], unicodeChar: 0 }).catch(() => {});
    } else if (ev.key.length === 1 && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
      invoke('remote_key_event', { action: 2, vk: 0, unicodeChar: ev.key.codePointAt(0) }).catch(() => {});
    } else if (ev.key.length === 1) {
      const code = ev.key.toUpperCase().charCodeAt(0);
      invoke('remote_key_event', { action: 0, vk: code, unicodeChar: 0 }).catch(() => {});
    }
  });

  cvs.addEventListener('keyup', (ev) => {
    if (!connected || !isTauri || !invoke) return;
    ev.preventDefault();
    if (SPECIAL_VK[ev.key]) {
      invoke('remote_key_event', { action: 1, vk: SPECIAL_VK[ev.key], unicodeChar: 0 }).catch(() => {});
    } else if (ev.key.length === 1 && (ev.ctrlKey || ev.metaKey || ev.altKey)) {
      const code = ev.key.toUpperCase().charCodeAt(0);
      invoke('remote_key_event', { action: 1, vk: code, unicodeChar: 0 }).catch(() => {});
    }
  });

  $('btnSendText').addEventListener('click', async () => {
    const text = $('typeInput').value;
    if (!text) return;
    if (isTauri) {
      await invoke('remote_type', { text });
      $('typeInput').value = '';
      toast('已发送文本', 'ok');
    } else {
      toast('浏览器模式请在「剪贴板与运维」推送文本');
    }
  });
  $('typeInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') $('btnSendText').click();
  });

  $('btnClipGet').addEventListener('click', async () => {
    try {
      const txt = isTauri
        ? await invoke('remote_clipboard_get')
        : (await (await fetch('/api/clipboard', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ deviceId: $('connDevId').value.replace(/\s+/g, ''), token: $('connToken').value, action: 'get' }),
          })).json()).text;
      $('clipBox').value = txt || '';
      toast('已拉取远端剪贴板', 'ok');
    } catch (e) {
      toast('剪贴板错误: ' + e, 'err');
    }
  });

  $('btnClipSet').addEventListener('click', async () => {
    try {
      const text = $('clipBox').value;
      if (isTauri) {
        await invoke('remote_clipboard_set', { text });
      } else {
        await fetch('/api/clipboard', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ deviceId: $('connDevId').value.replace(/\s+/g, ''), token: $('connToken').value, action: 'set', text }),
        });
      }
      toast('已推送到远端剪贴板', 'ok');
    } catch (e) {
      toast('剪贴板错误: ' + e, 'err');
    }
  });

  $('btnListDir').addEventListener('click', async () => {
    try {
      const raw = isTauri
        ? await invoke('remote_list_dir', { remotePath: $('dirPath').value })
        : await (await fetch('/api/files', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ deviceId: $('connDevId').value.replace(/\s+/g, ''), token: $('connToken').value, action: 'list', remotePath: $('dirPath').value }),
          })).text();
      const res = JSON.parse(raw);
      const box = $('fileList');
      box.innerHTML = '';
      if (!res.ok) {
        box.textContent = res.error;
        return;
      }
      for (const f of res.entries || []) {
        const div = document.createElement('div');
        div.className = 'file-row';
        div.innerHTML = `<span>${f.isDir ? '📁 ' : '📄 '}${f.name}</span><span style="color:var(--muted)">${f.isDir ? '文件夹' : f.size + ' B'}</span>`;
        box.appendChild(div);
      }
      if (!res.entries || res.entries.length === 0) {
        box.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:12px 0">(当前目录为空)</div>';
      }
    } catch (e) {
      toast('读取目录失败: ' + e, 'err');
    }
  });

  $('btnUpload').addEventListener('click', async () => {
    try {
      const b64 = btoa(unescape(encodeURIComponent($('upContent').value)));
      const raw = isTauri
        ? await invoke('remote_upload_base64', { remotePath: $('upName').value, contentBase64: b64 })
        : await (await fetch('/api/files', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ deviceId: $('connDevId').value.replace(/\s+/g, ''), token: $('connToken').value, action: 'upload', remotePath: $('upName').value, contentBase64: b64 }),
          })).text();
      const res = JSON.parse(raw);
      if (res.ok) {
        toast(`上传成功 (${res.bytes} B, SHA-256 已校验)`, 'ok');
        $('btnListDir').click();
      } else {
        toast('上传被拦截: ' + res.error, 'err');
      }
    } catch (e) {
      toast('上传失败: ' + e, 'err');
    }
  });

  $('btnExec').addEventListener('click', async () => {
    try {
      const raw = isTauri
        ? await invoke('remote_exec_guarded', { command: $('execCmd').value })
        : await (await fetch('/api/exec', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ deviceId: $('connDevId').value.replace(/\s+/g, ''), token: $('connToken').value, command: $('execCmd').value }),
          })).text();
      const res = JSON.parse(raw);
      $('execOut').textContent = res.ok ? (res.stdout || '(无输出)') : `[拦截/错误] ${res.error}`;
    } catch (e) {
      $('execOut').textContent = 'Error: ' + e;
    }
  });

  async function initMode() {
    loadSettings();
    applyConnMode(connMode);
    $('displayCode').textContent = formatDeviceCode($('hostDevId').value);
    $('hostAddrHint').textContent = `局域网直连监听：${$('hostBind').value.trim() || '0.0.0.0:19800'}`;

    for (let i = 0; i < 10 && !getTauriInvoke(); i++) {
      await new Promise((r) => setTimeout(r, 40));
    }
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);

    if (isTauri) {
      const listen = getTauriListen();
      if (listen) {
        listen('remote-tile-frame', (event) => handleTilePushEvent(event.payload));
        listen('remote-rtt', (event) => {
          lastRttMs = Number(event.payload) || 0;
          updateHud();
        });
      }
      try {
        const info = await invoke('get_platform_info');
        $('modeSubtitle').textContent = `Tauri 2 · ${info.os}/${info.arch}`;
        $('engineDot').className = 'dot ok';
        $('engineText').textContent = `原生引擎就绪 v${info.version}`;
        log(`Tauri 2 原生推流引擎初始化成功 (${info.os}/${info.arch} v${info.version})`);
      } catch (e) {
        $('engineDot').className = 'dot warn';
        $('engineText').textContent = 'IPC 通信异常';
        log('IPC 初始化失败: ' + e);
        toast('IPC 初始化失败: ' + e, 'err');
      }
    } else {
      $('modeSubtitle').textContent = '浏览器零安装模式';
      $('engineDot').className = 'dot ok';
      $('engineText').textContent = 'Web Canvas2D 模式';
      $('btnStartHost').disabled = true;
      $('hostBadge').textContent = '浏览器仅支持控制端';
      $('hostAddrHint').textContent = '浏览器模式请通过中继设备码连接；本机被控请使用桌面安装包。';
      applyConnMode('relay');
      await refreshDevices();
    }
  }

  initMode();
});
