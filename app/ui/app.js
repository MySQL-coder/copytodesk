'use strict';

window.addEventListener('DOMContentLoaded', () => {
  const $ = (id) => document.getElementById(id);

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
  let connMode = 'direct';
  let pollTimer = null;
  let connected = false;
  let origW = 1280;
  let origH = 720;
  let frameCount = 0;

  function formatDeviceCode(raw) {
    const s = String(raw || '').replace(/\s+/g, '');
    return s.replace(/(\d{3})(?=\d)/g, '$1 ');
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
      connMode = b.dataset.connmode;
      document.querySelectorAll('.mode-btn').forEach((x) => x.classList.toggle('active', x === b));
      $('fieldDirect').style.display = connMode === 'direct' ? 'block' : 'none';
      $('fieldRelay').style.display = connMode === 'relay' ? 'block' : 'none';
      $('relayQuickBar').style.display = connMode === 'relay' ? 'flex' : 'none';
      $('relayAddrPreview').textContent = $('hostRelay').value.trim() || '127.0.0.1:19901';
    });
  });

  $('hostDevId').addEventListener('input', () => {
    $('displayCode').textContent = formatDeviceCode($('hostDevId').value);
  });
  $('hostBind').addEventListener('input', () => {
    $('hostAddrHint').textContent = `直连监听：${$('hostBind').value.trim() || '127.0.0.1:19800'}`;
  });

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
    $('connToken').value = p;
    toast('已刷新临时密码：' + p);
  });

  async function refreshDevices() {
    if (isTauri) {
      toast('中继地址可在「高级设置」中修改');
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
          toast('已选中设备 ' + d.deviceId);
        });
        wrap.appendChild(card);
      }
      if ((data.devices || []).length > 0) $('connDevId').value = data.devices[0].deviceId;
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
          jpegQuality: 75,
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
      $('hostQuickInfo').textContent = `被控运行中 (${st.bindAddr})`;
      $('hostNavDot').className = 'nav-dot on';
      $('btnStartHost').style.display = 'none';
      $('btnStopHost').style.display = 'inline-flex';
      $('connDirect').value = st.bindAddr;
      $('connToken').value = $('hostToken').value;
      log(`被控启动成功: ${st.captureBackend} ${st.width}x${st.height} | 监听 ${st.bindAddr}`);
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

  async function drawFrame() {
    const t0 = performance.now();
    try {
      if (isTauri) {
        const res = await invoke('remote_screenshot', { maxWidth: 1280, quality: 72 });
        origW = res.origW;
        origH = res.origH;
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
        };
        img.src = `data:${res.mimeType};base64,${res.dataBase64}`;
      } else {
        const r = await fetch('/api/screenshot', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            deviceId: $('connDevId').value.replace(/\s+/g, ''),
            token: $('connToken').value,
            maxWidth: 1280,
            quality: 72,
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

  $('btnConnect').addEventListener('click', async () => {
    if (pollTimer) clearInterval(pollTimer);
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);
    $('btnConnect').disabled = true;
    $('btnConnect').textContent = '连接中...';
    try {
      if (isTauri) {
        const useRelay = connMode === 'relay';
        const relayAddr = $('hostRelay').value.trim() || '127.0.0.1:19901';
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
        log(`连接成功: ${ack.captureBackend} ${ack.width}x${ack.height} (输入: ${ack.inputBackend})`);
      }
      connected = true;
      frameCount = 0;
      $('connNavDot').className = 'nav-dot on';
      $('emptyStage').style.display = 'none';
      cvs.style.display = 'block';
      switchTab('viewer');
      toast('已建立远程连接', 'ok');
      await drawFrame();
      pollTimer = setInterval(drawFrame, 280);
    } catch (e) {
      log('连接失败: ' + e);
      toast('连接失败: ' + e, 'err');
    } finally {
      $('btnConnect').disabled = false;
      $('btnConnect').textContent = '立即连接';
    }
  });

  $('btnDisconnect').addEventListener('click', async () => {
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
    connected = false;
    if (isTauri && invoke) await invoke('disconnect_remote');
    $('connNavDot').className = 'nav-dot';
    $('hud').textContent = '已断开';
    cvs.style.display = 'none';
    $('emptyStage').style.display = 'flex';
    toast('已断开远程连接');
  });

  $('btnSnap').addEventListener('click', drawFrame);

  cvs.addEventListener('click', async (ev) => {
    if (!connected) return;
    const r = cvs.getBoundingClientRect();
    const x = Math.round(((ev.clientX - r.left) / r.width) * origW);
    const y = Math.round(((ev.clientY - r.top) / r.height) * origH);
    try {
      if (isTauri) {
        await invoke('remote_click', { x, y, button: 1 });
      } else {
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
      log(`远程点击 (${x}, ${y})`);
    } catch (e) {
      toast('点击发送失败: ' + e, 'err');
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
    // Wait briefly if WebView2 injects __TAURI__ asynchronously
    for (let i = 0; i < 10 && !getTauriInvoke(); i++) {
      await new Promise((r) => setTimeout(r, 40));
    }
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);
    $('displayCode').textContent = formatDeviceCode($('hostDevId').value);

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
      const relayBtn = document.querySelector('[data-connmode="relay"]');
      if (relayBtn) relayBtn.click();
      await refreshDevices();
    }
  }

  initMode();
});
