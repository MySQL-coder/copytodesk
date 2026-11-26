//! Cross-platform async Host service (v0.5.0 P2):
//! - Dual-password E2EE (`token` temporary + optional `unattended_token` permanent password)
//! - Multi-monitor enumeration & live hot-switching (`MonitorSelect` / `MonitorSelectAck`)
//! - Real-time delta-only `CursorState` push (`ChannelId::Cursor`)
//! - Active session audit list + instant `kick_viewers()` owner override
//! - Anti-hijack Relay ownership proof + sliding-window brute-force limiter + chunked file transfer

use crate::capture::{create_capturer, now_us};
use crate::clipboard;
use crate::codec::{downsample_rgba_crisp, encode_full_screenshot, CompoundTileEncoder};
use crate::cursor;
use crate::input::{create_injector, InputInjector};
use ctd_protocol::{
    read_frame, read_frame_opt_cipher, write_frame, write_frame_opt_cipher, ChannelId,
    CursorStatePacket, DownloadChunkReq, FileActionPayload, HelloAckPayload, HelloPayload,
    KeyEventPacket, MonitorInfo, MonitorSelectAckPayload, MouseEventPacket, MsgType, SendCipher,
    ServerE2eeHandshake, UploadChunkMeta, FILE_CHUNK_SIZE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
use tokio::time::{sleep, Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub bind_addr: String,
    pub token: String,
    #[serde(default)]
    pub unattended_token: String,
    pub device_id: String,
    #[serde(default)]
    pub device_secret: String,
    pub device_name: String,
    pub relay_addr: Option<String>,
    pub fps: u32,
    pub jpeg_quality: u8,
    pub force_synthetic: bool,
    pub file_root: PathBuf,
    #[serde(default)]
    pub allow_input: bool,
    pub allow_exec: bool,
    pub exec_allowlist: Vec<String>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:19800".to_string(),
            token: "demo123".to_string(),
            unattended_token: String::new(),
            device_id: "888100200".to_string(),
            device_secret: String::new(),
            device_name: std::env::consts::OS.to_string(),
            relay_addr: Some("38.34.13.214:19901".to_string()),
            fps: 25,
            jpeg_quality: 72,
            force_synthetic: false,
            file_root: PathBuf::new(),
            allow_input: true,
            allow_exec: false,
            exec_allowlist: vec!["echo".into(), "hostname".into(), "whoami".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub running: bool,
    pub device_id: String,
    pub bind_addr: String,
    pub lan_addrs: Vec<String>,
    pub relay_addr: Option<String>,
    pub relay_status: String,
    pub capture_backend: String,
    pub width: u32,
    pub height: u32,
    pub monitors: Vec<MonitorInfo>,
    pub active_monitor: u16,
    pub active_sessions: Vec<String>,
    pub exec_enabled: bool,
}

/// Sliding-window brute-force rate limiter (max 5 failed attempts in 60s -> 15s cooldown).
pub struct AuthRateLimiter {
    inner: std::sync::Mutex<(VecDeque<Instant>, Option<Instant>)>,
}

impl AuthRateLimiter {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new((VecDeque::new(), None)),
        }
    }

    pub fn is_locked_out(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        if let Some(until) = g.1 {
            if now < until {
                return true;
            }
            g.1 = None;
        }
        false
    }

    pub fn record_failure(&self) {
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        while let Some(&front) = g.0.front() {
            if now.duration_since(front) > Duration::from_secs(60) {
                g.0.pop_front();
            } else {
                break;
            }
        }
        g.0.push_back(now);
        if g.0.len() >= 5 {
            g.1 = Some(now + Duration::from_secs(15));
        }
    }

    pub fn record_success(&self) {
        let mut g = self.inner.lock().unwrap();
        g.0.clear();
        g.1 = None;
    }
}

fn constant_time_token_eq(a: &str, b: &str) -> bool {
    let ha = Sha256::digest(a.as_bytes());
    let hb = Sha256::digest(b.as_bytes());
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= ha[i] ^ hb[i];
    }
    diff == 0 && a.len() == b.len()
}

fn verify_host_token(provided: &str, cfg: &HostConfig) -> bool {
    if !cfg.token.is_empty() && constant_time_token_eq(provided, &cfg.token) {
        return true;
    }
    let perm = cfg.unattended_token.trim();
    if !perm.is_empty() && constant_time_token_eq(provided, perm) {
        return true;
    }
    false
}

fn compute_ownership_proof(device_id: &str, device_secret: &str) -> Option<String> {
    let trimmed = device_secret.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(b"CTD-OWN-v1:");
    h.update(device_id.as_bytes());
    h.update(b":");
    h.update(trimmed.as_bytes());
    Some(hex::encode(h.finalize()))
}

struct SharedState {
    cfg: HostConfig,
    lan_addrs: Vec<String>,
    relay_status: std::sync::RwLock<String>,
    auth_limiter: AuthRateLimiter,
    width: AtomicU32,
    height: AtomicU32,
    monitors: std::sync::RwLock<Vec<MonitorInfo>>,
    active_monitor: AtomicU16,
    requested_monitor: AtomicI32,
    active_sessions: std::sync::RwLock<HashMap<String, String>>,
    capture_backend: String,
    latest_rgba: RwLock<Vec<u8>>,
    clipboard: RwLock<String>,
    clipboard_png: RwLock<Vec<u8>>,
    injector: Arc<dyn InputInjector>,
    frame_tx: broadcast::Sender<(u8, Vec<u8>)>,
    cursor_tx: broadcast::Sender<[u8; 6]>,
    force_keyframe: AtomicBool,
    stream_max_w: AtomicU32,
    stream_quality: AtomicU8,
    running: AtomicBool,
    cap_thread: std::sync::OnceLock<std::thread::Thread>,
    stop_notify: Notify,
    kick_notify: Notify,
}

impl SharedState {
    fn wake_capturer(&self) {
        if let Some(t) = self.cap_thread.get() {
            t.unpark();
        }
    }
}

pub struct HostHandle {
    state: Arc<SharedState>,
}

fn discover_lan_endpoints(bind_addr: &str) -> Vec<String> {
    let port = bind_addr
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(19800);
    if bind_addr.starts_with("127.") {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(SocketAddr::V4(local)) = sock.local_addr() {
                let ip = local.ip();
                if !ip.is_loopback() && !ip.is_unspecified() {
                    out.push(format!("{ip}:{port}"));
                }
            }
        }
    }
    out
}

impl HostHandle {
    pub async fn start(mut cfg: HostConfig) -> Result<Self, String> {
        if cfg.file_root.as_os_str().is_empty() || cfg.file_root.is_relative() {
            cfg.file_root = std::env::temp_dir().join("CopyToDesk").join("shared-files");
        }
        std::fs::create_dir_all(&cfg.file_root)
            .map_err(|e| format!("create file_root {:?}: {}", cfg.file_root, e))?;

        let listener = TcpListener::bind(&cfg.bind_addr)
            .await
            .map_err(|e| format!("bind {}: {}", cfg.bind_addr, e))?;
        if cfg.bind_addr.ends_with(":0") {
            if let Ok(actual) = listener.local_addr() {
                cfg.bind_addr = actual.to_string();
            }
        }

        let lan_addrs = discover_lan_endpoints(&cfg.bind_addr);
        let mut capturer = create_capturer(cfg.force_synthetic);
        let (init_w, init_h) = capturer.dimensions();
        let init_monitors = capturer.monitors();
        let init_active_mon = capturer.active_monitor();
        let init_mon_info = capturer.active_monitor_info();
        let capture_backend = capturer.backend_name().to_string();

        let injector = create_injector(cfg.allow_input);
        injector.set_target_monitor(&init_mon_info);

        let (frame_tx, _) = broadcast::channel(3);
        let (cursor_tx, _) = broadcast::channel(16);

        let initial_relay_status = if cfg.relay_addr.as_deref().unwrap_or("").is_empty() {
            "disabled".to_string()
        } else {
            "connecting".to_string()
        };

        let state = Arc::new(SharedState {
            stream_max_w: AtomicU32::new(1280),
            stream_quality: AtomicU8::new(cfg.jpeg_quality.clamp(30, 92)),
            cfg: cfg.clone(),
            lan_addrs,
            relay_status: std::sync::RwLock::new(initial_relay_status),
            auth_limiter: AuthRateLimiter::new(),
            width: AtomicU32::new(init_w),
            height: AtomicU32::new(init_h),
            monitors: std::sync::RwLock::new(init_monitors),
            active_monitor: AtomicU16::new(init_active_mon),
            requested_monitor: AtomicI32::new(-1),
            active_sessions: std::sync::RwLock::new(HashMap::new()),
            capture_backend,
            latest_rgba: RwLock::new(vec![0u8; (init_w * init_h * 4) as usize]),
            clipboard: RwLock::new(String::new()),
            clipboard_png: RwLock::new(Vec::new()),
            injector,
            frame_tx,
            cursor_tx,
            force_keyframe: AtomicBool::new(true),
            running: AtomicBool::new(true),
            cap_thread: std::sync::OnceLock::new(),
            stop_notify: Notify::new(),
            kick_notify: Notify::new(),
        });

        // 1. Capture + Multi-Monitor Switch + Cursor Delta + Incremental Tile Encode loop
        let cap_state = state.clone();
        let cap_join = std::thread::spawn(move || {
            let interval = Duration::from_millis((1000 / cap_state.cfg.fps.max(1)).max(15) as u64);
            let mut cur_w = init_w;
            let mut cur_h = init_h;
            let mut cur_sw = 0u32;
            let mut cur_sh = 0u32;
            let mut cur_q = 0u8;
            let mut encoder = CompoundTileEncoder::new(cur_w, cur_h, 128, 72, 3);
            let mut raw_buf = vec![0u8; (cur_w * cur_h * 4) as usize];
            let mut scaled_buf: Vec<u8> = Vec::new();
            let mut last_cursor: Option<CursorStatePacket> = None;

            while cap_state.running.load(Ordering::Relaxed) {
                let t0 = std::time::Instant::now();

                // Check for pending monitor hot-switch request
                let req_mon = cap_state.requested_monitor.swap(-1, Ordering::AcqRel);
                if req_mon >= 0 {
                    if let Some(mon_info) = capturer.select_monitor(req_mon as u16) {
                        let (nw, nh) = capturer.dimensions();
                        cur_w = nw;
                        cur_h = nh;
                        raw_buf.resize((cur_w * cur_h * 4) as usize, 0);
                        cap_state.width.store(cur_w, Ordering::Release);
                        cap_state.height.store(cur_h, Ordering::Release);
                        cap_state
                            .active_monitor
                            .store(mon_info.index, Ordering::Release);
                        cap_state.injector.set_target_monitor(&mon_info);
                        if let Ok(mut mg) = cap_state.monitors.write() {
                            *mg = capturer.monitors();
                        }
                        cur_sw = 0; // trigger encoder reset + keyframe
                        last_cursor = None;
                        cap_state.force_keyframe.store(true, Ordering::Release);
                    }
                }

                let has_subscribers = cap_state.frame_tx.receiver_count() > 0;

                // Sample native OS cursor position & shape (only broadcast 6-byte delta when changed)
                if has_subscribers {
                    let active_mon_info = capturer.active_monitor_info();
                    if let Some(cur_pkt) = cursor::sample_cursor(&active_mon_info) {
                        if last_cursor != Some(cur_pkt) {
                            last_cursor = Some(cur_pkt);
                            let _ = cap_state.cursor_tx.send(cur_pkt.serialize());
                        }
                    }
                }

                if capturer.capture_rgba(&mut raw_buf) {
                    if let Ok(mut guard) = cap_state.latest_rgba.try_write() {
                        if guard.len() != raw_buf.len() {
                            *guard = raw_buf.clone();
                        } else {
                            guard.clone_from(&raw_buf);
                        }
                    }
                    if has_subscribers {
                        let max_w = cap_state.stream_max_w.load(Ordering::Relaxed);
                        let q = cap_state.stream_quality.load(Ordering::Relaxed);
                        let (sw, sh) = if max_w > 0 && cur_w > max_w {
                            let nh = ((cur_h as u64 * max_w as u64) / cur_w as u64).max(1) as u32;
                            (max_w, nh)
                        } else {
                            (cur_w, cur_h)
                        };

                        let mut force_kf = cap_state.force_keyframe.swap(false, Ordering::AcqRel);
                        if sw != cur_sw || sh != cur_sh || q != cur_q {
                            cur_sw = sw;
                            cur_sh = sh;
                            cur_q = q;
                            encoder = CompoundTileEncoder::new(sw, sh, 128, q, 3);
                            force_kf = true;
                        }

                        let encode_slice: &[u8] = if sw == cur_w && sh == cur_h {
                            &raw_buf
                        } else {
                            downsample_rgba_crisp(&raw_buf, cur_w, cur_h, sw, sh, &mut scaled_buf);
                            &scaled_buf
                        };

                        if encode_slice.len() == (sw * sh * 4) as usize {
                            let (pkt, flags) =
                                encoder.encode_rgba(encode_slice, now_us(), force_kf);
                            if !pkt.tiles.is_empty() {
                                let _ = cap_state.frame_tx.send((flags, pkt.serialize()));
                            }
                        }
                    }
                }

                let target_wait = if has_subscribers {
                    interval
                } else {
                    Duration::from_millis(250)
                };
                let elapsed = t0.elapsed();
                if elapsed < target_wait && cap_state.running.load(Ordering::Relaxed) {
                    std::thread::park_timeout(target_wait - elapsed);
                }
            }
        });
        let _ = state.cap_thread.set(cap_join.thread().clone());

        // 2. Direct TCP accept loop
        let srv_state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = srv_state.stop_notify.notified() => break,
                    res = listener.accept() => {
                        if let Ok((stream, _)) = res {
                            let _ = stream.set_nodelay(true);
                            let st = srv_state.clone();
                            tokio::spawn(async move { handle_client(st, stream).await; });
                        }
                    }
                }
            }
        });

        // 3. Optional outbound Relay registration + ownership binding + 20s PING/PONG keepalive
        if let Some(relay_addr) = cfg.relay_addr.clone() {
            if !relay_addr.is_empty() {
                let r_state = state.clone();
                tokio::spawn(async move {
                    relay_loop(r_state, relay_addr).await;
                });
            }
        }

        Ok(Self { state })
    }

    pub fn status(&self) -> HostStatus {
        let relay_status = self
            .state
            .relay_status
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let monitors = self
            .state
            .monitors
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let active_sessions = self
            .state
            .active_sessions
            .read()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default();
        HostStatus {
            running: self.state.running.load(Ordering::Relaxed),
            device_id: self.state.cfg.device_id.clone(),
            bind_addr: self.state.cfg.bind_addr.clone(),
            lan_addrs: self.state.lan_addrs.clone(),
            relay_addr: self.state.cfg.relay_addr.clone(),
            relay_status,
            capture_backend: self.state.capture_backend.clone(),
            width: self.state.width.load(Ordering::Relaxed),
            height: self.state.height.load(Ordering::Relaxed),
            monitors,
            active_monitor: self.state.active_monitor.load(Ordering::Relaxed),
            active_sessions,
            exec_enabled: self.state.cfg.allow_exec,
        }
    }

    pub fn kick_viewers(&self) {
        self.state.kick_notify.notify_waiters();
    }

    pub fn stop(&self) {
        self.state.running.store(false, Ordering::Release);
        self.state.wake_capturer();
        self.state.kick_notify.notify_waiters();
        self.state.stop_notify.notify_waiters();
    }
}

fn set_relay_status(state: &SharedState, s: impl Into<String>) {
    if let Ok(mut g) = state.relay_status.write() {
        *g = s.into();
    }
}

async fn relay_loop(state: Arc<SharedState>, relay_addr: String) {
    let key_part = compute_ownership_proof(&state.cfg.device_id, &state.cfg.device_secret)
        .map(|k| format!(" KEY={k}"))
        .unwrap_or_default();

    while state.running.load(Ordering::Relaxed) {
        let connect_fut = TcpStream::connect(&relay_addr);
        let stream = tokio::select! {
            _ = state.stop_notify.notified() => break,
            res = tokio::time::timeout(Duration::from_secs(6), connect_fut) => {
                match res {
                    Ok(Ok(s)) => s,
                    _ => {
                        set_relay_status(&state, "reconnecting");
                        tokio::select! {
                            _ = state.stop_notify.notified() => break,
                            _ = sleep(Duration::from_secs(2)) => continue,
                        }
                    }
                }
            }
        };

        let _ = stream.set_nodelay(true);
        let mut reader = BufReader::new(stream);
        let lan_part = if state.lan_addrs.is_empty() {
            String::new()
        } else {
            format!(" LAN={}", state.lan_addrs.join(","))
        };
        let reg = format!(
            "REGISTER {} {}{}{}\n",
            state.cfg.device_id, state.cfg.device_name, lan_part, key_part
        );

        let mut backoff_secs = 2u64;
        if reader.get_mut().write_all(reg.as_bytes()).await.is_ok() {
            let mut line = String::new();
            if tokio::time::timeout(Duration::from_secs(6), reader.read_line(&mut line))
                .await
                .ok()
                .and_then(|r| r.ok())
                .is_some()
            {
                let trimmed_resp = line.trim();
                if trimmed_resp.starts_with("OK REGISTERED") {
                    set_relay_status(&state, "connected");
                    let mut hb_interval = tokio::time::interval(Duration::from_secs(20));
                    hb_interval.tick().await;
                    let mut last_pong = Instant::now();

                    while state.running.load(Ordering::Relaxed) {
                        line.clear();
                        tokio::select! {
                            _ = state.stop_notify.notified() => return,
                            _ = hb_interval.tick() => {
                                if last_pong.elapsed() > Duration::from_secs(45) {
                                    break;
                                }
                                if reader.get_mut().write_all(b"PING\n").await.is_err() {
                                    break;
                                }
                            }
                            res = reader.read_line(&mut line) => {
                                match res {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {
                                        last_pong = Instant::now();
                                        let trimmed = line.trim();
                                        if let Some(sid) = trimmed.strip_prefix("INCOMING ") {
                                            let session_id = sid.trim().to_string();
                                            let st = state.clone();
                                            let addr = relay_addr.clone();
                                            tokio::spawn(async move {
                                                if let Ok(Ok(ds)) = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr)).await {
                                                    let _ = ds.set_nodelay(true);
                                                    let mut r = BufReader::new(ds);
                                                    let acc = format!("ACCEPT {session_id}\n");
                                                    if r.get_mut().write_all(acc.as_bytes()).await.is_ok() {
                                                        let mut ok = String::new();
                                                        if tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut ok)).await.is_ok()
                                                            && ok.starts_with("OK TUNNEL")
                                                        {
                                                            handle_client(st, r.into_inner()).await;
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    set_relay_status(&state, "reconnecting");
                } else if trimmed_resp.contains("ALREADY_OWNED") {
                    set_relay_status(&state, "already_owned");
                    backoff_secs = 15;
                } else {
                    set_relay_status(&state, format!("error:{trimmed_resp}"));
                }
            }
        }

        tokio::select! {
            _ = state.stop_notify.notified() => break,
            _ = sleep(Duration::from_secs(backoff_secs)) => {}
        }
    }
}

type WriterGuard = Arc<Mutex<(tokio::net::tcp::OwnedWriteHalf, u32, Option<SendCipher>)>>;

async fn handle_client(state: Arc<SharedState>, stream: TcpStream) {
    if state.auth_limiter.is_locked_out() {
        return;
    }

    let (mut rd, mut wr_raw) = stream.into_split();

    let Ok(Ok((first_hdr, first_payload))) =
        tokio::time::timeout(Duration::from_secs(6), read_frame(&mut rd)).await
    else {
        return;
    };

    let (mut rx_cipher, tx_cipher, hello) = match first_hdr.msg_type {
        MsgType::E2eeInit => {
            let Ok((pending_hs, ack_payload)) = ServerE2eeHandshake::begin(&first_payload) else {
                return;
            };
            if write_frame(
                &mut wr_raw,
                MsgType::E2eeAck,
                ChannelId::Control,
                0,
                1,
                &ack_payload,
            )
            .await
            .is_err()
            {
                return;
            }
            let Ok(Ok((h_hdr, h_ct))) =
                tokio::time::timeout(Duration::from_secs(6), read_frame(&mut rd)).await
            else {
                state.auth_limiter.record_failure();
                return;
            };
            if h_hdr.msg_type != MsgType::Hello {
                state.auth_limiter.record_failure();
                return;
            }
            // Try temporary token first, then optional unattended permanent token
            let mut opened_opt = pending_hs
                .try_open_first_frame(&state.cfg.token, &h_hdr, &h_ct)
                .ok();
            if opened_opt.is_none() {
                let perm = state.cfg.unattended_token.trim();
                if !perm.is_empty() {
                    opened_opt = pending_hs.try_open_first_frame(perm, &h_hdr, &h_ct).ok();
                }
            }
            let Some((send_c, recv_c, h_pt)) = opened_opt else {
                state.auth_limiter.record_failure();
                return;
            };
            let Ok(hello) = serde_json::from_slice::<HelloPayload>(&h_pt) else {
                state.auth_limiter.record_failure();
                return;
            };
            (Some(recv_c), Some(send_c), hello)
        }
        MsgType::Hello => {
            let Ok(hello) = serde_json::from_slice::<HelloPayload>(&first_payload) else {
                return;
            };
            (None, None, hello)
        }
        _ => return,
    };

    if !verify_host_token(&hello.token, &state.cfg) {
        state.auth_limiter.record_failure();
        return;
    }
    state.auth_limiter.record_success();

    let session_id = format!("{:08x}", rand::random::<u32>());
    let is_e2ee = tx_cipher.is_some();
    let wr: WriterGuard = Arc::new(Mutex::new((wr_raw, 1u32, tx_cipher)));

    let monitors = state
        .monitors
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();
    let ack = HelloAckPayload {
        session_id: session_id.clone(),
        width: state.width.load(Ordering::Relaxed) as u16,
        height: state.height.load(Ordering::Relaxed) as u16,
        tile_size: 128,
        capture_backend: state.capture_backend.clone(),
        input_backend: state.injector.backend_name().to_string(),
        exec_enabled: state.cfg.allow_exec,
        codec_probe: serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "e2ee": is_e2ee
        }),
        monitors,
        active_monitor: state.active_monitor.load(Ordering::Relaxed),
    };
    let ack_bytes = serde_json::to_vec(&ack).unwrap_or_default();
    if send_locked(&wr, MsgType::HelloAck, ChannelId::Control, 0, &ack_bytes)
        .await
        .is_err()
    {
        return;
    }

    if let Ok(mut sg) = state.active_sessions.write() {
        sg.insert(session_id.clone(), hello.client_name.clone());
    }

    let mut rx_vid = state.frame_tx.subscribe();
    let mut rx_cur = state.cursor_tx.subscribe();
    state.force_keyframe.store(true, Ordering::Release);
    state.wake_capturer();

    let wr_push = wr.clone();
    let st_push = state.clone();
    let push_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = st_push.stop_notify.notified() => break,
                _ = st_push.kick_notify.notified() => break,
                cur_res = rx_cur.recv() => {
                    if let Ok(cur_bytes) = cur_res {
                        if send_locked(&wr_push, MsgType::CursorState, ChannelId::Cursor, 0, &cur_bytes).await.is_err() {
                            break;
                        }
                    }
                }
                recv_res = rx_vid.recv() => {
                    match recv_res {
                        Ok((flags, data)) => {
                            if send_locked(&wr_push, MsgType::VideoTileFrame, ChannelId::Video, flags, &data).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            st_push.force_keyframe.store(true, Ordering::Release);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    loop {
        let frame_res = tokio::select! {
            _ = state.stop_notify.notified() => break,
            _ = state.kick_notify.notified() => break,
            r = read_frame_opt_cipher(&mut rd, rx_cipher.as_mut()) => r,
        };
        let Ok((hdr, payload)) = frame_res else { break };

        match hdr.msg_type {
            MsgType::Ping if payload.len() >= 8 => {
                let mut pong = vec![0u8; 16];
                pong[0..8].copy_from_slice(&payload[0..8]);
                pong[8..16].copy_from_slice(&now_us().to_le_bytes());
                let _ = send_locked(&wr, MsgType::Pong, ChannelId::Control, 0, &pong).await;
            }
            MsgType::RequestKeyFrame => {
                if payload.len() >= 3 {
                    let mw = u16::from_le_bytes([payload[0], payload[1]]) as u32;
                    let q = payload[2].clamp(30, 92);
                    state.stream_max_w.store(mw, Ordering::Relaxed);
                    state.stream_quality.store(q, Ordering::Relaxed);
                }
                state.force_keyframe.store(true, Ordering::Release);
                state.wake_capturer();
            }
            MsgType::MonitorSelect if payload.len() >= 2 => {
                let target_idx = u16::from_le_bytes([payload[0], payload[1]]);
                let monitors = state
                    .monitors
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let resp = if let Some(found) = monitors.iter().find(|m| m.index == target_idx) {
                    state.active_monitor.store(target_idx, Ordering::Release);
                    state.width.store(found.width, Ordering::Release);
                    state.height.store(found.height, Ordering::Release);
                    state.injector.set_target_monitor(found);
                    state
                        .requested_monitor
                        .store(target_idx as i32, Ordering::Release);
                    state.wake_capturer();
                    for _ in 0..25 {
                        if state.requested_monitor.load(Ordering::Acquire) < 0 {
                            break;
                        }
                        sleep(Duration::from_millis(10)).await;
                    }
                    MonitorSelectAckPayload {
                        ok: true,
                        active_monitor: state.active_monitor.load(Ordering::Relaxed),
                        width: state.width.load(Ordering::Relaxed) as u16,
                        height: state.height.load(Ordering::Relaxed) as u16,
                        monitors: state
                            .monitors
                            .read()
                            .map(|g| g.clone())
                            .unwrap_or(monitors),
                    }
                } else {
                    MonitorSelectAckPayload {
                        ok: false,
                        active_monitor: state.active_monitor.load(Ordering::Relaxed),
                        width: state.width.load(Ordering::Relaxed) as u16,
                        height: state.height.load(Ordering::Relaxed) as u16,
                        monitors,
                    }
                };
                if let Ok(bytes) = serde_json::to_vec(&resp) {
                    let _ = send_locked(
                        &wr,
                        MsgType::MonitorSelectAck,
                        ChannelId::Control,
                        0,
                        &bytes,
                    )
                    .await;
                }
            }
            MsgType::MouseEvent => {
                if let Ok(ev) = MouseEventPacket::deserialize(&payload) {
                    state.injector.inject_mouse(ev);
                }
            }
            MsgType::KeyEvent => {
                if let Ok(ev) = KeyEventPacket::deserialize(&payload) {
                    state.injector.inject_key(ev);
                }
            }
            MsgType::ScreenshotReq if payload.len() >= 4 => {
                let max_w = u16::from_le_bytes([payload[0], payload[1]]) as u32;
                let quality = payload[2];
                let as_png = payload[3] == 1;
                let rgba = state.latest_rgba.read().await.clone();
                let w = state.width.load(Ordering::Relaxed);
                let h = state.height.load(Ordering::Relaxed);
                if let Ok(Ok((ow, oh, img))) = tokio::task::spawn_blocking(move || {
                    encode_full_screenshot(&rgba, w, h, max_w, quality, as_png)
                })
                .await
                {
                    let mut resp = Vec::with_capacity(13 + img.len());
                    resp.extend_from_slice(&(w as u16).to_le_bytes());
                    resp.extend_from_slice(&(h as u16).to_le_bytes());
                    resp.extend_from_slice(&ow.to_le_bytes());
                    resp.extend_from_slice(&oh.to_le_bytes());
                    resp.push(if as_png { 1 } else { 0 });
                    resp.extend_from_slice(&(img.len() as u32).to_le_bytes());
                    resp.extend_from_slice(&img);
                    let _ =
                        send_locked(&wr, MsgType::ScreenshotResp, ChannelId::Control, 0, &resp)
                            .await;
                }
            }
            MsgType::ClipboardGet => {
                let fallback = state.clipboard.read().await.clone();
                let os_txt = tokio::task::spawn_blocking(clipboard::get_text)
                    .await
                    .unwrap_or_default();
                let txt = if os_txt.is_empty() { fallback } else { os_txt };
                let _ = send_locked(
                    &wr,
                    MsgType::ClipboardData,
                    ChannelId::Control,
                    0,
                    txt.as_bytes(),
                )
                .await;
            }
            MsgType::ClipboardData => {
                let txt = String::from_utf8_lossy(&payload).to_string();
                *state.clipboard.write().await = txt.clone();
                let txt_os = txt.clone();
                let _ = tokio::task::spawn_blocking(move || clipboard::set_text(&txt_os)).await;
                let _ = send_locked(
                    &wr,
                    MsgType::ClipboardData,
                    ChannelId::Control,
                    0,
                    txt.as_bytes(),
                )
                .await;
            }
            MsgType::ClipboardImageGet => {
                let fallback = state.clipboard_png.read().await.clone();
                let os_png = tokio::task::spawn_blocking(clipboard::get_image_png)
                    .await
                    .unwrap_or_default();
                let png = if os_png.is_empty() { fallback } else { os_png };
                let _ = send_locked(
                    &wr,
                    MsgType::ClipboardImageData,
                    ChannelId::Control,
                    0,
                    &png,
                )
                .await;
            }
            MsgType::ClipboardImageData => {
                let len_bytes = (payload.len() as u32).to_le_bytes();
                *state.clipboard_png.write().await = payload.clone();
                let png_os = payload;
                let _ = tokio::task::spawn_blocking(move || clipboard::set_image_png(&png_os)).await;
                let _ = send_locked(
                    &wr,
                    MsgType::ClipboardImageData,
                    ChannelId::Control,
                    0,
                    &len_bytes,
                )
                .await;
            }
            MsgType::FileListReq => {
                let rel = String::from_utf8_lossy(&payload);
                let resp = handle_file_list(&state.cfg.file_root, &rel);
                let _ =
                    send_locked(&wr, MsgType::FileListResp, ChannelId::File, 0, resp.as_bytes())
                        .await;
            }
            MsgType::FileUploadReq => {
                let resp = handle_file_upload(&state.cfg.file_root, &payload);
                let _ = send_locked(
                    &wr,
                    MsgType::FileUploadResp,
                    ChannelId::File,
                    0,
                    resp.as_bytes(),
                )
                .await;
            }
            MsgType::FileUploadChunk => {
                let root = state.cfg.file_root.clone();
                let resp =
                    tokio::task::spawn_blocking(move || handle_file_upload_chunk(&root, &payload))
                        .await
                        .unwrap_or_else(|e| {
                            serde_json::json!({ "ok": false, "error": e.to_string() }).to_string()
                        });
                let _ = send_locked(
                    &wr,
                    MsgType::FileUploadChunkAck,
                    ChannelId::File,
                    0,
                    resp.as_bytes(),
                )
                .await;
            }
            MsgType::FileDownloadReq => {
                let rel = String::from_utf8_lossy(&payload);
                let resp = handle_file_download(&state.cfg.file_root, &rel);
                let _ =
                    send_locked(&wr, MsgType::FileDownloadResp, ChannelId::File, 0, &resp).await;
            }
            MsgType::FileDownloadChunkReq => {
                let root = state.cfg.file_root.clone();
                let resp = tokio::task::spawn_blocking(move || {
                    handle_file_download_chunk(&root, &payload)
                })
                .await
                .unwrap_or_else(|e| encode_download_chunk_err(&e.to_string()));
                let _ = send_locked(
                    &wr,
                    MsgType::FileDownloadChunkResp,
                    ChannelId::File,
                    0,
                    &resp,
                )
                .await;
            }
            MsgType::FileActionReq => {
                let root = state.cfg.file_root.clone();
                let resp =
                    tokio::task::spawn_blocking(move || handle_file_action(&root, &payload))
                        .await
                        .unwrap_or_else(|e| {
                            serde_json::json!({ "ok": false, "error": e.to_string() }).to_string()
                        });
                let _ = send_locked(
                    &wr,
                    MsgType::FileActionResp,
                    ChannelId::File,
                    0,
                    resp.as_bytes(),
                )
                .await;
            }
            MsgType::ExecReq => {
                let json_str = String::from_utf8_lossy(&payload);
                let resp = handle_guarded_exec(&state.cfg, &json_str).await;
                let _ =
                    send_locked(&wr, MsgType::ExecResp, ChannelId::Control, 0, resp.as_bytes())
                        .await;
            }
            _ => {}
        }
    }

    push_task.abort();
    if let Ok(mut sg) = state.active_sessions.write() {
        sg.remove(&session_id);
    }
}

async fn send_locked(
    wr: &WriterGuard,
    msg_type: MsgType,
    ch: ChannelId,
    flags: u8,
    payload: &[u8],
) -> Result<(), ctd_protocol::ProtocolError> {
    let mut guard = wr.lock().await;
    guard.1 = guard.1.wrapping_add(1);
    let seq = guard.1;
    let (ref mut stream_wr, _, ref mut cipher_opt) = *guard;
    write_frame_opt_cipher(stream_wr, cipher_opt.as_mut(), msg_type, ch, flags, seq, payload).await
}

fn resolve_sandboxed(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let clean = rel.trim_start_matches(['/', '\\']);
    let mut joined = root.to_path_buf();
    for comp in Path::new(clean).components() {
        match comp {
            Component::Normal(p) => joined.push(p),
            Component::CurDir => {}
            _ => return Err("Path escapes sandbox fileRoot".into()),
        }
    }
    Ok(joined)
}

fn handle_file_list(root: &Path, rel: &str) -> String {
    let dir = match resolve_sandboxed(root, rel) {
        Ok(d) => d,
        Err(e) => return serde_json::json!({ "ok": false, "error": e }).to_string(),
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return serde_json::json!({ "ok": false, "error": "Directory not found" }).to_string();
    };
    let mut entries = Vec::new();
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        if name.ends_with(".ctdpart") {
            continue;
        }
        let meta = item.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta
            .as_ref()
            .map(|m| if is_dir { 0 } else { m.len() })
            .unwrap_or(0);
        entries.push(serde_json::json!({
            "name": name,
            "isDir": is_dir,
            "size": size
        }));
    }
    entries.sort_by(|a, b| {
        let ad = a.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false);
        let bd = b.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false);
        bd.cmp(&ad).then_with(|| {
            let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            an.cmp(bn)
        })
    });
    serde_json::json!({ "ok": true, "path": rel, "root": root.to_string_lossy(), "entries": entries }).to_string()
}

fn handle_file_upload(root: &Path, payload: &[u8]) -> String {
    if payload.len() < 66 {
        return serde_json::json!({ "ok": false, "error": "Short upload packet" }).to_string();
    }
    let path_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    if 2 + path_len + 64 > payload.len() {
        return serde_json::json!({ "ok": false, "error": "Truncated upload header" }).to_string();
    }
    let rel = String::from_utf8_lossy(&payload[2..2 + path_len]).to_string();
    let expected =
        String::from_utf8_lossy(&payload[2 + path_len..2 + path_len + 64]).to_lowercase();
    let data = &payload[2 + path_len + 64..];
    let actual = hex::encode(Sha256::digest(data));
    if expected != actual {
        return serde_json::json!({ "ok": false, "error": "SHA-256 mismatch" }).to_string();
    }
    let target = match resolve_sandboxed(root, &rel) {
        Ok(t) => t,
        Err(e) => return serde_json::json!({ "ok": false, "error": e }).to_string(),
    };
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&target, data) {
        Ok(_) => serde_json::json!({ "ok": true, "path": rel, "bytes": data.len(), "sha256": actual }).to_string(),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
    }
}

fn handle_file_upload_chunk(root: &Path, payload: &[u8]) -> String {
    if payload.len() < 2 {
        return serde_json::json!({ "ok": false, "error": "Short chunk packet" }).to_string();
    }
    let meta_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    if 2 + meta_len > payload.len() {
        return serde_json::json!({ "ok": false, "error": "Truncated chunk metadata" }).to_string();
    }
    let Ok(meta) = serde_json::from_slice::<UploadChunkMeta>(&payload[2..2 + meta_len]) else {
        return serde_json::json!({ "ok": false, "error": "Invalid chunk metadata JSON" }).to_string();
    };
    if meta.path.trim().is_empty() {
        return serde_json::json!({ "ok": false, "error": "Empty target path" }).to_string();
    }
    let target = match resolve_sandboxed(root, &meta.path) {
        Ok(t) => t,
        Err(e) => return serde_json::json!({ "ok": false, "error": e }).to_string(),
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string();
        }
    }

    let part_path = PathBuf::from(format!("{}.ctdpart", target.to_string_lossy()));
    let chunk = &payload[2 + meta_len..];

    let file_res = if meta.offset == 0 {
        std::fs::File::create(&part_path)
    } else {
        std::fs::OpenOptions::new().write(true).open(&part_path)
    };
    let mut f = match file_res {
        Ok(f) => f,
        Err(e) => return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
    };
    if meta.offset > 0 {
        if let Err(e) = f.seek(SeekFrom::Start(meta.offset)) {
            return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string();
        }
    }
    if !chunk.is_empty() {
        if let Err(e) = f.write_all(chunk) {
            return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string();
        }
    }
    let _ = f.flush();
    drop(f);

    let written = meta.offset + chunk.len() as u64;
    if !meta.eof {
        return serde_json::json!({
            "ok": true,
            "done": false,
            "written": written
        })
        .to_string();
    }

    let Ok(mut rf) = std::fs::File::open(&part_path) else {
        return serde_json::json!({ "ok": false, "error": "Failed to reopen part file" }).to_string();
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total_bytes = 0u64;
    loop {
        match rf.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                total_bytes += n as u64;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part_path);
                return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string();
            }
        }
    }
    drop(rf);

    let actual_sha = hex::encode(hasher.finalize());
    if let Some(expected) = &meta.expected_sha256 {
        if !expected.is_empty() && !expected.eq_ignore_ascii_case(&actual_sha) {
            let _ = std::fs::remove_file(&part_path);
            return serde_json::json!({
                "ok": false,
                "error": format!("SHA-256 mismatch (expected {expected}, got {actual_sha})")
            })
            .to_string();
        }
    }

    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    if let Err(e) = std::fs::rename(&part_path, &target) {
        return serde_json::json!({ "ok": false, "error": e.to_string() }).to_string();
    }

    serde_json::json!({
        "ok": true,
        "done": true,
        "path": meta.path,
        "bytes": total_bytes,
        "sha256": actual_sha
    })
    .to_string()
}

fn handle_file_download(root: &Path, rel: &str) -> Vec<u8> {
    let fail = |msg: &str| {
        let mut resp = vec![0u8; 67 + msg.len()];
        resp[65..67].copy_from_slice(&(msg.len() as u16).to_le_bytes());
        resp[67..].copy_from_slice(msg.as_bytes());
        resp
    };
    let target = match resolve_sandboxed(root, rel) {
        Ok(t) => t,
        Err(e) => return fail(&e),
    };
    let Ok(data) = std::fs::read(&target) else {
        return fail("File not found");
    };
    let sha = hex::encode(Sha256::digest(&data));
    let mut resp = Vec::with_capacity(67 + data.len());
    resp.push(1);
    resp.extend_from_slice(sha.as_bytes());
    resp.extend_from_slice(&0u16.to_le_bytes());
    resp.extend_from_slice(&data);
    resp
}

fn encode_download_chunk_err(msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + msg.len());
    out.push(0u8);
    out.extend_from_slice(msg.as_bytes());
    out
}

fn handle_file_download_chunk(root: &Path, payload: &[u8]) -> Vec<u8> {
    let Ok(req) = serde_json::from_slice::<DownloadChunkReq>(payload) else {
        return encode_download_chunk_err("Invalid download request JSON");
    };
    let target = match resolve_sandboxed(root, &req.path) {
        Ok(t) => t,
        Err(e) => return encode_download_chunk_err(&e),
    };
    let Ok(meta) = std::fs::metadata(&target) else {
        return encode_download_chunk_err("File not found");
    };
    if meta.is_dir() {
        return encode_download_chunk_err("Cannot download a directory");
    }
    let total_size = meta.len();
    let max_bytes = (req.max_bytes as usize).clamp(4096, FILE_CHUNK_SIZE * 4);
    let mut f = match std::fs::File::open(&target) {
        Ok(f) => f,
        Err(e) => return encode_download_chunk_err(&e.to_string()),
    };
    if req.offset > 0 && req.offset < total_size {
        if let Err(e) = f.seek(SeekFrom::Start(req.offset)) {
            return encode_download_chunk_err(&e.to_string());
        }
    }

    let to_read = if req.offset >= total_size {
        0
    } else {
        ((total_size - req.offset) as usize).min(max_bytes)
    };
    let mut chunk = vec![0u8; to_read];
    if to_read > 0 {
        if let Err(e) = f.read_exact(&mut chunk) {
            return encode_download_chunk_err(&e.to_string());
        }
    }
    let eof = if req.offset + (chunk.len() as u64) >= total_size {
        1u8
    } else {
        0u8
    };
    let digest = Sha256::digest(&chunk);

    let mut out = Vec::with_capacity(50 + chunk.len());
    out.push(1u8);
    out.extend_from_slice(&total_size.to_le_bytes());
    out.extend_from_slice(&req.offset.to_le_bytes());
    out.push(eof);
    out.extend_from_slice(&digest);
    out.extend_from_slice(&chunk);
    out
}

fn handle_file_action(root: &Path, payload: &[u8]) -> String {
    let Ok(act) = serde_json::from_slice::<FileActionPayload>(payload) else {
        return serde_json::json!({ "ok": false, "error": "Invalid file action JSON" }).to_string();
    };
    let clean_rel = act.path.trim().trim_matches(['/', '\\']);
    if clean_rel.is_empty() || clean_rel == "." {
        return serde_json::json!({ "ok": false, "error": "Target relative path cannot be empty" })
            .to_string();
    }
    let target = match resolve_sandboxed(root, clean_rel) {
        Ok(t) => t,
        Err(e) => return serde_json::json!({ "ok": false, "error": e }).to_string(),
    };
    if target == root {
        return serde_json::json!({ "ok": false, "error": "Cannot modify sandbox root directory" })
            .to_string();
    }

    match act.action.as_str() {
        "mkdir" => match std::fs::create_dir_all(&target) {
            Ok(_) => serde_json::json!({ "ok": true, "action": "mkdir", "path": clean_rel }).to_string(),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
        },
        "delete" => {
            if !target.exists() {
                return serde_json::json!({ "ok": false, "error": "Target does not exist" }).to_string();
            }
            let res = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else {
                std::fs::remove_file(&target)
            };
            match res {
                Ok(_) => serde_json::json!({ "ok": true, "action": "delete", "path": clean_rel }).to_string(),
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
            }
        }
        "rename" => {
            let new_name = act.new_name.unwrap_or_default().trim().to_string();
            let mut comps = Path::new(&new_name).components();
            let valid_single = matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none();
            if !valid_single {
                return serde_json::json!({ "ok": false, "error": "Invalid new file/folder name" }).to_string();
            }
            let Some(parent) = target.parent() else {
                return serde_json::json!({ "ok": false, "error": "Invalid parent directory" }).to_string();
            };
            let dest = parent.join(&new_name);
            if dest.exists() {
                return serde_json::json!({ "ok": false, "error": "Target name already exists" }).to_string();
            }
            match std::fs::rename(&target, &dest) {
                Ok(_) => serde_json::json!({ "ok": true, "action": "rename", "newName": new_name }).to_string(),
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
            }
        }
        "cancelUpload" => {
            let part_path = PathBuf::from(format!("{}.ctdpart", target.to_string_lossy()));
            let _ = std::fs::remove_file(&part_path);
            serde_json::json!({ "ok": true, "action": "cancelUpload" }).to_string()
        }
        other => serde_json::json!({ "ok": false, "error": format!("Unknown file action: {other}") }).to_string(),
    }
}

async fn handle_guarded_exec(cfg: &HostConfig, json_str: &str) -> String {
    if !cfg.allow_exec {
        return serde_json::json!({ "ok": false, "allowed": false, "error": "Remote exec is disabled" }).to_string();
    }
    let val: serde_json::Value = serde_json::from_str(json_str).unwrap_or_default();
    let cmd = val
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if cmd.is_empty() || cmd.contains(['&', '|', ';', '>', '<', '`', '$', '\r', '\n']) {
        return serde_json::json!({ "ok": false, "allowed": false, "error": "Command blocked by guardrail" }).to_string();
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let prog = parts[0].to_lowercase();
    if !cfg
        .exec_allowlist
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&prog))
    {
        return serde_json::json!({ "ok": false, "allowed": false, "error": format!("Command '{prog}' not in allowlist") }).to_string();
    }
    let mut c = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new(parts[0]);
        if parts.len() > 1 {
            c.args(&parts[1..]);
        }
        c
    };
    match c.output().await {
        Ok(out) => serde_json::json!({
            "ok": true,
            "allowed": true,
            "exitCode": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        })
        .to_string(),
        Err(e) => serde_json::json!({ "ok": false, "allowed": true, "error": e.to_string() }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_locks_after_five_failures_and_clears_on_success() {
        let lim = AuthRateLimiter::new();
        for _ in 0..4 {
            assert!(!lim.is_locked_out());
            lim.record_failure();
        }
        assert!(!lim.is_locked_out());
        lim.record_failure();
        assert!(lim.is_locked_out());
        lim.record_success();
        assert!(!lim.is_locked_out());
    }
}
