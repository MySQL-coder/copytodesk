'use strict';

window.addEventListener('DOMContentLoaded', () => {
  const $ = (id) => document.getElementById(id);
  const STORAGE_KEY = 'copytodesk_settings_v1';

  function log(msg) {
    const el = $('eventLog');
    if (!el) return;
    el.textContent = `[${new Date().toLocaleTimeString()}] ${msg}\n` + el.textContent.slice(0, 3000);
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

  let invoke = getTauriInvoke();
  let isTauri = Boolean(invoke);
  let connMode = 'relay';
  let connected = false;
  let loopToken = 0;
  let origW = 1280;
  let origH = 720;
  let frameCount = 0;
  let lastMoveSent = 0;

  function formatDeviceCode(raw) {
    const s = String(raw || '').replace(/\s+/g, '');
    if (/^\d{6,12}$/.test(s)) return s.replace(/(\d{3})(?=\d)/g, '$1 ');
    return s;
  }

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
      localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
    } catch {
      // ignore storage errors
    }
  }

  function loadSettings() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (!raw) return;
      const d = JSON.parse(raw);
      if (typeof d.hostDevId === 'string' && d.hostDevId.trim()) $('hostDevId').value = d.hostDevId;
      if (typeof d.hostToken === 'string') $('hostToken').value = d.hostToken;
      if (typeof d.hostBind === 'string' && d.hostBind.trim()) $('hostBind').value = d.hostBind;
      if (typeof d.hostRelay === 'string') $('hostRelay').value = d.hostRelay;
      if (typeof d.hostAllowInput === 'boolean') $('hostAllowInput').checked = d.hostAllowInput;
      if (typeof d.hostExec === 'boolean') $('hostExec').checked = d.hostExec;
      if (typeof d.connDirect === 'string') $('connDirect').value = d.connDirect;
      if (typeof d.connDevId === 'string') $('connDevId').value = d.connDevId;
      if (typeof d.connToken === 'string') $('connToken').value = d.connToken;
      if (typeof d.viewerQuality === 'string') $('viewerQuality').value = d.viewerQuality;
      if (d.connMode === 'direct' || d.connMode === 'relay') connMode = d.connMode;
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

  // Auto-save settings whenever any input changes
  for (const id of ['hostDevId', 'hostToken', 'hostBind', 'hostRelay', 'connDirect', 'connDevId', 'connToken', 'viewerQuality']) {
    $(id).addEventListener('input', () => {
      if (id === 'hostDevId') $('displayCode').textContent = formatDeviceCode($('hostDevId').value);
      if (id === 'hostBind') $('hostAddrHint').textContent = `直连监听：${$('hostBind').value.trim() || '127.0.0.1:19800'}`;
      if (id === 'hostRelay') $('relayAddrPreview').textContent = $('hostRelay').value.trim() || '未配置中继';
      saveSettings();
    });
    $(id).addEventListener('change', saveSettings);
  }
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
      const bindAddr = $('hostBind').value.trim() || '127.0.0.1:19800';
      const relayVal = $('hostRelay').value.trim();
      const st = await invoke('start_host', {
        config: {
          bindAddr,
          token: $('hostToken').value,
          deviceId: $('hostDevId').value.replace(/\s+/g, '') || '888100200',
          deviceName: 'CopyToDesk-Desktop',
          relayAddr: relayVal ? relayVal : null,
          fps: 20,
          jpegQuality: 70,
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
      $('hostQuickInfo').textContent = `被控运行中 (${relayVal ? '中继已注册' : st.bindAddr})`;
      $('hostNavDot').className = 'nav-dot on';
      $('btnStartHost').style.display = 'none';
      $('btnStopHost').style.display = 'inline-flex';
      log(`被控启动成功: ${st.captureBackend} ${st.width}x${st.height} | 中继=${relayVal || '无'} | 键鼠=${$('hostAllowInput').checked}`);
      toast(`被控服务已启动 (${st.width}×${st.height})`, 'ok');
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
    log('被控服务已停止');
    toast('已停止本机被控服务');
  });

  const cvs = $('screen');
  const ctx = cvs.getContext('2d');

  function currentQuality() {
    const parts = ($('viewerQuality').value || '1280:68').split(':');
    return { maxWidth: Number(parts[0]) || 1280, quality: Number(parts[1]) || 68 };
  }

  async function drawFrame() {
    const t0 = performance.now();
    const { maxWidth, quality } = currentQuality();
    try {
      if (isTauri) {
        const res = await invoke('remote_screenshot', { maxWidth, quality });
        origW = res.origW;
        origH = res.origH;
        await new Promise((resolve) => {
          const img = new Image();
          img.onload = () => {
            if (cvs.width !== img.width || cvs.height !== img.height) {
              cvs.width = img.width;
              cvs.height = img.height;
            }
            ctx.drawImage(img, 0, 0);
            frameCount++;
            const dt = (performance.now() - t0).toFixed(0);
            $('hud').textContent = `${origW}×${origH} · ${dt}ms · 帧 #${frameCount}`;
            resolve();
          };
          img.onerror = resolve;
          img.src = `data:${res.mimeType};base64,${res.dataBase64}`;
        });
      } else {
        const r = await fetch('/api/screenshot', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            deviceId: $('connDevId').value.replace(/\s+/g, ''),
            token: $('connToken').value,
            maxWidth,
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
        frameCount++;
        const dt = (performance.now() - t0).toFixed(0);
        $('hud').textContent = `${origW}×${origH} · ${dt}ms · Canvas2D #${frameCount}`;
      }
    } catch (e) {
      $('hud').textContent = '拉帧失败: ' + e;
    }
  }

  async function runFrameLoop(myToken) {
    while (connected && loopToken === myToken) {
      await drawFrame();
      // Small breather so mouse/keyboard IPC commands acquire client lock instantly between frames
      await new Promise((r) => setTimeout(r, 70));
    }
  }

  $('btnConnect').addEventListener('click', async () => {
    loopToken++;
    connected = false;
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);
    saveSettings();
    $('btnConnect').disabled = true;
    $('btnConnect').textContent = '连接中...';
    try {
      if (isTauri) {
        const useRelay = connMode === 'relay';
        const relayAddr = $('hostRelay').value.trim() || '38.34.13.214:19901';
        const ack = await invoke('connect_remote', {
          params: {
            directAddr: useRelay ? null : ($('connDirect').value.trim() || '127.0.0.1:19800'),
            relayAddr: useRelay ? relayAddr : null,
            deviceId: useRelay ? ($('connDevId').value.replace(/\s+/g, '') || '888100200') : null,
            token: $('connToken').value,
            clientName: 'CopyToDesk-Desktop',
          },
        });
        origW = ack.width;
        origH = ack.height;
        log(`连接成功: ${ack.captureBackend} ${ack.width}x${ack.height} (输入驱动: ${ack.inputBackend})`);
        if (ack.inputBackend === 'DryRun-Audit') {
          toast('已连接（对方未开启键鼠控制，仅观看模式）');
        } else {
          toast(`已连接 (${ack.inputBackend})`, 'ok');
        }
      } else {
        toast('已建立 Web 远程连接', 'ok');
      }
      connected = true;
      frameCount = 0;
      $('connNavDot').className = 'nav-dot on';
      $('emptyStage').style.display = 'none';
      cvs.style.display = 'block';
      switchTab('viewer');
      cvs.focus();
      const myToken = ++loopToken;
      runFrameLoop(myToken);
    } catch (e) {
      log('连接失败: ' + e);
      toast('连接失败: ' + e, 'err');
    } finally {
      $('btnConnect').disabled = false;
      $('btnConnect').textContent = '立即连接';
    }
  });

  $('btnDisconnect').addEventListener('click', async () => {
    loopToken++;
    connected = false;
    if (isTauri && invoke) await invoke('disconnect_remote');
    $('connNavDot').className = 'nav-dot';
    $('hud').textContent = '已断开';
    cvs.style.display = 'none';
    $('emptyStage').style.display = 'flex';
    toast('已断开远程连接');
  });

  $('btnSnap').addEventListener('click', drawFrame);

  // ---- Full Remote Mouse Control (Move / Left / Right / Middle / Drag / Wheel) ----
  function mapCanvasXY(ev) {
    const r = cvs.getBoundingClientRect();
    const x = Math.max(0, Math.min(origW, Math.round(((ev.clientX - r.left) / Math.max(1, r.width)) * origW)));
    const y = Math.max(0, Math.min(origH, Math.round(((ev.clientY - r.top) / Math.max(1, r.height)) * origH)));
    return { x, y };
  }

  function mapBtn(evButton) {
    if (evButton === 2) return 2; // Right
    if (evButton === 1) return 3; // Middle
    return 1; // Left
  }

  async function sendMouse(action, button, x, y, wheelDelta = 0) {
    if (!connected) return;
    try {
      if (isTauri) {
        await invoke('remote_mouse_event', { action, button, x, y, wheelDelta });
      } else if (action === 1) {
        await fetch('/api/input', {
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
        });
      }
    } catch {
      // ignore transient mouse send errors
    }
  }

  cvs.addEventListener('contextmenu', (ev) => ev.preventDefault());

  cvs.addEventListener('mousedown', (ev) => {
    ev.preventDefault();
    cvs.focus();
    const { x, y } = mapCanvasXY(ev);
    sendMouse(1, mapBtn(ev.button), x, y, 0);
  });

  cvs.addEventListener('mouseup', (ev) => {
    ev.preventDefault();
    const { x, y } = mapCanvasXY(ev);
    sendMouse(2, mapBtn(ev.button), x, y, 0);
  });

  cvs.addEventListener('mousemove', (ev) => {
    if (!connected || !isTauri) return;
    const now = performance.now();
    if (now - lastMoveSent < 45) return;
    lastMoveSent = now;
    const { x, y } = mapCanvasXY(ev);
    sendMouse(0, 0, x, y, 0);
  });

  cvs.addEventListener('wheel', (ev) => {
    if (!connected || !isTauri) return;
    ev.preventDefault();
    const { x, y } = mapCanvasXY(ev);
    const delta = ev.deltaY < 0 ? 120 : -120;
    sendMouse(3, 0, x, y, delta);
  }, { passive: false });

  // ---- Direct Keyboard Input on Focused Canvas ----
  const SPECIAL_VK = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0d, Escape: 0x1b, ' ': 0x20,
    ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
    Delete: 0x2e, Shift: 0x10, Control: 0x11, Alt: 0x12, Meta: 0x5b,
  };

  cvs.addEventListener('keydown', async (ev) => {
    if (!connected || !isTauri) return;
    ev.preventDefault();
    if (SPECIAL_VK[ev.key]) {
      await invoke('remote_key_event', { action: 0, vk: SPECIAL_VK[ev.key], unicodeChar: 0 }).catch(() => {});
    } else if (ev.key.length === 1 && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
      await invoke('remote_key_event', { action: 2, vk: 0, unicodeChar: ev.key.codePointAt(0) }).catch(() => {});
    } else if (ev.key.length === 1) {
      const code = ev.key.toUpperCase().charCodeAt(0);
      await invoke('remote_key_event', { action: 0, vk: code, unicodeChar: 0 }).catch(() => {});
    }
  });

  cvs.addEventListener('keyup', async (ev) => {
    if (!connected || !isTauri) return;
    ev.preventDefault();
    if (SPECIAL_VK[ev.key]) {
      await invoke('remote_key_event', { action: 1, vk: SPECIAL_VK[ev.key], unicodeChar: 0 }).catch(() => {});
    } else if (ev.key.length === 1 && (ev.ctrlKey || ev.metaKey || ev.altKey)) {
      const code = ev.key.toUpperCase().charCodeAt(0);
      await invoke('remote_key_event', { action: 1, vk: code, unicodeChar: 0 }).catch(() => {});
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
    $('hostAddrHint').textContent = `直连监听：${$('hostBind').value.trim() || '127.0.0.1:19800'}`;

    for (let i = 0; i < 10 && !getTauriInvoke(); i++) {
      await new Promise((r) => setTimeout(r, 40));
    }
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);

    if (isTauri) {
      try {
        const info = await invoke('get_platform_info');
        $('modeSubtitle').textContent = `Tauri 2 · ${info.os}/${info.arch}`;
        $('engineDot').className = 'dot ok';
        $('engineText').textContent = `原生引擎就绪 v${info.version}`;
        log(`Tauri 2 原生引擎初始化成功 (${info.os}/${info.arch} v${info.version})`);
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
