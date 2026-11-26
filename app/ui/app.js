'use strict';

window.addEventListener('DOMContentLoaded', () => {
  const $ = (id) => document.getElementById(id);
  const STORAGE_KEY_V2 = 'copytodesk_settings_v2';
  const STORAGE_KEY_V1 = 'copytodesk_settings_v1';
  const FILE_CHUNK_BYTES = 256 * 1024; // 256 KB streaming chunks

  const KEY_MOD_SHIFT = 1 << 1;
  const KEY_MOD_CTRL = 1 << 2;
  const KEY_MOD_ALT = 1 << 3;
  const KEY_MOD_META = 1 << 4;

  const CURSOR_CSS_MAP = {
    0: 'default',
    1: 'text',
    2: 'pointer',
    3: 'wait',
    4: 'crosshair',
    5: 'ew-resize',
    6: 'ns-resize',
    7: 'nwse-resize',
    8: 'nesw-resize',
    9: 'move',
    10: 'not-allowed',
  };

  function log(msg) {
    const el = $('eventLog');
    if (!el) return;
    el.textContent = `[${new Date().toLocaleTimeString()}] ${msg}\n` + el.textContent.slice(0, 3800);
  }

  function toast(msg, type = '') {
    const wrap = $('toastWrap');
    if (!wrap) return;
    const d = document.createElement('div');
    d.className = 'toast ' + type;
    d.textContent = msg;
    wrap.appendChild(d);
    setTimeout(() => d.remove(), 3400);
  }

  window.addEventListener('error', (ev) => {
    const m = ev.message || 'Unknown script error';
    if ($('engineText')) $('engineText').textContent = 'JS 错误: ' + m.slice(0, 24);
    log('JS Error: ' + m);
    toast('脚本异常: ' + m, 'err');
  });

  function randomHex(byteCount) {
    const arr = new Uint8Array(byteCount);
    if (window.crypto && window.crypto.getRandomValues) {
      window.crypto.getRandomValues(arr);
    } else {
      for (let i = 0; i < byteCount; i++) arr[i] = Math.floor(Math.random() * 256);
    }
    return Array.from(arr, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  function formatBytes(bytes) {
    const n = Number(bytes) || 0;
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(2)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  function uint8ToBase64(u8) {
    let binary = '';
    const step = 0x8000;
    for (let i = 0; i < u8.length; i += step) {
      binary += String.fromCharCode.apply(null, u8.subarray(i, i + step));
    }
    return btoa(binary);
  }

  function base64ToUint8(b64) {
    const bin = atob(b64);
    const len = bin.length;
    const out = new Uint8Array(len);
    for (let i = 0; i < len; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

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

  function getTauriChannelCtor() {
    if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.Channel === 'function') {
      return window.__TAURI__.core.Channel;
    }
    if (window.__TAURI_INTERNALS__ && typeof window.__TAURI_INTERNALS__.Channel === 'function') {
      return window.__TAURI_INTERNALS__.Channel;
    }
    return null;
  }

  let invoke = getTauriInvoke();
  let isTauri = Boolean(invoke);
  let localOs = navigator.platform.toLowerCase().includes('mac') ? 'macos' : 'windows';
  let remoteOs = '';
  let deviceSecret = '';
  let hostRunning = false;
  let connMode = 'relay';
  let connected = false;
  let reconnecting = false;
  let reconnToken = 0;
  let lastConnectParams = null;
  let ctrlCmdMapEnabled = false;
  let invertScroll = false;
  let scaleOneToOne = false;
  let savedDevices = [];
  let editingBookKey = null;
  let renamingFilePath = null;
  let transferActive = false;
  let uploadCancelRequested = false;
  let viewerPillHideTimer = null;
  let autoTier = '1280:68';
  let webLoopToken = 0;
  let origW = 1280;
  let origH = 720;
  let streamW = 1280;
  let streamH = 720;
  let lastMoveSent = 0;
  let cursorHideTimer = null;

  // Rolling 1s HUD telemetry
  let secFrames = 0;
  let secBytes = 0;
  let curFps = 0;
  let curKbps = 0;
  let lastRttMs = 0;
  let losslessState = false;

  const heldRemoteVks = new Set();

  const cvs = $('screen');
  const ctx = cvs.getContext('2d');

  function formatDeviceCode(raw) {
    const s = String(raw || '').replace(/\s+/g, '');
    if (/^\d{6,12}$/.test(s)) return s.replace(/(\d{3})(?=\d)/g, '$1 ');
    return s;
  }

  function updatePermBadge() {
    const hasPerm = Boolean($('hostPermPwd').value.trim());
    $('permPwdBadge').style.display = hasPerm ? 'inline-flex' : 'none';
  }

  function updateHud() {
    if (!connected) return;
    const rttPart = lastRttMs > 0 ? ` · ${lastRttMs.toFixed(1)}ms` : '';
    const refPart = losslessState ? ' · 无损✓' : '';
    $('hud').textContent = `${streamW}×${streamH} · ${curFps} FPS · ${curKbps.toFixed(0)} KB/s${rttPart}${refPart}`;
  }

  function updateScrollDirBtn() {
    const btn = $('btnScrollDir');
    if (!btn) return;
    btn.classList.toggle('active', invertScroll);
    btn.textContent = invertScroll ? '⇅ 滚轮反向' : '⇅ 滚轮正向';
  }

  function renderSavedBook() {
    const wrap = $('savedBookWrap');
    const grid = $('savedBookGrid');
    if (!wrap || !grid) return;
    if (!Array.isArray(savedDevices) || savedDevices.length === 0) {
      wrap.style.display = 'none';
      grid.innerHTML = '';
      return;
    }
    wrap.style.display = 'block';
    grid.innerHTML = '';

    for (const item of savedDevices) {
      const card = document.createElement('div');
      card.className = 'book-card';

      const left = document.createElement('div');
      left.style.minWidth = '0';
      left.style.flex = '1';

      if (editingBookKey === item.key) {
        const inp = document.createElement('input');
        inp.className = 'input';
        inp.style.height = '28px';
        inp.style.fontSize = '12.5px';
        inp.value = item.alias || '';
        inp.placeholder = '输入设备备注名（回车保存）';
        inp.addEventListener('click', (e) => e.stopPropagation());
        const commitAlias = () => {
          item.alias = inp.value.trim();
          editingBookKey = null;
          saveSettings();
          renderSavedBook();
        };
        inp.addEventListener('keydown', (e) => {
          if (e.key === 'Enter') commitAlias();
          if (e.key === 'Escape') {
            editingBookKey = null;
            renderSavedBook();
          }
        });
        inp.addEventListener('blur', commitAlias);
        left.appendChild(inp);
        setTimeout(() => inp.focus(), 10);
      } else {
        const title = document.createElement('div');
        title.style.fontWeight = '600';
        title.style.fontSize = '13px';
        title.style.overflow = 'hidden';
        title.style.textOverflow = 'ellipsis';
        title.style.whiteSpace = 'nowrap';
        const dispTarget = item.mode === 'relay' ? formatDeviceCode(item.target) : item.target;
        title.textContent = item.alias ? `${item.alias} (${dispTarget})` : dispTarget;

        const sub = document.createElement('div');
        sub.style.fontSize = '11px';
        sub.style.color = 'var(--muted)';
        const modeTag = item.mode === 'relay' ? '🌐 设备码' : '⚡ 直连';
        const pwdTag = item.token ? ' · 已存密' : '';
        sub.textContent = `${modeTag}${pwdTag}`;

        left.appendChild(title);
        left.appendChild(sub);
      }

      const actions = document.createElement('div');
      actions.className = 'book-actions';

      const editBtn = document.createElement('button');
      editBtn.type = 'button';
      editBtn.className = 'icon-btn';
      editBtn.style.width = '26px';
      editBtn.style.height = '26px';
      editBtn.title = '修改备注名';
      editBtn.textContent = '✎';
      editBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        editingBookKey = item.key;
        renderSavedBook();
      });

      const delBtn = document.createElement('button');
      delBtn.type = 'button';
      delBtn.className = 'icon-btn';
      delBtn.style.width = '26px';
      delBtn.style.height = '26px';
      delBtn.title = '移除记录';
      delBtn.textContent = '✕';
      delBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        savedDevices = savedDevices.filter((x) => x.key !== item.key);
        saveSettings();
        renderSavedBook();
      });

      actions.appendChild(editBtn);
      actions.appendChild(delBtn);
      card.appendChild(left);
      card.appendChild(actions);

      card.addEventListener('click', () => {
        if (editingBookKey === item.key) return;
        applyConnMode(item.mode);
        if (item.mode === 'relay') {
          $('connDevId').value = item.target;
        } else {
          $('connDirect').value = item.target;
        }
        if (item.token) {
          $('connToken').value = item.token;
        }
        saveSettings();
        $('btnConnect').click();
      });

      grid.appendChild(card);
    }
  }

  function recordSavedDevice(mode, target, token, remoteOsHint) {
    const cleanTarget = String(target || '').trim();
    if (!cleanTarget) return;
    const key = `${mode}:${cleanTarget}`;
    const existing = savedDevices.find((x) => x.key === key);
    const keepPwd = $('connSavePwd').checked;
    const entry = {
      key,
      mode,
      target: cleanTarget,
      alias: existing && existing.alias ? existing.alias : (remoteOsHint ? `${remoteOsHint.toUpperCase()} 设备` : ''),
      token: keepPwd ? token : '',
      updatedAt: Date.now(),
    };
    savedDevices = [entry, ...savedDevices.filter((x) => x.key !== key)].slice(0, 12);
    saveSettings();
    renderSavedBook();
  }
  function updateKeyMapBtn() {
    const btn = $('btnKeyMap');
    if (!btn) return;
    btn.classList.toggle('active', ctrlCmdMapEnabled);
    if (!ctrlCmdMapEnabled) {
      btn.textContent = '⌘ Ctrl↔Cmd: 关';
    } else if (localOs !== 'macos' && remoteOs === 'macos') {
      btn.textContent = '⌘ Ctrl→Cmd: 开';
    } else if (localOs === 'macos' && remoteOs !== 'macos') {
      btn.textContent = '⌘ Cmd→Ctrl: 开';
    } else {
      btn.textContent = '⌘ Ctrl↔Cmd: 开';
    }
  }

  setInterval(() => {
    if (!connected || !isTauri) return;
    curFps = secFrames;
    curKbps = secBytes / 1024;
    secFrames = 0;
    secBytes = 0;
    updateHud();
  }, 1000);

  // Auto Adaptive Bitrate Controller (runs every 4s when viewerQuality == 'auto')
  setInterval(() => {
    if (!connected || !isTauri || !invoke) return;
    if ($('viewerQuality').value !== 'auto' || lastRttMs <= 0) return;
    let nextTier = autoTier;
    if (lastRttMs < 24) {
      nextTier = '1920:78';
    } else if (lastRttMs > 95) {
      nextTier = '960:56';
    } else if (lastRttMs > 32 && lastRttMs <= 85) {
      nextTier = '1280:68';
    }
    if (nextTier !== autoTier) {
      autoTier = nextTier;
      const [mw, q] = nextTier.split(':').map(Number);
      invoke('remote_set_stream_quality', { maxWidth: mw, quality: q }).catch(() => {});
      log(`智能码率自适应调整: RTT=${lastRttMs.toFixed(1)}ms -> ${mw}p Q=${q}`);
    }
  }, 4000);

  function saveSettings() {
    try {
      if (!deviceSecret) deviceSecret = randomHex(16);
      const data = {
        deviceSecret,
        hostDevId: $('hostDevId').value,
        hostToken: $('hostToken').value,
        hostPermPwd: $('hostPermPwd').value,
        hostBind: $('hostBind').value,
        hostRelay: $('hostRelay').value,
        hostFileRoot: $('hostFileRoot').value,
        hostPreventSleep: $('hostPreventSleep').checked,
        hostCloseToTray: $('hostCloseToTray').checked,
        hostAutoStart: $('hostAutoStart').checked,
        hostAutoClipSync: $('hostAutoClipSync').checked,
        hostAllowInput: $('hostAllowInput').checked,
        hostExec: $('hostExec').checked,
        connMode,
        connDirect: $('connDirect').value,
        connDevId: $('connDevId').value,
        connToken: $('connToken').value,
        connSavePwd: $('connSavePwd').checked,
        viewerQuality: $('viewerQuality').value,
        invertScroll,
        savedDevices,
      };
      localStorage.setItem(STORAGE_KEY_V2, JSON.stringify(data));
      updatePermBadge();
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
      if (raw) {
        const d = JSON.parse(raw);
        if (typeof d.deviceSecret === 'string' && d.deviceSecret.length >= 16) {
          deviceSecret = d.deviceSecret;
        }
        if (typeof d.hostDevId === 'string' && d.hostDevId.trim()) $('hostDevId').value = d.hostDevId;
        if (typeof d.hostToken === 'string') $('hostToken').value = d.hostToken;
        if (typeof d.hostPermPwd === 'string') $('hostPermPwd').value = d.hostPermPwd;
        if (typeof d.hostBind === 'string' && d.hostBind.trim()) {
          $('hostBind').value = (fromV1 && d.hostBind.trim() === '127.0.0.1:19800') ? '0.0.0.0:19800' : d.hostBind;
        }
        if (typeof d.hostRelay === 'string') $('hostRelay').value = d.hostRelay;
        if (typeof d.hostFileRoot === 'string') $('hostFileRoot').value = d.hostFileRoot;
        if (typeof d.hostPreventSleep === 'boolean') $('hostPreventSleep').checked = d.hostPreventSleep;
        if (typeof d.hostCloseToTray === 'boolean') $('hostCloseToTray').checked = d.hostCloseToTray;
        if (typeof d.hostAutoStart === 'boolean') $('hostAutoStart').checked = d.hostAutoStart;
        if (typeof d.hostAutoClipSync === 'boolean') $('hostAutoClipSync').checked = d.hostAutoClipSync;
        if (typeof d.hostAllowInput === 'boolean') $('hostAllowInput').checked = d.hostAllowInput;
        if (typeof d.hostExec === 'boolean') $('hostExec').checked = d.hostExec;
        if (typeof d.connDirect === 'string') $('connDirect').value = d.connDirect;
        if (typeof d.connDevId === 'string') $('connDevId').value = d.connDevId;
        if (typeof d.connToken === 'string') $('connToken').value = d.connToken;
        if (typeof d.connSavePwd === 'boolean') $('connSavePwd').checked = d.connSavePwd;
        if (typeof d.viewerQuality === 'string') $('viewerQuality').value = d.viewerQuality;
        if (typeof d.invertScroll === 'boolean') invertScroll = d.invertScroll;
        if (Array.isArray(d.savedDevices)) savedDevices = d.savedDevices;
        if (d.connMode === 'direct' || d.connMode === 'relay') connMode = d.connMode;
      }
      if (!deviceSecret) {
        deviceSecret = randomHex(16);
        saveSettings();
      } else if (fromV1) {
        saveSettings();
      }
      updatePermBadge();
    } catch {
      if (!deviceSecret) deviceSecret = randomHex(16);
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
    if (name === 'files' && connected) {
      refreshRemoteDir().catch(() => {});
    }
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
    const raw = $('viewerQuality').value || 'auto';
    const spec = raw === 'auto' ? autoTier : raw;
    const parts = spec.split(':');
    return { maxWidth: Number(parts[0]) || 0, quality: Number(parts[1]) || 68 };
  }

  for (const id of ['hostDevId', 'hostToken', 'hostPermPwd', 'hostBind', 'hostRelay', 'hostFileRoot', 'connDirect', 'connDevId', 'connToken']) {
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
      toast($('viewerQuality').value === 'auto' ? '已启用智能网络自适应画质' : '已切换画质并请求关键帧');
    }
  });

  for (const id of ['hostPreventSleep', 'hostAutoStart', 'hostAutoClipSync', 'hostAllowInput', 'hostExec', 'connSavePwd']) {
    $(id).addEventListener('change', saveSettings);
  }
  $('hostOsAutoStart').addEventListener('change', async () => {
    if (!isTauri || !invoke) {
      $('hostOsAutoStart').checked = false;
      toast('仅桌面安装包支持系统开机自启', 'err');
      return;
    }
    const want = $('hostOsAutoStart').checked;
    try {
      await invoke('set_os_autostart', { enabled: want });
      if (want && !$('hostAutoStart').checked) {
        $('hostAutoStart').checked = true;
        saveSettings();
      }
      toast(want ? '已开启系统开机自启（登录时静默进托盘并自动开启被控）' : '已关闭系统开机自启', 'ok');
      log(`系统开机自启状态已更新: ${want}`);
    } catch (e) {
      $('hostOsAutoStart').checked = !want;
      toast('设置开机自启失败: ' + e, 'err');
    }
  });
  $('hostCloseToTray').addEventListener('change', () => {
    saveSettings();
    if (isTauri && invoke) {
      invoke('set_close_to_tray', { enabled: $('hostCloseToTray').checked }).catch(() => {});
    }
  });
  $('btnOpenSharedRoot').addEventListener('click', async () => {
    if (!isTauri || !invoke) {
      toast('仅桌面端支持打开本机共享目录');
      return;
    }
    try {
      const dir = await invoke('open_shared_dir', { customPath: $('hostFileRoot').value.trim() });
      toast('已打开本机共享目录: ' + dir);
    } catch (e) {
      toast('打开共享目录失败: ' + e, 'err');
    }
  });
  $('btnCopyMacFix').addEventListener('click', async () => {
    const cmd = 'sudo xattr -rd com.apple.quarantine /Applications/CopyToDesk.app';
    if (isTauri && invoke) await invoke('local_clipboard_set', { text: cmd }).catch(() => {});
    else if (navigator.clipboard) navigator.clipboard.writeText(cmd).catch(() => {});
    toast('已复制 macOS 放行命令，粘贴至终端执行即可', 'ok');
  });

  $('btnClearBook').addEventListener('click', () => {
    savedDevices = [];
    saveSettings();
    renderSavedBook();
    toast('已清空设备簿记录');
  });

  $('btnRotateSecret').addEventListener('click', () => {
    deviceSecret = randomHex(16);
    saveSettings();
    log('已重新生成本机防抢注密钥（若原设备码已绑定旧密钥，请同步更换新设备代码）');
    toast('已重置本机防抢注密钥', 'ok');
  });

  $('btnCopyCode').addEventListener('click', async () => {
    const val = $('hostDevId').value.trim();
    if (isTauri && invoke) await invoke('local_clipboard_set', { text: val }).catch(() => {});
    else if (navigator.clipboard) navigator.clipboard.writeText(val).catch(() => {});
    toast('已复制设备代码', 'ok');
  });
  $('btnCopyPwd').addEventListener('click', async () => {
    const val = $('hostToken').value;
    if (isTauri && invoke) await invoke('local_clipboard_set', { text: val }).catch(() => {});
    else if (navigator.clipboard) navigator.clipboard.writeText(val).catch(() => {});
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
      if (data.privacyProtected) {
        log(`中继隐私防枚举已开启（当前在线设备总数: ${data.onlineCount || 0}，请直接输入目标设备码连接）`);
        return;
      }
      for (const d of data.devices || []) {
        const card = document.createElement('div');
        card.className = 'dev-card';
        const shield = d.owned ? ' 🛡' : '';
        card.innerHTML = `<span class="dot ok"></span><div><div style="font-weight:600;font-size:13.5px">${formatDeviceCode(d.deviceId)}${shield}</div><div style="font-size:11.5px;color:var(--muted)">${d.name}</div></div>`;
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

  // Poll host relay status + live connected viewers audit
  async function refreshHostStatusAudit() {
    if (!hostRunning || !isTauri || !invoke) return;
    try {
      const st = await invoke('host_status');
      if (!st || !st.running) return;
      const ob = $('ownerInfoBadge');
      if (st.relayStatus === 'connected') {
        ob.style.display = 'inline-flex';
        ob.className = 'badge ok';
        ob.textContent = '🛡 防抢注已锁定 · 中继在线';
      } else if (st.relayStatus === 'already_owned') {
        ob.style.display = 'inline-flex';
        ob.className = 'badge err';
        ob.textContent = '⚠ 设备码已被他人占用';
        $('hostAddrHint').textContent = '⚠ 该设备代码已被其他设备绑定！请在「高级设置」更换本机设备代码后重启服务';
      } else if (st.relayStatus === 'reconnecting' || st.relayStatus === 'connecting') {
        ob.style.display = 'inline-flex';
        ob.className = 'badge';
        ob.textContent = '中继连接中...';
      } else {
        ob.style.display = 'none';
      }

      const sessions = st.activeSessions || [];
      if (sessions.length > 0) {
        $('activeSessionsBar').style.display = 'flex';
        $('activeSessionsText').textContent = `👁 当前正在被远控 (${sessions.length} 台)：${sessions.join(', ')}`;
      } else {
        $('activeSessionsBar').style.display = 'none';
      }
    } catch {
      // ignore
    }
  }

  setInterval(() => {
    if (hostRunning) refreshHostStatusAudit();
  }, 3000);

  $('btnKickViewers').addEventListener('click', async () => {
    if (!isTauri || !invoke) return;
    await invoke('host_kick_viewers').catch(() => {});
    $('activeSessionsBar').style.display = 'none';
    log('已主动强制断开所有正在远控本机的会话');
    toast('已断开所有远控会话', 'ok');
    setTimeout(refreshHostStatusAudit, 400);
  });

  async function startHostService(silent = false) {
    invoke = getTauriInvoke();
    if (!invoke) {
      if (!silent) toast('当前非桌面安装包环境，无法启动本机被控', 'err');
      return false;
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
          unattendedToken: $('hostPermPwd').value.trim(),
          deviceId: $('hostDevId').value.replace(/\s+/g, '') || '888100200',
          deviceSecret,
          deviceName: `CopyToDesk-${localOs}`,
          relayAddr: relayVal ? relayVal : null,
          fps: 25,
          jpegQuality: 72,
          forceSynthetic: false,
          fileRoot: $('hostFileRoot').value.trim(),
          allowInput: $('hostAllowInput').checked,
          allowExec: $('hostExec').checked,
          execAllowlist: ['echo', 'hostname', 'whoami'],
          preventSleep: $('hostPreventSleep').checked,
          autoClipboardSync: $('hostAutoClipSync').checked,
        },
      });
      hostRunning = true;
      $('hostBadge').textContent = '被控运行中';
      $('hostBadge').className = 'badge ok';
      const monCount = (st.monitors && st.monitors.length) ? ` · ${st.monitors.length}屏` : '';
      $('captureInfoBadge').textContent = `${st.captureBackend} · ${st.width}×${st.height}${monCount}`;
      const lanHint = (st.lanAddrs && st.lanAddrs.length > 0) ? `局域网IP: ${st.lanAddrs.join(', ')}` : st.bindAddr;
      const sleepTag = st.preventSleep ? ' + 防休眠' : '';
      $('hostAddrHint').textContent = `${lanHint}（E2EE${sleepTag} + 20s 保活已启用）`;
      $('hostQuickInfo').textContent = `被控运行中 (${relayVal ? '中继+内网双栈' : st.bindAddr})`;
      $('hostNavDot').className = 'nav-dot on';
      $('btnStartHost').style.display = 'none';
      $('btnStopHost').style.display = 'inline-flex';
      log(`被控启动成功: ${st.captureBackend} ${st.width}x${st.height} | 共享目录=${st.fileRoot || ''}`);
      if (!silent) toast(`被控已启动 (${st.width}×${st.height})`, 'ok');
      setTimeout(refreshHostStatusAudit, 800);
      return true;
    } catch (e) {
      $('hostBadge').textContent = '启动失败';
      $('hostBadge').className = 'badge err';
      log('被控启动失败: ' + e);
      if (!silent) toast('启动失败: ' + e, 'err');
      return false;
    } finally {
      $('btnStartHost').disabled = false;
      $('btnStartHost').textContent = '启动被控服务';
    }
  }

  $('btnStartHost').addEventListener('click', () => {
    startHostService(false);
  });

  $('btnApplyHostSettings').addEventListener('click', async () => {
    saveSettings();
    if (hostRunning) {
      const ok = await startHostService(true);
      if (ok) {
        toast('已保存配置并热重载本机被控服务', 'ok');
      }
    } else {
      toast('设置已保存（下次开启被控立即生效）', 'ok');
    }
  });

  $('btnStopHost').addEventListener('click', async () => {
    invoke = getTauriInvoke();
    if (!invoke) return;
    await invoke('stop_host');
    hostRunning = false;
    $('hostBadge').textContent = '被控已停止';
    $('hostBadge').className = 'badge';
    $('captureInfoBadge').textContent = '待启动';
    $('ownerInfoBadge').style.display = 'none';
    $('activeSessionsBar').style.display = 'none';
    $('hostQuickInfo').textContent = '被控服务：已停止';
    $('hostNavDot').className = 'nav-dot';
    $('btnStopHost').style.display = 'none';
    $('btnStartHost').style.display = 'inline-flex';
    log('被控服务已秒停释放端口');
    toast('已停止本机被控服务');
  });

  // ---- Multi-Monitor Selector Helper ----
  function populateMonitorSelect(monitors, activeIndex) {
    const sel = $('viewerMonitor');
    sel.innerHTML = '';
    const list = Array.isArray(monitors) ? monitors : [];
    if (list.length === 0) {
      sel.style.display = 'none';
      return;
    }
    sel.style.display = 'inline-block';
    for (const m of list) {
      const opt = document.createElement('option');
      opt.value = String(m.index);
      opt.style.color = '#000';
      const pri = m.isPrimary ? '主屏 ' : '';
      opt.textContent = `🖥 屏幕${m.index + 1} (${pri}${m.width}×${m.height})`;
      if (m.index === activeIndex) opt.selected = true;
      sel.appendChild(opt);
    }
  }

  $('viewerMonitor').addEventListener('change', async () => {
    if (!connected || !isTauri || !invoke) return;
    const idx = Number($('viewerMonitor').value) || 0;
    try {
      const ack = await invoke('remote_select_monitor', { index: idx });
      if (ack && ack.ok) {
        origW = ack.width;
        origH = ack.height;
        streamW = ack.width;
        streamH = ack.height;
        populateMonitorSelect(ack.monitors, ack.activeMonitor);
        toast(`已切换至屏幕 ${ack.activeMonitor + 1} (${ack.width}×${ack.height})`, 'ok');
        log(`远端显示器已切换: index=${ack.activeMonitor} (${ack.width}x${ack.height})`);
      }
    } catch (e) {
      toast('切换显示器失败: ' + e, 'err');
    }
    cvs.focus();
  });

  // ---- Remote Cursor Shape + Floating Indicator ----
  function handleRemoteCursor(cur) {
    if (!connected) return;
    if (!cur.visible) {
      cvs.style.cursor = 'none';
      $('remoteCursorDot').style.display = 'none';
      return;
    }
    cvs.style.cursor = CURSOR_CSS_MAP[cur.shape] || 'default';
    const dot = $('remoteCursorDot');
    dot.style.display = 'block';
    dot.style.opacity = '0.9';
    dot.style.left = `${((cur.normX / 65535) * 100).toFixed(2)}%`;
    dot.style.top = `${((cur.normY / 65535) * 100).toFixed(2)}%`;
    if (cursorHideTimer) clearTimeout(cursorHideTimer);
    cursorHideTimer = setTimeout(() => {
      dot.style.opacity = '0';
    }, 1400);
  }

  // ---- Zero-Base64 Binary IPC Channel + Off-Main-Thread createImageBitmap Renderer ----
  let renderChain = Promise.resolve();
  const snapCanvas = document.createElement('canvas');
  const snapCtx = snapCanvas.getContext('2d');

  async function decodeBlobTile(blob, x, y, w, h) {
    if (typeof createImageBitmap === 'function') {
      try {
        const bmp = await createImageBitmap(blob);
        return { bmp, isBitmap: true, x, y, w, h };
      } catch {
        // fall through to object URL Image fallback
      }
    }
    return new Promise((resolve) => {
      const url = URL.createObjectURL(blob);
      const img = new Image();
      img.onload = () => {
        URL.revokeObjectURL(url);
        resolve({ bmp: img, isBitmap: false, x, y, w, h });
      };
      img.onerror = () => {
        URL.revokeObjectURL(url);
        resolve(null);
      };
      img.src = url;
    });
  }

  async function decodeTileImage(t) {
    const u8 = base64ToUint8(t.dataBase64);
    const blob = new Blob([u8], { type: t.mime });
    return decodeBlobTile(blob, t.x, t.y, t.w, t.h);
  }

  function enqueueTileBatchRender(screenW, screenH, isKeyFrame, hasLossless, byteLen, decodeAllFn) {
    if (!connected) return;
    secFrames++;
    secBytes += byteLen || 0;
    streamW = screenW;
    streamH = screenH;
    if (isKeyFrame) losslessState = false;
    if (hasLossless) losslessState = true;

    renderChain = renderChain.then(async () => {
      if (!connected) return;
      if (cvs.width !== screenW || cvs.height !== screenH) {
        const hadPixels = cvs.width > 0 && cvs.height > 0 && !isKeyFrame;
        if (hadPixels) {
          snapCanvas.width = cvs.width;
          snapCanvas.height = cvs.height;
          snapCtx.drawImage(cvs, 0, 0);
        }
        cvs.width = screenW;
        cvs.height = screenH;
        if (hadPixels) {
          ctx.drawImage(snapCanvas, 0, 0, screenW, screenH);
        }
      }
      const decoded = await decodeAllFn();
      for (const item of decoded) {
        if (item) {
          ctx.drawImage(item.bmp, item.x, item.y, item.w, item.h);
          if (item.isBitmap && typeof item.bmp.close === 'function') {
            item.bmp.close();
          }
        }
      }
      updateHud();
    }).catch(() => {});
  }

  function handleBinaryTileFrame(rawMsg) {
    if (!connected) return;
    let u8 = null;
    if (rawMsg instanceof ArrayBuffer) {
      u8 = new Uint8Array(rawMsg);
    } else if (ArrayBuffer.isView(rawMsg)) {
      u8 = new Uint8Array(rawMsg.buffer, rawMsg.byteOffset, rawMsg.byteLength);
    } else if (Array.isArray(rawMsg)) {
      u8 = new Uint8Array(rawMsg);
    }
    if (!u8 || u8.byteLength < 25) return;

    const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    const hdrFlags = view.getUint8(0);
    const screenW = view.getUint16(13, true);
    const screenH = view.getUint16(15, true);
    const count = view.getUint16(19, true);
    const isKeyFrame = (hdrFlags & 1) !== 0;
    const hasLossless = (hdrFlags & 2) !== 0;
    const byteLen = u8.byteLength - 1;

    const tileSlices = [];
    let pos = 25;
    for (let i = 0; i < count; i++) {
      if (pos + 14 > u8.byteLength) break;
      const x = view.getUint16(pos, true);
      const y = view.getUint16(pos + 2, true);
      const w = view.getUint16(pos + 4, true);
      const h = view.getUint16(pos + 6, true);
      const enc = view.getUint8(pos + 8);
      const len = view.getUint32(pos + 10, true);
      pos += 14;
      if (pos + len > u8.byteLength) break;
      const mime = enc === 2 ? 'image/png' : 'image/jpeg';
      const bytes = u8.subarray(pos, pos + len);
      tileSlices.push({ bytes, mime, x, y, w, h });
      pos += len;
    }

    enqueueTileBatchRender(screenW, screenH, isKeyFrame, hasLossless, byteLen, () =>
      Promise.all(tileSlices.map((s) => decodeBlobTile(new Blob([s.bytes], { type: s.mime }), s.x, s.y, s.w, s.h)))
    );
  }

  function createFrameStreamChannel() {
    const ChannelCtor = getTauriChannelCtor();
    if (!ChannelCtor) return null;
    const ch = new ChannelCtor();
    ch.onmessage = (msg) => {
      handleBinaryTileFrame(msg);
    };
    return ch;
  }

  function handleTilePushEvent(ev) {
    enqueueTileBatchRender(
      ev.screenW,
      ev.screenH,
      ev.isKeyFrame,
      ev.hasLossless,
      ev.byteLen || 0,
      () => Promise.all((ev.tiles || []).map(decodeTileImage))
    );
  }

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

  function releaseAllRemoteModifiers() {
    if (!isTauri || !invoke || heldRemoteVks.size === 0) return;
    for (const vk of Array.from(heldRemoteVks)) {
      invoke('remote_key_event', { action: 1, vk, flags: 0, unicodeChar: 0 }).catch(() => {});
    }
    heldRemoteVks.clear();
  }

  window.addEventListener('blur', releaseAllRemoteModifiers);

  function applyConnectedInfo(info) {
    const ack = info.ack;
    origW = ack.width;
    origH = ack.height;
    streamW = ack.width;
    streamH = ack.height;
    remoteOs = (ack.codecProbe && ack.codecProbe.os) ? String(ack.codecProbe.os) : '';

    populateMonitorSelect(ack.monitors || [], ack.activeMonitor || 0);

    ctrlCmdMapEnabled = Boolean(remoteOs && ((localOs === 'macos') !== (remoteOs === 'macos')));
    updateKeyMapBtn();

    $('e2eeBadge').style.display = info.encrypted ? 'inline-block' : 'none';
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
  }

  async function handleUnexpectedDisconnect(reason) {
    if (!connected || reconnecting || !isTauri || !invoke || !lastConnectParams) return;
    releaseAllRemoteModifiers();
    reconnecting = true;
    const myReconn = ++reconnToken;
    const delays = [1200, 2400, 4500, 8000, 12000];
    $('reconnBanner').style.display = 'inline-flex';
    log(`远端连接中断 (${reason || 'network'})，启动自动重连...`);

    for (let i = 0; i < delays.length; i++) {
      if (!reconnecting || reconnToken !== myReconn) return;
      $('reconnText').textContent = `🔄 连接中断，正在第 ${i + 1}/${delays.length} 次自动重连...`;
      await new Promise((r) => setTimeout(r, delays[i]));
      if (!reconnecting || reconnToken !== myReconn) return;
      try {
        const { maxWidth, quality } = currentQuality();
        const params = { ...lastConnectParams, initialMaxWidth: maxWidth, initialQuality: quality };
        const frameChannel = createFrameStreamChannel();
        const info = await invoke('connect_remote', { params, frameChannel });
        if (!reconnecting || reconnToken !== myReconn) return;
        applyConnectedInfo(info);
        reconnecting = false;
        $('reconnBanner').style.display = 'none';
        log(`自动重连成功 [${info.route} -> ${info.endpoint}]`);
        toast('已自动恢复远控连接', 'ok');
        cvs.focus();
        return;
      } catch {
        // continue backoff
      }
    }

    if (reconnecting && reconnToken === myReconn) {
      reconnecting = false;
      $('reconnBanner').style.display = 'none';
      toast('自动重连失败，远端可能已关闭或更换密码', 'err');
      $('btnDisconnect').click();
    }
  }

  $('btnCancelReconn').addEventListener('click', () => {
    reconnecting = false;
    reconnToken++;
    $('reconnBanner').style.display = 'none';
    $('btnDisconnect').click();
  });

  $('btnConnect').addEventListener('click', async () => {
    reconnecting = false;
    reconnToken++;
    $('reconnBanner').style.display = 'none';
    webLoopToken++;
    connected = false;
    invoke = getTauriInvoke();
    isTauri = Boolean(invoke);
    saveSettings();
    $('btnConnect').disabled = true;
    $('btnConnect').textContent = '密钥协商中...';
    try {
      autoTier = '1280:68';
      const { maxWidth, quality } = currentQuality();
      const useRelay = connMode === 'relay';
      const targetStr = useRelay
        ? ($('connDevId').value.replace(/\s+/g, '') || '888100200')
        : ($('connDirect').value.trim() || '127.0.0.1:19800');

      if (isTauri) {
        const relayAddr = $('hostRelay').value.trim() || '38.34.13.214:19901';
        const params = {
          directAddr: useRelay ? null : targetStr,
          relayAddr: useRelay ? relayAddr : null,
          deviceId: useRelay ? targetStr : null,
          token: $('connToken').value,
          clientName: `CopyToDesk-${localOs}`,
          initialMaxWidth: maxWidth,
          initialQuality: quality,
        };
        const frameChannel = createFrameStreamChannel();
        const info = await invoke('connect_remote', { params, frameChannel });
        lastConnectParams = params;
        applyConnectedInfo(info);
        recordSavedDevice(connMode, targetStr, $('connToken').value, remoteOs);
        const ack = info.ack;
        log(`E2EE 连接成功 [${info.route} -> ${info.endpoint}]: 远端=${remoteOs || 'unknown'} ${ack.captureBackend} ${ack.width}x${ack.height} (${(ack.monitors || []).length}屏)`);
        toast(info.route === 'lan-p2p' ? `🔒 E2EE 局域网直连 (${info.endpoint})` : `🔒 E2EE 已连接 (${ack.inputBackend})`, 'ok');
      } else {
        recordSavedDevice(connMode, targetStr, $('connToken').value, 'web');
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
      $('canvasWrap').style.display = 'inline-flex';
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
    reconnecting = false;
    reconnToken++;
    lastConnectParams = null;
    $('reconnBanner').style.display = 'none';
    releaseAllRemoteModifiers();
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
    }
    webLoopToken++;
    connected = false;
    $('connNavDot').className = 'nav-dot';
    $('e2eeBadge').style.display = 'none';
    $('routeBadge').style.display = 'none';
    $('viewerMonitor').style.display = 'none';
    $('remoteCursorDot').style.display = 'none';
    $('hud').textContent = '已断开';
    $('canvasWrap').style.display = 'none';
    $('emptyStage').style.display = 'flex';
    switchTab('home');
    toast('已断开远程连接');
    if (isTauri && invoke) {
      invoke('disconnect_remote').catch(() => {});
    }
  });

  // ---- Scale Mode & Fullscreen & System Combo Shortcuts ----
  $('btnScaleMode').addEventListener('click', () => {
    scaleOneToOne = !scaleOneToOne;
    $('canvasWrap').classList.toggle('scale-1to1', scaleOneToOne);
    $('btnScaleMode').classList.toggle('active', scaleOneToOne);
    $('btnScaleMode').textContent = scaleOneToOne ? '🔍 1:1 原寸' : '🔍 自适应';
    cvs.focus();
  });

  $('btnFullscreen').addEventListener('click', async () => {
    const page = $('tab-viewer');
    try {
      if (!document.fullscreenElement) {
        await page.requestFullscreen();
      } else {
        await document.exitFullscreen();
      }
    } catch {
      // ignore if unsupported
    }
    cvs.focus();
  });

  document.addEventListener('fullscreenchange', () => {
    const isFs = Boolean(document.fullscreenElement);
    $('btnFullscreen').classList.toggle('active', isFs);
    $('btnFullscreen').textContent = isFs ? '⛶ 退出全屏' : '⛶ 全屏';
    $('viewerBar').classList.remove('fs-open');
  });

  $('fsHoverTab').addEventListener('mouseenter', () => {
    $('viewerBar').classList.add('fs-open');
  });
  $('tab-viewer').addEventListener('mousemove', (ev) => {
    if (!document.fullscreenElement) return;
    if (ev.clientY <= 8) {
      $('viewerBar').classList.add('fs-open');
    } else if (ev.clientY > 64 && !$('viewerBar').matches(':hover') && !$('viewerBar').matches(':focus-within')) {
      $('viewerBar').classList.remove('fs-open');
    }
  });

  $('btnScrollDir').addEventListener('click', () => {
    invertScroll = !invertScroll;
    updateScrollDirBtn();
    saveSettings();
    toast(invertScroll ? '已切换为反向滚动（macOS 自然滚动风格）' : '已切换为正向滚动');
    cvs.focus();
  });

  $('comboSelect').addEventListener('change', async () => {
    const raw = $('comboSelect').value;
    $('comboSelect').value = '';
    if (!raw || !connected || !isTauri || !invoke) return;

    let combo = raw;
    if (raw === 'show_desktop') {
      combo = remoteOs === 'macos' ? 'show_desktop_mac' : 'show_desktop_win';
    } else if (raw === 'app_switch') {
      combo = remoteOs === 'macos' ? 'app_switch_mac' : 'app_switch_win';
    } else if (raw === 'lock_screen') {
      combo = remoteOs === 'macos' ? 'lock_mac' : 'lock_win';
    }

    try {
      await invoke('remote_send_combo', { combo });
      toast('已发送远程组合键: ' + combo, 'ok');
    } catch (e) {
      toast('发送组合键失败: ' + e, 'err');
    }
    cvs.focus();
  });

  $('btnKeyMap').addEventListener('click', () => {
    releaseAllRemoteModifiers();
    ctrlCmdMapEnabled = !ctrlCmdMapEnabled;
    updateKeyMapBtn();
    toast(ctrlCmdMapEnabled ? '已开启 Ctrl ↔ Command 智能快捷键映射' : '已关闭 Ctrl ↔ Command 映射');
    cvs.focus();
  });

  $('btnQuickClipSync').addEventListener('click', async () => {
    if (!connected || !isTauri || !invoke) return;
    try {
      const remoteTxt = await invoke('remote_clipboard_get');
      if (remoteTxt) {
        await invoke('local_clipboard_set', { text: remoteTxt });
        $('clipBox').value = remoteTxt;
        toast('已将远端剪贴板同步到本机', 'ok');
      } else {
        toast('远端剪贴板当前为空');
      }
    } catch (e) {
      toast('同步剪贴板失败: ' + e, 'err');
    }
    cvs.focus();
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
    if (now - lastMoveSent < 25) return;
    lastMoveSent = now;
    const { nx, ny } = mapNormXY(ev);
    sendMouseNorm(0, 0, nx, ny, 0);
  });

  cvs.addEventListener('wheel', (ev) => {
    if (!connected || !isTauri) return;
    ev.preventDefault();
    const { nx, ny } = mapNormXY(ev);
    let rawDy = ev.deltaY;
    if (ev.deltaMode === 1) rawDy *= 40;
    else if (ev.deltaMode === 2) rawDy *= 360;
    if (rawDy === 0) return;
    let delta = Math.round((invertScroll ? rawDy : -rawDy) * 1.2);
    if (delta === 0) delta = (invertScroll ? rawDy : -rawDy) > 0 ? 15 : -15;
    delta = Math.max(-600, Math.min(600, delta));
    sendMouseNorm(3, 0, nx, ny, delta);
  }, { passive: false });

  // ---- Full Developer Cross-OS Keyboard + Seamless Clipboard Sync ----
  const SPECIAL_VK = {
    Backspace: 0x08, Tab: 0x09, Enter: 0x0d, CapsLock: 0x14, Escape: 0x1b, ' ': 0x20,
    PageUp: 0x21, PageDown: 0x22, End: 0x23, Home: 0x24,
    ArrowLeft: 0x25, ArrowUp: 0x26, ArrowRight: 0x27, ArrowDown: 0x28,
    Insert: 0x2d, Delete: 0x2e, Shift: 0x10, Control: 0x11, Alt: 0x12, Meta: 0x5b,
    F1: 0x70, F2: 0x71, F3: 0x72, F4: 0x73, F5: 0x74, F6: 0x75,
    F7: 0x76, F8: 0x77, F9: 0x78, F10: 0x79, F11: 0x7a, F12: 0x7b,
  };

  const CODE_TO_OEM_VK = {
    Semicolon: 0xba,
    Equal: 0xbb,
    Comma: 0xbc,
    Minus: 0xbd,
    Period: 0xbe,
    Slash: 0xbf,
    Backquote: 0xc0,
    BracketLeft: 0xdb,
    Backslash: 0xdc,
    BracketRight: 0xdd,
    Quote: 0xde,
  };

  function resolveShortcutVk(ev) {
    if (CODE_TO_OEM_VK[ev.code]) return CODE_TO_OEM_VK[ev.code];
    if (ev.code && ev.code.startsWith('Key') && ev.code.length === 4) {
      return ev.code.charCodeAt(3); // 'A'..'Z' (0x41..0x5A)
    }
    if (ev.code && ev.code.startsWith('Digit') && ev.code.length === 6) {
      return ev.code.charCodeAt(5); // '0'..'9' (0x30..0x39)
    }
    if (ev.key && ev.key.length === 1) {
      return ev.key.toUpperCase().charCodeAt(0);
    }
    return 0;
  }

  function computeRemoteModifiers(ev) {
    let ctrl = ev.ctrlKey;
    let meta = ev.metaKey;
    if (ctrlCmdMapEnabled) {
      ctrl = ev.metaKey;
      meta = ev.ctrlKey;
    }
    let flags = 0;
    if (ev.shiftKey) flags |= KEY_MOD_SHIFT;
    if (ctrl) flags |= KEY_MOD_CTRL;
    if (ev.altKey) flags |= KEY_MOD_ALT;
    if (meta) flags |= KEY_MOD_META;
    return { ctrl, meta, flags };
  }

  function mapModifierVk(key) {
    if (ctrlCmdMapEnabled) {
      if (key === 'Control') return 0x5b;
      if (key === 'Meta') return 0x11;
    }
    return SPECIAL_VK[key] || 0;
  }

  cvs.addEventListener('keydown', async (ev) => {
    if (!connected || !isTauri || !invoke) return;
    // Allow Escape to exit fullscreen naturally when in fullscreen
    if (ev.key === 'Escape' && document.fullscreenElement) return;
    ev.preventDefault();

    const { ctrl, meta, flags } = computeRemoteModifiers(ev);

    if (ev.key === 'Control' || ev.key === 'Meta' || ev.key === 'Shift' || ev.key === 'Alt') {
      const mvk = mapModifierVk(ev.key);
      if (mvk) {
        heldRemoteVks.add(mvk);
        invoke('remote_key_event', { action: 0, vk: mvk, flags, unicodeChar: 0 }).catch(() => {});
      }
      return;
    }

    if (SPECIAL_VK[ev.key]) {
      invoke('remote_key_event', { action: 0, vk: SPECIAL_VK[ev.key], flags, unicodeChar: 0 }).catch(() => {});
      return;
    }

    if (ev.key.length === 1 && !ctrl && !meta && !ev.altKey) {
      invoke('remote_key_event', { action: 2, vk: 0, flags: 0, unicodeChar: ev.key.codePointAt(0) }).catch(() => {});
      return;
    }

    const vkCode = resolveShortcutVk(ev);
    if (vkCode) {
      const upper = ev.key.length === 1 ? ev.key.toUpperCase() : '';
      const isPrimaryShortcut = (remoteOs === 'macos') ? meta : ctrl;

      if (isPrimaryShortcut && upper === 'V' && !ev.repeat) {
        try {
          const localTxt = await invoke('local_clipboard_get');
          if (localTxt) {
            await invoke('remote_clipboard_set', { text: localTxt });
          } else {
            const localPng = await invoke('local_clipboard_get_image');
            if (localPng) {
              await invoke('remote_clipboard_set_image', { pngBase64: localPng });
            }
          }
        } catch {
          // continue key injection
        }
      }

      invoke('remote_key_event', { action: 0, vk: vkCode, flags, unicodeChar: 0 }).catch(() => {});

      if (isPrimaryShortcut && (upper === 'C' || upper === 'X') && !ev.repeat) {
        setTimeout(async () => {
          try {
            const remoteTxt = await invoke('remote_clipboard_get');
            if (remoteTxt) {
              await invoke('local_clipboard_set', { text: remoteTxt });
              $('clipBox').value = remoteTxt;
            } else {
              const remotePng = await invoke('remote_clipboard_get_image');
              if (remotePng) {
                await invoke('local_clipboard_set_image', { pngBase64: remotePng });
              }
            }
          } catch {
            // ignore
          }
        }, 160);
      }
    }
  });

  cvs.addEventListener('keyup', (ev) => {
    if (!connected || !isTauri || !invoke) return;
    ev.preventDefault();

    const { flags } = computeRemoteModifiers(ev);

    if (ev.key === 'Control' || ev.key === 'Meta' || ev.key === 'Shift' || ev.key === 'Alt') {
      const mvk = mapModifierVk(ev.key);
      if (mvk) {
        heldRemoteVks.delete(mvk);
        invoke('remote_key_event', { action: 1, vk: mvk, flags, unicodeChar: 0 }).catch(() => {});
      }
      return;
    }

    if (SPECIAL_VK[ev.key]) {
      invoke('remote_key_event', { action: 1, vk: SPECIAL_VK[ev.key], flags, unicodeChar: 0 }).catch(() => {});
    } else {
      const vkCode = resolveShortcutVk(ev);
      if (vkCode) {
        invoke('remote_key_event', { action: 1, vk: vkCode, flags, unicodeChar: 0 }).catch(() => {});
      }
    }
  });

  $('btnSendText').addEventListener('click', async () => {
    const text = $('typeInput').value;
    if (!text) return;
    if (isTauri) {
      await invoke('remote_type', { text });
      $('typeInput').value = '';
      toast('已发送文本', 'ok');
      cvs.focus();
    } else {
      toast('浏览器模式请在「剪贴板与运维」推送文本');
    }
  });
  $('typeInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') $('btnSendText').click();
  });

  // ---- Native System Clipboard Tab ----
  $('btnReadLocalClip').addEventListener('click', async () => {
    try {
      const txt = isTauri
        ? await invoke('local_clipboard_get')
        : (navigator.clipboard ? await navigator.clipboard.readText() : '');
      $('clipBox').value = txt || '';
      toast('已读取本机系统剪贴板', 'ok');
    } catch (e) {
      toast('读取本机剪贴板失败: ' + e, 'err');
    }
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
      if (isTauri && txt) {
        await invoke('local_clipboard_set', { text: txt });
      }
      toast('已拉取远端剪贴板并同步到本机系统', 'ok');
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
      toast('已写入远端操作系统剪贴板', 'ok');
    } catch (e) {
      toast('剪贴板错误: ' + e, 'err');
    }
  });

  function showClipImagePreview(b64, label) {
    if (!b64) return;
    $('clipImgPreviewWrap').style.display = 'block';
    $('clipImgLabel').textContent = label;
    $('clipImgPreview').src = `data:image/png;base64,${b64}`;
  }

  $('btnClipImgGet').addEventListener('click', async () => {
    if (!isTauri || !invoke || !connected) {
      toast('请先在桌面端连接远端设备', 'err');
      return;
    }
    try {
      const b64 = await invoke('remote_clipboard_get_image');
      if (!b64) {
        toast('远端系统剪贴板当前没有图片/截图');
        return;
      }
      await invoke('local_clipboard_set_image', { pngBase64: b64 });
      showClipImagePreview(b64, '✓ 已从远端拉取并写入本机系统剪贴板');
      toast('已将远端截图同步到本机剪贴板（可直接 Ctrl+V 粘贴）', 'ok');
    } catch (e) {
      toast('拉取远端图片失败: ' + e, 'err');
    }
  });

  $('btnClipImgSet').addEventListener('click', async () => {
    if (!isTauri || !invoke || !connected) {
      toast('请先在桌面端连接远端设备', 'err');
      return;
    }
    try {
      const b64 = await invoke('local_clipboard_get_image');
      if (!b64) {
        toast('本机系统剪贴板当前没有图片（可先截图后再试）');
        return;
      }
      const bytes = await invoke('remote_clipboard_set_image', { pngBase64: b64 });
      showClipImagePreview(b64, `✓ 已推送本机截图至远端剪贴板 (${formatBytes(bytes)})`);
      toast('本机截图已推送至远端系统剪贴板', 'ok');
    } catch (e) {
      toast('推送本机图片失败: ' + e, 'err');
    }
  });

  // ---- Chunked Streaming File Transfer + Recursive Folder Drop + Sandbox File Manager ----
  function requestCancelTransfer() {
    if (!transferActive) return;
    uploadCancelRequested = true;
    if (isTauri && invoke) {
      invoke('remote_cancel_transfer').catch(() => {});
    }
    toast('正在中止当前传输...');
  }

  function showTransferProgress(title, transferred, total, speedMBs, active = false) {
    transferActive = active;
    const box = $('transferProgressBox');
    box.style.display = 'block';
    $('btnCancelTransfer').style.display = active ? 'inline-flex' : 'none';
    const pct = total > 0 ? Math.min(100, Math.round((transferred / total) * 100)) : 100;
    $('transferTitle').textContent = title;
    const speedStr = speedMBs > 0 ? ` · ${speedMBs.toFixed(1)} MB/s` : '';
    const statText = `${pct}% (${formatBytes(transferred)} / ${formatBytes(total)}${speedStr})`;
    $('transferStat').textContent = statText;
    $('transferBar').style.width = `${pct}%`;

    const pill = $('viewerTransferPill');
    if (pill) {
      if (viewerPillHideTimer) {
        clearTimeout(viewerPillHideTimer);
        viewerPillHideTimer = null;
      }
      pill.style.display = 'inline-flex';
      $('viewerTransferText').textContent = `${title} · ${pct}%${speedStr}`;
      $('viewerCancelTransferBtn').style.display = active ? 'inline-flex' : 'none';
      if (!active) {
        viewerPillHideTimer = setTimeout(() => {
          pill.style.display = 'none';
        }, 3200);
      }
    }
  }

  $('btnCancelTransfer').addEventListener('click', requestCancelTransfer);
  $('viewerCancelTransferBtn').addEventListener('click', requestCancelTransfer);

  function joinRemotePath(dir, name) {
    const cleanDir = String(dir || '').replace(/\\/g, '/').replace(/^\/+|\/+$/g, '');
    const cleanName = String(name || '').replace(/\\/g, '/').replace(/^\/+/, '');
    return cleanDir ? `${cleanDir}/${cleanName}` : cleanName;
  }

  async function downloadRemoteEntry(relPath, fileName) {
    if (!connected) {
      toast('请先连接远端设备', 'err');
      return;
    }
    try {
      showTransferProgress(`⬇ 正在下载 ${fileName}`, 0, 1, 0, true);
      if (isTauri && invoke) {
        const t0 = performance.now();
        const res = await invoke('remote_download_file', { remotePath: relPath });
        const sec = Math.max(0.01, (performance.now() - t0) / 1000);
        const speed = (res.bytes / (1024 * 1024)) / sec;
        showTransferProgress(`✓ 下载完成 ${fileName}`, res.bytes, res.bytes, speed, false);
        log(`文件下载成功: ${res.localPath} (${res.bytes} B, SHA256=${res.sha256.slice(0, 16)}...)`);
        toast(`已保存至 ${res.localPath}`, 'ok');
      } else {
        const r = await fetch('/api/files', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            deviceId: $('connDevId').value.replace(/\s+/g, ''),
            token: $('connToken').value,
            action: 'download',
            remotePath: relPath,
          }),
        });
        const res = await r.json();
        if (!res.ok) throw new Error(res.error || 'Download failed');
        const bin = atob(res.contentBase64);
        const u8 = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
        const blob = new Blob([u8]);
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = fileName;
        a.click();
        URL.revokeObjectURL(url);
        showTransferProgress(`✓ 浏览器已下载 ${fileName}`, res.bytes, res.bytes, 0);
        toast(`已下载 ${fileName}`, 'ok');
      }
    } catch (e) {
      showTransferProgress(`✕ 下载已停止 (${fileName})`, 0, 1, 0, false);
      toast('下载中断: ' + e, 'err');
      log('下载中断: ' + e);
    }
  }

  async function runRemoteFileAction(action, relPath, newName = null) {
    if (!connected || !isTauri || !invoke) {
      toast('请先在桌面端连接远端设备', 'err');
      return false;
    }
    const raw = await invoke('remote_file_action', { action, remotePath: relPath, newName });
    const res = JSON.parse(raw);
    if (!res.ok) {
      toast(`操作失败: ${res.error}`, 'err');
      return false;
    }
    return true;
  }

  async function refreshRemoteDir() {
    const curDir = $('dirPath').value.trim();
    const raw = isTauri
      ? await invoke('remote_list_dir', { remotePath: curDir })
      : await (await fetch('/api/files', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            deviceId: $('connDevId').value.replace(/\s+/g, ''),
            token: $('connToken').value,
            action: 'list',
            remotePath: curDir,
          }),
        })).text();
    const res = JSON.parse(raw);
    const box = $('fileList');
    box.innerHTML = '';
    if (!res.ok) {
      box.textContent = res.error;
      return;
    }
    if ($('remoteRootHint')) {
      $('remoteRootHint').textContent = res.root ? `远端共享根目录：${res.root}` : '';
    }
    for (const f of res.entries || []) {
      const row = document.createElement('div');
      row.className = 'file-row';
      const entryRelPath = joinRemotePath(curDir, f.name);

      if (renamingFilePath === entryRelPath) {
        const inp = document.createElement('input');
        inp.className = 'input';
        inp.style.height = '28px';
        inp.style.fontSize = '12.5px';
        inp.value = f.name;
        const commitRename = async () => {
          const nextName = inp.value.trim();
          renamingFilePath = null;
          if (nextName && nextName !== f.name) {
            if (await runRemoteFileAction('rename', entryRelPath, nextName)) {
              toast(`已重命名为 ${nextName}`, 'ok');
            }
          }
          refreshRemoteDir().catch(() => {});
        };
        inp.addEventListener('keydown', (ev) => {
          if (ev.key === 'Enter') commitRename();
          if (ev.key === 'Escape') {
            renamingFilePath = null;
            refreshRemoteDir().catch(() => {});
          }
        });
        inp.addEventListener('blur', commitRename);
        row.appendChild(inp);
        setTimeout(() => inp.focus(), 10);
      } else {
        const nameBtn = document.createElement('button');
        nameBtn.type = 'button';
        nameBtn.className = 'file-name-btn' + (f.isDir ? ' dir' : '');
        nameBtn.textContent = `${f.isDir ? '📁 ' : '📄 '}${f.name}`;
        if (f.isDir) {
          nameBtn.addEventListener('click', () => {
            $('dirPath').value = entryRelPath;
            refreshRemoteDir().catch(() => {});
          });
        }
        row.appendChild(nameBtn);
      }

      const right = document.createElement('div');
      right.style.display = 'flex';
      right.style.alignItems = 'center';
      right.style.gap = '6px';
      right.style.flexShrink = '0';

      const metaSpan = document.createElement('span');
      metaSpan.style.color = 'var(--muted)';
      metaSpan.style.fontSize = '12px';
      metaSpan.textContent = f.isDir ? '目录' : formatBytes(f.size);
      right.appendChild(metaSpan);

      if (!f.isDir) {
        const dlBtn = document.createElement('button');
        dlBtn.type = 'button';
        dlBtn.className = 'btn xs ghost';
        dlBtn.textContent = '⬇ 下载';
        dlBtn.addEventListener('click', () => {
          downloadRemoteEntry(entryRelPath, f.name);
        });
        right.appendChild(dlBtn);
      }

      if (isTauri) {
        const rnBtn = document.createElement('button');
        rnBtn.type = 'button';
        rnBtn.className = 'icon-btn';
        rnBtn.style.width = '26px';
        rnBtn.style.height = '26px';
        rnBtn.title = '重命名';
        rnBtn.textContent = '✎';
        rnBtn.addEventListener('click', () => {
          renamingFilePath = entryRelPath;
          refreshRemoteDir().catch(() => {});
        });
        right.appendChild(rnBtn);

        const delBtn = document.createElement('button');
        delBtn.type = 'button';
        delBtn.className = 'icon-btn';
        delBtn.style.width = '26px';
        delBtn.style.height = '26px';
        delBtn.title = '删除';
        delBtn.textContent = '🗑';
        delBtn.addEventListener('click', async () => {
          if (await runRemoteFileAction('delete', entryRelPath, null)) {
            toast(`已删除 ${f.name}`, 'ok');
            refreshRemoteDir().catch(() => {});
          }
        });
        right.appendChild(delBtn);
      }

      row.appendChild(right);
      box.appendChild(row);
    }
    if (!res.entries || res.entries.length === 0) {
      box.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:12px 0">(当前共享目录为空)</div>';
    }
  }

  $('btnListDir').addEventListener('click', () => {
    refreshRemoteDir().catch((e) => toast('读取目录失败: ' + e, 'err'));
  });

  $('btnDirUp').addEventListener('click', () => {
    const cur = $('dirPath').value.trim().replace(/\\/g, '/').replace(/\/+$/g, '');
    if (!cur) return;
    const idx = cur.lastIndexOf('/');
    $('dirPath').value = idx >= 0 ? cur.slice(0, idx) : '';
    refreshRemoteDir().catch(() => {});
  });

  $('btnMkdir').addEventListener('click', async () => {
    const name = $('newFolderName').value.trim();
    if (!name) {
      toast('请先输入新文件夹名称');
      return;
    }
    const curDir = $('dirPath').value.trim();
    const rel = joinRemotePath(curDir, name);
    try {
      if (await runRemoteFileAction('mkdir', rel, null)) {
        $('newFolderName').value = '';
        toast(`已创建文件夹 ${name}`, 'ok');
        refreshRemoteDir().catch(() => {});
      }
    } catch (e) {
      toast('创建文件夹失败: ' + e, 'err');
    }
  });
  $('newFolderName').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') $('btnMkdir').click();
  });

  $('btnOpenDownloads').addEventListener('click', async () => {
    if (!isTauri || !invoke) {
      toast('浏览器模式文件直接保存至系统默认下载目录');
      return;
    }
    try {
      const p = await invoke('open_downloads_dir');
      toast('已打开下载目录: ' + p);
    } catch (e) {
      toast('打开目录失败: ' + e, 'err');
    }
  });

  async function extractDroppedEntries(dataTransfer) {
    const rawItems = dataTransfer && dataTransfer.items ? Array.from(dataTransfer.items) : [];
    const fallbackFiles = dataTransfer && dataTransfer.files ? Array.from(dataTransfer.files) : [];
    const rootEntries = rawItems
      .map((it) => (typeof it.webkitGetAsEntry === 'function' ? it.webkitGetAsEntry() : null))
      .filter(Boolean);

    if (rootEntries.length === 0) {
      return fallbackFiles.map((f) => ({ file: f, relPath: f.webkitRelativePath || f.name }));
    }

    const collected = [];
    async function walkEntry(entry, prefix) {
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isFile) {
        const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
        collected.push({ file, relPath: rel });
      } else if (entry.isDirectory) {
        const reader = entry.createReader();
        const children = [];
        while (true) {
          const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
          if (!batch || batch.length === 0) break;
          children.push(...batch);
        }
        if (children.length === 0) {
          collected.push({ emptyDir: rel });
        } else {
          for (const child of children) {
            await walkEntry(child, rel);
          }
        }
      }
    }

    for (const root of rootEntries) {
      await walkEntry(root, '');
    }
    return collected;
  }

  async function uploadEntryItems(items) {
    if (!items || items.length === 0) return;
    if (!connected) {
      toast('请先连接远端设备后再上传文件或文件夹', 'err');
      return;
    }
    uploadCancelRequested = false;
    const baseDir = $('upTargetDir').value.trim() || $('dirPath').value.trim();
    let uploadedFiles = 0;

    for (const item of items) {
      if (uploadCancelRequested) break;
      if (item.emptyDir) {
        if (isTauri && invoke) {
          const dirRel = joinRemotePath(baseDir, item.emptyDir);
          await invoke('remote_file_action', { action: 'mkdir', remotePath: dirRel, newName: null }).catch(() => {});
        }
        continue;
      }

      const file = item.file;
      const relName = item.relPath || file.webkitRelativePath || file.name;
      const remotePath = joinRemotePath(baseDir, relName);
      const total = file.size;
      const uploadId = `up-${randomHex(6)}`;
      const t0 = performance.now();

      try {
        showTransferProgress(`⬆ 正在上传 ${relName}`, 0, Math.max(1, total), 0, true);

        if (isTauri && invoke) {
          if (total === 0) {
            await invoke('remote_upload_chunk', {
              uploadId,
              remotePath,
              offset: 0,
              totalSize: 0,
              eof: true,
              chunkBase64: '',
            });
          } else {
            let offset = 0;
            while (offset < total) {
              if (uploadCancelRequested) {
                await invoke('remote_file_action', { action: 'cancelUpload', remotePath, newName: null }).catch(() => {});
                showTransferProgress(`✕ 已取消上传 ${relName}`, offset, total, 0, false);
                toast(`已取消上传 ${relName}`);
                return;
              }
              const end = Math.min(offset + FILE_CHUNK_BYTES, total);
              const eof = end >= total;
              const ab = await file.slice(offset, end).arrayBuffer();
              const b64 = uint8ToBase64(new Uint8Array(ab));
              await invoke('remote_upload_chunk', {
                uploadId,
                remotePath,
                offset,
                totalSize: total,
                eof,
                chunkBase64: b64,
              });
              offset = end;
              const sec = Math.max(0.01, (performance.now() - t0) / 1000);
              const speed = (offset / (1024 * 1024)) / sec;
              showTransferProgress(`⬆ 正在上传 ${relName}`, offset, total, speed, true);
            }
          }
          const sec = Math.max(0.01, (performance.now() - t0) / 1000);
          const speed = (total / (1024 * 1024)) / sec;
          showTransferProgress(`✓ 上传成功 ${relName} (SHA-256 已校验)`, total, Math.max(1, total), speed, false);
          log(`分块上传完成: ${remotePath} (${formatBytes(total)})`);
          uploadedFiles++;
        } else {
          const ab = await file.arrayBuffer();
          const b64 = uint8ToBase64(new Uint8Array(ab));
          const r = await fetch('/api/files', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({
              deviceId: $('connDevId').value.replace(/\s+/g, ''),
              token: $('connToken').value,
              action: 'upload',
              remotePath,
              contentBase64: b64,
            }),
          });
          const res = await r.json();
          if (!res.ok) throw new Error(res.error || 'Upload failed');
          showTransferProgress(`✓ 上传成功 ${relName}`, total, Math.max(1, total), 0, false);
          uploadedFiles++;
        }
      } catch (e) {
        showTransferProgress(`✕ 上传中断 (${relName})`, 0, 1, 0, false);
        toast(`上传 ${relName} 失败: ` + e, 'err');
        log(`上传失败 (${relName}): ` + e);
        return;
      }
    }

    if (uploadedFiles > 0) {
      toast(uploadedFiles === 1 ? '文件上传完成 (SHA-256 已校验)' : `已完成 ${uploadedFiles} 个文件递归上传`, 'ok');
    }
    refreshRemoteDir().catch(() => {});
  }

  async function uploadFileList(files) {
    if (!files || files.length === 0) return;
    const items = Array.from(files).map((f) => ({ file: f, relPath: f.webkitRelativePath || f.name }));
    await uploadEntryItems(items);
  }

  $('btnPickFiles').addEventListener('click', (ev) => {
    ev.stopPropagation();
    $('filePicker').click();
  });
  $('btnPickFolder').addEventListener('click', (ev) => {
    ev.stopPropagation();
    $('folderPicker').click();
  });
  $('dropZone').addEventListener('click', () => $('filePicker').click());
  $('filePicker').addEventListener('change', () => {
    if ($('filePicker').files && $('filePicker').files.length > 0) {
      uploadFileList($('filePicker').files);
      $('filePicker').value = '';
    }
  });
  $('folderPicker').addEventListener('change', () => {
    if ($('folderPicker').files && $('folderPicker').files.length > 0) {
      uploadFileList($('folderPicker').files);
      $('folderPicker').value = '';
    }
  });

  for (const zone of [$('dropZone'), $('viewerStage')]) {
    if (!zone) continue;
    zone.addEventListener('dragover', (ev) => {
      ev.preventDefault();
      if (zone === $('viewerStage') && !connected) return;
      zone.classList.add('dragover');
    });
    zone.addEventListener('dragleave', (ev) => {
      if (!zone.contains(ev.relatedTarget)) {
        zone.classList.remove('dragover');
      }
    });
    zone.addEventListener('drop', async (ev) => {
      ev.preventDefault();
      zone.classList.remove('dragover');
      if (!ev.dataTransfer) return;
      try {
        const items = await extractDroppedEntries(ev.dataTransfer);
        if (items.length > 0) {
          await uploadEntryItems(items);
        }
      } catch (e) {
        toast('读取拖拽文件失败: ' + e, 'err');
      }
    });
  }

  $('btnUpload').addEventListener('click', async () => {
    try {
      const b64 = btoa(unescape(encodeURIComponent($('upContent').value)));
      const raw = isTauri
        ? await invoke('remote_upload_base64', { remotePath: $('upName').value, contentBase64: b64 })
        : await (await fetch('/api/files', {
            method: 'POST',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({
              deviceId: $('connDevId').value.replace(/\s+/g, ''),
              token: $('connToken').value,
              action: 'upload',
              remotePath: $('upName').value,
              contentBase64: b64,
            }),
          })).text();
      const res = JSON.parse(raw);
      if (res.ok) {
        toast(`文本已保存 (${res.bytes} B, SHA-256 已校验)`, 'ok');
        refreshRemoteDir().catch(() => {});
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
            body: JSON.stringify({
              deviceId: $('connDevId').value.replace(/\s+/g, ''),
              token: $('connToken').value,
              command: $('execCmd').value,
            }),
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
    updateKeyMapBtn();
    updateScrollDirBtn();
    renderSavedBook();
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
        listen('remote-cursor', (event) => handleRemoteCursor(event.payload));
        listen('remote-rtt', (event) => {
          lastRttMs = Number(event.payload) || 0;
          updateHud();
        });
        listen('remote-disconnected', (event) => {
          handleUnexpectedDisconnect(event.payload);
        });
        listen('remote-clipboard-push', async (event) => {
          if (!connected || !$('hostAutoClipSync').checked) return;
          const txt = typeof event.payload === 'string' ? event.payload : '';
          if (!txt) return;
          $('clipBox').value = txt;
          await invoke('local_clipboard_set', { text: txt }).catch(() => {});
          log(`剪贴板已无感同步自远端 (${txt.length} 字符)`);
        });
        listen('file-transfer-progress', (event) => {
          const p = event.payload || {};
          if (p.direction === 'download') {
            showTransferProgress(`⬇ 正在下载 ${p.name}`, p.transferred || 0, p.total || 1, 0, true);
          }
        });
      }
      try {
        await invoke('set_close_to_tray', { enabled: $('hostCloseToTray').checked }).catch(() => {});
        const info = await invoke('get_platform_info');
        localOs = info.os || localOs;
        updateKeyMapBtn();
        if (typeof info.osAutoStart === 'boolean') {
          $('hostOsAutoStart').checked = info.osAutoStart;
        }
        if (info.sharedDir && !$('hostFileRoot').value) {
          $('hostFileRoot').placeholder = `默认持久化至 ${info.sharedDir}`;
        }
        $('modeSubtitle').textContent = `Tauri 2 · ${info.os}/${info.arch}`;
        $('engineDot').className = 'dot ok';
        $('engineText').textContent = `原生引擎就绪 v${info.version}`;
        log(`Tauri 2 引擎就绪 (${info.os}/${info.arch} v${info.version})`);
        const bootMinimized = Boolean(info.launchedMinimized);
        if ((bootMinimized || $('hostAutoStart').checked) && !hostRunning) {
          log(bootMinimized ? '开机静默自启：正在后台开启被控服务...' : '检测到已勾选「启动时自动开启被控服务」，正在自动启动...');
          await startHostService(bootMinimized);
        }
        if (bootMinimized) {
          await invoke('hide_to_tray').catch(() => {});
        }
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
