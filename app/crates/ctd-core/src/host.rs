//! Cross-platform async Host service:
//! - Direct TCP listener + optional outbound Relay registration (NAT traversal)
//! - Token handshake, compound tile video broadcast, screenshot, clipboard, SHA-256 sandboxed files, guarded exec.

use crate::capture::{create_capturer, now_us};
use crate::codec::{encode_full_screenshot, CompoundTileEncoder};
use crate::input::{create_injector, InputInjector};
use ctd_protocol::{
    read_frame, write_frame, ChannelId, HelloAckPayload, HelloPayload, KeyEventPacket,
    MouseEventPacket, MsgType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    pub bind_addr: String,
    pub token: String,
    pub device_id: String,
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
            bind_addr: "127.0.0.1:19800".to_string(),
            token: "demo123".to_string(),
            device_id: "888100200".to_string(),
            device_name: std::env::consts::OS.to_string(),
            relay_addr: Some("127.0.0.1:19901".to_string()),
            fps: 30,
            jpeg_quality: 75,
            force_synthetic: false,
            file_root: PathBuf::from("./shared-files"),
            allow_input: false,
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
    pub relay_addr: Option<String>,
    pub capture_backend: String,
    pub width: u32,
    pub height: u32,
    pub exec_enabled: bool,
}

struct SharedState {
    cfg: HostConfig,
    width: u32,
    height: u32,
    capture_backend: String,
    latest_rgba: RwLock<Vec<u8>>,
    clipboard: RwLock<String>,
    injector: Arc<dyn InputInjector>,
    frame_tx: broadcast::Sender<(u8, Vec<u8>)>,
    running: AtomicBool,
}

pub struct HostHandle {
    state: Arc<SharedState>,
}

impl HostHandle {
    pub async fn start(mut cfg: HostConfig) -> Result<Self, String> {
        if cfg.file_root.as_os_str().is_empty() || cfg.file_root.is_relative() {
            cfg.file_root = std::env::temp_dir().join("CopyToDesk").join("shared-files");
        }
        std::fs::create_dir_all(&cfg.file_root)
            .map_err(|e| format!("create file_root {:?}: {}", cfg.file_root, e))?;
        let mut capturer = create_capturer(cfg.force_synthetic);
        let (width, height) = capturer.dimensions();
        let capture_backend = capturer.backend_name().to_string();
        let (frame_tx, _) = broadcast::channel(4);

        let state = Arc::new(SharedState {
            cfg: cfg.clone(),
            width,
            height,
            capture_backend,
            latest_rgba: RwLock::new(vec![0u8; (width * height * 4) as usize]),
            clipboard: RwLock::new(String::new()),
            injector: create_injector(cfg.allow_input),
            frame_tx,
            running: AtomicBool::new(true),
        });

        let listener = TcpListener::bind(&cfg.bind_addr)
            .await
            .map_err(|e| format!("bind {}: {}", cfg.bind_addr, e))?;

        // 1. Capture + Compound Encode loop (blocking OS capture on dedicated thread)
        let cap_state = state.clone();
        std::thread::spawn(move || {
            let interval = Duration::from_millis((1000 / cap_state.cfg.fps.max(1)).max(10) as u64);
            let mut encoder = CompoundTileEncoder::new(width, height, 128, cap_state.cfg.jpeg_quality, 3);
            let mut buf = vec![0u8; (width * height * 4) as usize];
            while cap_state.running.load(Ordering::Relaxed) {
                let t0 = std::time::Instant::now();
                if capturer.capture_rgba(&mut buf) {
                    if let Ok(mut guard) = cap_state.latest_rgba.try_write() {
                        guard.clone_from(&buf);
                    }
                    if cap_state.frame_tx.receiver_count() > 0 {
                        let (pkt, flags) = encoder.encode_rgba(&buf, now_us(), false);
                        if !pkt.tiles.is_empty() {
                            let _ = cap_state.frame_tx.send((flags, pkt.serialize()));
                        }
                    }
                }
                let elapsed = t0.elapsed();
                if elapsed < interval {
                    std::thread::sleep(interval - elapsed);
                }
            }
        });

        // 2. Direct TCP accept loop
        let srv_state = state.clone();
        tokio::spawn(async move {
            while srv_state.running.load(Ordering::Relaxed) {
                tokio::select! {
                    res = listener.accept() => {
                        if let Ok((stream, _)) = res {
                            let _ = stream.set_nodelay(true);
                            let st = srv_state.clone();
                            tokio::spawn(async move { handle_client(st, stream).await; });
                        }
                    }
                    _ = sleep(Duration::from_millis(200)) => {}
                }
            }
        });

        // 3. Optional outbound Relay registration loop (NAT traversal)
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
        HostStatus {
            running: self.state.running.load(Ordering::Relaxed),
            device_id: self.state.cfg.device_id.clone(),
            bind_addr: self.state.cfg.bind_addr.clone(),
            relay_addr: self.state.cfg.relay_addr.clone(),
            capture_backend: self.state.capture_backend.clone(),
            width: self.state.width,
            height: self.state.height,
            exec_enabled: self.state.cfg.allow_exec,
        }
    }

    pub fn stop(&self) {
        self.state.running.store(false, Ordering::Relaxed);
    }
}

async fn relay_loop(state: Arc<SharedState>, relay_addr: String) {
    while state.running.load(Ordering::Relaxed) {
        if let Ok(stream) = TcpStream::connect(&relay_addr).await {
            let _ = stream.set_nodelay(true);
            let mut reader = BufReader::new(stream);
            let reg = format!("REGISTER {} {}\n", state.cfg.device_id, state.cfg.device_name);
            if reader.get_mut().write_all(reg.as_bytes()).await.is_ok() {
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_ok() && line.starts_with("OK REGISTERED") {
                    while state.running.load(Ordering::Relaxed) {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if let Some(sid) = line.strip_prefix("INCOMING ") {
                                    let session_id = sid.trim().to_string();
                                    let st = state.clone();
                                    let addr = relay_addr.clone();
                                    tokio::spawn(async move {
                                        if let Ok(ds) = TcpStream::connect(&addr).await {
                                            let _ = ds.set_nodelay(true);
                                            let mut r = BufReader::new(ds);
                                            let acc = format!("ACCEPT {session_id}\n");
                                            if r.get_mut().write_all(acc.as_bytes()).await.is_ok() {
                                                let mut ok = String::new();
                                                if r.read_line(&mut ok).await.is_ok() && ok.starts_with("OK TUNNEL") {
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
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn handle_client(state: Arc<SharedState>, stream: TcpStream) {
    let (mut rd, wr) = stream.into_split();
    let wr = Arc::new(Mutex::new((wr, 0u32)));

    // Expect Hello
    let Ok((hdr, payload)) = read_frame(&mut rd).await else { return };
    if hdr.msg_type != MsgType::Hello {
        return;
    }
    let Ok(hello) = serde_json::from_slice::<HelloPayload>(&payload) else { return };
    if hello.token != state.cfg.token {
        return;
    }

    let ack = HelloAckPayload {
        session_id: format!("{:08x}", rand::random::<u32>()),
        width: state.width as u16,
        height: state.height as u16,
        tile_size: 128,
        capture_backend: state.capture_backend.clone(),
        input_backend: state.injector.backend_name().to_string(),
        exec_enabled: state.cfg.allow_exec,
        codec_probe: serde_json::json!({ "os": std::env::consts::OS, "arch": std::env::consts::ARCH }),
    };
    let ack_bytes = serde_json::to_vec(&ack).unwrap_or_default();
    if send_locked(&wr, MsgType::HelloAck, ChannelId::Control, 0, &ack_bytes).await.is_err() {
        return;
    }

    // Spawn video frame forwarder
    let mut rx = state.frame_tx.subscribe();
    let wr_vid = wr.clone();
    let vid_task = tokio::spawn(async move {
        while let Ok((flags, data)) = rx.recv().await {
            if send_locked(&wr_vid, MsgType::VideoTileFrame, ChannelId::Video, flags, &data).await.is_err() {
                break;
            }
        }
    });

    while let Ok((hdr, payload)) = read_frame(&mut rd).await {
        match hdr.msg_type {
            MsgType::Ping if payload.len() >= 8 => {
                let mut pong = vec![0u8; 16];
                pong[0..8].copy_from_slice(&payload[0..8]);
                pong[8..16].copy_from_slice(&now_us().to_le_bytes());
                let _ = send_locked(&wr, MsgType::Pong, ChannelId::Control, 0, &pong).await;
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
                if let Ok((ow, oh, img)) = encode_full_screenshot(&rgba, state.width, state.height, max_w, quality, as_png) {
                    let mut resp = Vec::with_capacity(13 + img.len());
                    resp.extend_from_slice(&(state.width as u16).to_le_bytes());
                    resp.extend_from_slice(&(state.height as u16).to_le_bytes());
                    resp.extend_from_slice(&ow.to_le_bytes());
                    resp.extend_from_slice(&oh.to_le_bytes());
                    resp.push(if as_png { 1 } else { 0 });
                    resp.extend_from_slice(&(img.len() as u32).to_le_bytes());
                    resp.extend_from_slice(&img);
                    let _ = send_locked(&wr, MsgType::ScreenshotResp, ChannelId::Control, 0, &resp).await;
                }
            }
            MsgType::ClipboardGet => {
                let txt = state.clipboard.read().await.clone();
                let _ = send_locked(&wr, MsgType::ClipboardData, ChannelId::Control, 0, txt.as_bytes()).await;
            }
            MsgType::ClipboardData => {
                let txt = String::from_utf8_lossy(&payload).to_string();
                *state.clipboard.write().await = txt.clone();
                let _ = send_locked(&wr, MsgType::ClipboardData, ChannelId::Control, 0, txt.as_bytes()).await;
            }
            MsgType::FileListReq => {
                let rel = String::from_utf8_lossy(&payload);
                let resp = handle_file_list(&state.cfg.file_root, &rel);
                let _ = send_locked(&wr, MsgType::FileListResp, ChannelId::File, 0, resp.as_bytes()).await;
            }
            MsgType::FileUploadReq => {
                let resp = handle_file_upload(&state.cfg.file_root, &payload);
                let _ = send_locked(&wr, MsgType::FileUploadResp, ChannelId::File, 0, resp.as_bytes()).await;
            }
            MsgType::FileDownloadReq => {
                let rel = String::from_utf8_lossy(&payload);
                let resp = handle_file_download(&state.cfg.file_root, &rel);
                let _ = send_locked(&wr, MsgType::FileDownloadResp, ChannelId::File, 0, &resp).await;
            }
            MsgType::ExecReq => {
                let json_str = String::from_utf8_lossy(&payload);
                let resp = handle_guarded_exec(&state.cfg, &json_str).await;
                let _ = send_locked(&wr, MsgType::ExecResp, ChannelId::Control, 0, resp.as_bytes()).await;
            }
            _ => {}
        }
    }

    vid_task.abort();
}

async fn send_locked(
    wr: &Arc<Mutex<(tokio::net::tcp::OwnedWriteHalf, u32)>>,
    msg_type: MsgType,
    ch: ChannelId,
    flags: u8,
    payload: &[u8],
) -> Result<(), ctd_protocol::ProtocolError> {
    let mut guard = wr.lock().await;
    guard.1 = guard.1.wrapping_add(1);
    let seq = guard.1;
    write_frame(&mut guard.0, msg_type, ch, flags, seq, payload).await
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
        let meta = item.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| if is_dir { 0 } else { m.len() }).unwrap_or(0);
        entries.push(serde_json::json!({
            "name": item.file_name().to_string_lossy(),
            "isDir": is_dir,
            "size": size
        }));
    }
    serde_json::json!({ "ok": true, "path": rel, "entries": entries }).to_string()
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
    let expected = String::from_utf8_lossy(&payload[2 + path_len..2 + path_len + 64]).to_lowercase();
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

async fn handle_guarded_exec(cfg: &HostConfig, json_str: &str) -> String {
    if !cfg.allow_exec {
        return serde_json::json!({ "ok": false, "allowed": false, "error": "Remote exec is disabled" }).to_string();
    }
    let val: serde_json::Value = serde_json::from_str(json_str).unwrap_or_default();
    let cmd = val.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
    if cmd.is_empty() || cmd.contains(['&', '|', ';', '>', '<', '`', '$', '\r', '\n']) {
        return serde_json::json!({ "ok": false, "allowed": false, "error": "Command blocked by guardrail" }).to_string();
    }
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let prog = parts[0].to_lowercase();
    if !cfg.exec_allowlist.iter().any(|a| a.eq_ignore_ascii_case(&prog)) {
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
