//! Cross-platform async Client (`RemoteClient`, v0.5.0 P2):
//! - End-to-End Encryption (`X25519` + `HKDF-SHA256` + `ChaCha20-Poly1305`)
//! - Live multi-monitor switching (`select_monitor`) + real-time `CursorState` stream
//! - Smart LAN-first P2P direct race + lock-free split writer + 256 KB chunked file transfer

use crate::capture::now_us;
use base64::Engine;
use ctd_protocol::{
    connect_via_relay, frame_flags, lookup_lan_via_relay, read_frame, read_frame_opt_cipher,
    write_frame, write_frame_opt_cipher, ChannelId, CursorStatePacket, DownloadChunkReq,
    E2eeClientHandshake, FileActionPayload, HelloAckPayload, HelloPayload, KeyEventPacket,
    MonitorSelectAckPayload, MouseEventPacket, MsgType, RecvCipher, SendCipher, TileEncoding,
    UploadChunkMeta, VideoTileFramePacket, FILE_CHUNK_SIZE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub direct_addr: Option<String>,
    pub relay_addr: Option<String>,
    pub device_id: Option<String>,
    pub token: String,
    pub client_name: Option<String>,
    pub initial_max_width: Option<u16>,
    pub initial_quality: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectInfo {
    pub ack: HelloAckPayload,
    pub route: String, // "direct" | "lan-p2p" | "relay"
    pub endpoint: String,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TileItem {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub mime: String,
    pub is_lossless: bool,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TilePushEvent {
    pub frame_id: u32,
    pub screen_w: u16,
    pub screen_h: u16,
    pub encode_us: u32,
    pub byte_len: u32,
    pub is_key_frame: bool,
    pub has_lossless: bool,
    pub tiles: Vec<TileItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPushEvent {
    pub norm_x: u16,
    pub norm_y: u16,
    pub shape: u8,
    pub visible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    pub orig_w: u16,
    pub orig_h: u16,
    pub out_w: u16,
    pub out_h: u16,
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadResult {
    pub ok: bool,
    pub remote_path: String,
    pub local_path: String,
    pub bytes: u64,
    pub sha256: String,
}

type RpcWaiters = Arc<Mutex<HashMap<u8, VecDeque<oneshot::Sender<Vec<u8>>>>>>;

pub struct RemoteClient {
    tx: mpsc::UnboundedSender<(MsgType, ChannelId, Vec<u8>)>,
    waiters: RpcWaiters,
    upload_hashers: Mutex<HashMap<String, Sha256>>,
    remote_w: AtomicU16,
    remote_h: AtomicU16,
    download_cancel: Arc<AtomicBool>,
    manual_disconnect: Arc<AtomicBool>,
    cancel: Arc<Notify>,
    pub info: ConnectInfo,
}

async fn perform_e2ee_hello(
    mut stream: TcpStream,
    token: &str,
    client_name: &str,
) -> Result<(TcpStream, HelloAckPayload, SendCipher, RecvCipher), String> {
    let (hs, init_bytes) = E2eeClientHandshake::new();
    write_frame(
        &mut stream,
        MsgType::E2eeInit,
        ChannelId::Control,
        0,
        1,
        &init_bytes,
    )
    .await
    .map_err(|e| e.to_string())?;

    let (ack_hdr, ack_raw) = timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .map_err(|_| "E2EE 密钥协商超时（目标可能离线或受速率限制）".to_string())?
        .map_err(|_| "认证被拒绝（密码错误或受防爆破保护限制）".to_string())?;
    if ack_hdr.msg_type != MsgType::E2eeAck {
        return Err("远端未响应 E2EE 密钥协商".into());
    }

    let (mut send_c, mut recv_c) = hs.finish(&ack_raw, token).map_err(|e| e.to_string())?;

    let hello = HelloPayload {
        client_name: client_name.to_string(),
        token: token.to_string(),
        supports_clip_push: true,
    };
    let hello_bytes = serde_json::to_vec(&hello).map_err(|e| e.to_string())?;
    write_frame_opt_cipher(
        &mut stream,
        Some(&mut send_c),
        MsgType::Hello,
        ChannelId::Control,
        0,
        2,
        &hello_bytes,
    )
    .await
    .map_err(|e| e.to_string())?;

    let (hdr, payload) = timeout(
        Duration::from_secs(5),
        read_frame_opt_cipher(&mut stream, Some(&mut recv_c)),
    )
    .await
    .map_err(|_| "认证握手超时".to_string())?
    .map_err(|_| "握手认证失败（访问密码错误或连接被拒绝）".to_string())?;

    if hdr.msg_type != MsgType::HelloAck {
        return Err("握手认证被拒绝（密码错误）".into());
    }
    let ack: HelloAckPayload = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    Ok((stream, ack, send_c, recv_c))
}

impl RemoteClient {
    /// Fallback decoder for non-binary IPC environments: converts `[flags_u8, VideoTileFramePacket...]` into `TilePushEvent`.
    pub fn decode_raw_frame_to_event(raw: &[u8]) -> Option<TilePushEvent> {
        if raw.len() < 25 {
            return None;
        }
        let hdr_flags = raw[0];
        let payload = &raw[1..];
        let byte_len = payload.len() as u32;
        let pkt = VideoTileFramePacket::deserialize(payload).ok()?;
        let tiles = pkt
            .tiles
            .into_iter()
            .map(|t| {
                let (mime, is_lossless) = match t.encoding {
                    TileEncoding::PngLossless => ("image/png".to_string(), true),
                    _ => ("image/jpeg".to_string(), false),
                };
                TileItem {
                    x: t.x,
                    y: t.y,
                    w: t.w,
                    h: t.h,
                    mime,
                    is_lossless,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(&t.data),
                }
            })
            .collect();
        Some(TilePushEvent {
            frame_id: pkt.frame_id,
            screen_w: pkt.screen_w,
            screen_h: pkt.screen_h,
            encode_us: pkt.encode_us,
            byte_len,
            is_key_frame: (hdr_flags & frame_flags::KEY_FRAME) != 0,
            has_lossless: (hdr_flags & frame_flags::LOSSLESS_REFINEMENT) != 0,
            tiles,
        })
    }

    pub async fn connect_streaming<F, C, R, P, D>(
        params: &ConnectParams,
        on_raw_frame: F,
        on_cursor: C,
        on_rtt: R,
        on_clipboard: P,
        on_disconnect: D,
    ) -> Result<Self, String>
    where
        F: Fn(Vec<u8>) + Send + Sync + 'static,
        C: Fn(CursorPushEvent) + Send + Sync + 'static,
        R: Fn(f64) + Send + Sync + 'static,
        P: Fn(String) + Send + Sync + 'static,
        D: Fn(String) + Send + Sync + 'static,
    {
        let cname = params
            .client_name
            .as_deref()
            .unwrap_or("CopyToDesk-Desktop");

        let (stream, ack, mut send_cipher, mut recv_cipher, route, endpoint) =
            if let (Some(relay), Some(dev)) = (&params.relay_addr, &params.device_id) {
                if !relay.is_empty() && !dev.is_empty() {
                    let mut lan_hit = None;
                    let lan_candidates = lookup_lan_via_relay(relay, dev).await;
                    for cand in lan_candidates {
                        if let Ok(Ok(s)) =
                            timeout(Duration::from_millis(380), TcpStream::connect(&cand)).await
                        {
                            let _ = s.set_nodelay(true);
                            if let Ok(Ok((s2, ack, sc, rc))) = timeout(
                                Duration::from_millis(650),
                                perform_e2ee_hello(s, &params.token, cname),
                            )
                            .await
                            {
                                lan_hit = Some((s2, ack, sc, rc, "lan-p2p".to_string(), cand));
                                break;
                            }
                        }
                    }
                    if let Some(hit) = lan_hit {
                        hit
                    } else {
                        let s = connect_via_relay(relay, dev)
                            .await
                            .map_err(|e| e.to_string())?;
                        let (s2, ack, sc, rc) =
                            perform_e2ee_hello(s, &params.token, cname).await?;
                        (s2, ack, sc, rc, "relay".to_string(), relay.clone())
                    }
                } else {
                    let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
                    let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
                    let _ = s.set_nodelay(true);
                    let (s2, ack, sc, rc) = perform_e2ee_hello(s, &params.token, cname).await?;
                    (s2, ack, sc, rc, "direct".to_string(), addr.to_string())
                }
            } else {
                let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
                let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
                let _ = s.set_nodelay(true);
                let (s2, ack, sc, rc) = perform_e2ee_hello(s, &params.token, cname).await?;
                (s2, ack, sc, rc, "direct".to_string(), addr.to_string())
            };

        let init_w = ack.width;
        let init_h = ack.height;
        let info = ConnectInfo {
            ack,
            route,
            endpoint,
            encrypted: true,
        };
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<(MsgType, ChannelId, Vec<u8>)>();
        let waiters: RpcWaiters = Arc::new(Mutex::new(HashMap::new()));
        let manual_disconnect = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(Notify::new());

        // 2. Dedicated Lock-Free Encrypted Writer Task
        let cancel_wr = cancel.clone();
        tokio::spawn(async move {
            let mut seq = 2u32;
            loop {
                tokio::select! {
                    _ = cancel_wr.notified() => break,
                    item = rx.recv() => {
                        let Some((mtype, ch, payload)) = item else { break };
                        seq = seq.wrapping_add(1);
                        if write_frame_opt_cipher(&mut wr, Some(&mut send_cipher), mtype, ch, 0, seq, &payload).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // 3. Dedicated Continuous Encrypted Reader Task
        let cancel_rd = cancel.clone();
        let waiters_rd = waiters.clone();
        let manual_disc_rd = manual_disconnect.clone();
        tokio::spawn(async move {
            loop {
                let frame_res = tokio::select! {
                    _ = cancel_rd.notified() => break,
                    r = read_frame_opt_cipher(&mut rd, Some(&mut recv_cipher)) => r,
                };
                let Ok((hdr, payload)) = frame_res else { break };

                match hdr.msg_type {
                    MsgType::VideoTileFrame => {
                        if payload.len() >= 24 {
                            let mut raw = Vec::with_capacity(1 + payload.len());
                            raw.push(hdr.flags);
                            raw.extend_from_slice(&payload);
                            on_raw_frame(raw);
                        }
                    }
                    MsgType::CursorState => {
                        if let Ok(cur) = CursorStatePacket::deserialize(&payload) {
                            on_cursor(CursorPushEvent {
                                norm_x: cur.norm_x,
                                norm_y: cur.norm_y,
                                shape: cur.shape,
                                visible: cur.visible != 0,
                            });
                        }
                    }
                    MsgType::ClipboardPush => {
                        let txt = String::from_utf8_lossy(&payload).to_string();
                        if !txt.is_empty() {
                            on_clipboard(txt);
                        }
                    }
                    MsgType::Pong if payload.len() >= 8 => {
                        let sent = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                        let now = now_us();
                        if now >= sent {
                            on_rtt((now - sent) as f64 / 1000.0);
                        }
                    }
                    other => {
                        let mut map = waiters_rd.lock().await;
                        if let Some(q) = map.get_mut(&(other as u8)) {
                            if let Some(sender) = q.pop_front() {
                                let _ = sender.send(payload);
                            }
                        }
                    }
                }
            }
            if !manual_disc_rd.load(Ordering::Acquire) {
                on_disconnect("远端连接已中断".to_string());
            }
            cancel_rd.notify_waiters();
        });

        // 4. Periodic 2s Ping Task
        let tx_ping = tx.clone();
        let cancel_ping = cancel.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = cancel_ping.notified() => break,
                    _ = iv.tick() => {
                        let ts = now_us().to_le_bytes().to_vec();
                        if tx_ping.send((MsgType::Ping, ChannelId::Control, ts)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let client = Self {
            tx,
            waiters,
            upload_hashers: Mutex::new(HashMap::new()),
            remote_w: AtomicU16::new(init_w),
            remote_h: AtomicU16::new(init_h),
            download_cancel: Arc::new(AtomicBool::new(false)),
            manual_disconnect,
            cancel,
            info,
        };
        let mw = params.initial_max_width.unwrap_or(1280);
        let q = params.initial_quality.unwrap_or(68);
        let _ = client.set_stream_quality(mw, q);
        Ok(client)
    }

    pub fn cancel_transfer(&self) {
        self.download_cancel.store(true, Ordering::Release);
    }

    pub fn disconnect(&self) {
        self.manual_disconnect.store(true, Ordering::Release);
        self.cancel.notify_waiters();
    }

    fn send_fast(&self, msg: MsgType, ch: ChannelId, payload: Vec<u8>) -> Result<(), String> {
        self.tx
            .send((msg, ch, payload))
            .map_err(|_| "Connection closed".to_string())
    }

    async fn request_rpc(
        &self,
        req_type: MsgType,
        resp_type: MsgType,
        ch: ChannelId,
        payload: Vec<u8>,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, String> {
        let (otx, orx) = oneshot::channel();
        {
            let mut map = self.waiters.lock().await;
            map.entry(resp_type as u8).or_default().push_back(otx);
        }
        self.send_fast(req_type, ch, payload)?;
        timeout(Duration::from_millis(timeout_ms), orx)
            .await
            .map_err(|_| "RPC response timeout".to_string())?
            .map_err(|_| "Connection closed".to_string())
    }

    pub fn set_stream_quality(&self, max_w: u16, quality: u8) -> Result<(), String> {
        let mut p = vec![0u8; 3];
        p[0..2].copy_from_slice(&max_w.to_le_bytes());
        p[2] = quality;
        self.send_fast(MsgType::RequestKeyFrame, ChannelId::Control, p)
    }

    pub async fn select_monitor(&self, index: u16) -> Result<MonitorSelectAckPayload, String> {
        let resp = self
            .request_rpc(
                MsgType::MonitorSelect,
                MsgType::MonitorSelectAck,
                ChannelId::Control,
                index.to_le_bytes().to_vec(),
                5000,
            )
            .await?;
        let ack: MonitorSelectAckPayload =
            serde_json::from_slice(&resp).map_err(|e| e.to_string())?;
        if ack.ok {
            self.remote_w.store(ack.width, Ordering::Relaxed);
            self.remote_h.store(ack.height, Ordering::Relaxed);
        }
        Ok(ack)
    }

    pub fn mouse_event_norm(
        &self,
        action: u8,
        button: u8,
        norm_x: u16,
        norm_y: u16,
        wheel_delta: i16,
    ) -> Result<(), String> {
        let ev = MouseEventPacket {
            action,
            button,
            norm_x,
            norm_y,
            wheel_delta,
        };
        self.send_fast(
            MsgType::MouseEvent,
            ChannelId::Input,
            ev.serialize().to_vec(),
        )
    }

    pub fn mouse_event(
        &self,
        action: u8,
        button: u8,
        x: u32,
        y: u32,
        wheel_delta: i16,
    ) -> Result<(), String> {
        let w = self.remote_w.load(Ordering::Relaxed).max(1) as u64;
        let h = self.remote_h.load(Ordering::Relaxed).max(1) as u64;
        let nx = ((x as u64 * 65535) / w).min(65535) as u16;
        let ny = ((y as u64 * 65535) / h).min(65535) as u16;
        self.mouse_event_norm(action, button, nx, ny, wheel_delta)
    }

    pub fn click(&self, x: u32, y: u32, button: u8) -> Result<(), String> {
        self.mouse_event(1, button, x, y, 0)?;
        self.mouse_event(2, button, x, y, 0)
    }

    pub fn key_event(&self, action: u8, vk: u16, unicode_char: u32) -> Result<(), String> {
        self.key_event_ext(action, vk, 0, unicode_char)
    }

    pub fn key_event_ext(
        &self,
        action: u8,
        vk: u16,
        flags: u16,
        unicode_char: u32,
    ) -> Result<(), String> {
        let ev = KeyEventPacket {
            action,
            vk,
            scan_code: 0,
            flags,
            unicode_char,
        };
        self.send_fast(MsgType::KeyEvent, ChannelId::Input, ev.serialize().to_vec())
    }

    pub fn type_text(&self, text: &str) -> Result<(), String> {
        for ch in text.chars() {
            self.key_event(2, 0, ch as u32)?;
        }
        Ok(())
    }

    pub async fn screenshot(
        &self,
        max_w: u16,
        quality: u8,
        as_png: bool,
    ) -> Result<ScreenshotResult, String> {
        let mut req = vec![0u8; 4];
        req[0..2].copy_from_slice(&max_w.to_le_bytes());
        req[2] = quality;
        req[3] = if as_png { 1 } else { 0 };
        let resp = self
            .request_rpc(
                MsgType::ScreenshotReq,
                MsgType::ScreenshotResp,
                ChannelId::Control,
                req,
                6000,
            )
            .await?;
        if resp.len() < 13 {
            return Err("Short screenshot response".into());
        }
        let orig_w = u16::from_le_bytes([resp[0], resp[1]]);
        let orig_h = u16::from_le_bytes([resp[2], resp[3]]);
        let out_w = u16::from_le_bytes([resp[4], resp[5]]);
        let out_h = u16::from_le_bytes([resp[6], resp[7]]);
        let mime_type = if resp[8] == 1 {
            "image/png"
        } else {
            "image/jpeg"
        }
        .to_string();
        let len = u32::from_le_bytes([resp[9], resp[10], resp[11], resp[12]]) as usize;
        let img = &resp[13..(13 + len).min(resp.len())];
        Ok(ScreenshotResult {
            orig_w,
            orig_h,
            out_w,
            out_h,
            mime_type,
            data_base64: base64::engine::general_purpose::STANDARD.encode(img),
        })
    }

    pub async fn clipboard_get(&self) -> Result<String, String> {
        let resp = self
            .request_rpc(
                MsgType::ClipboardGet,
                MsgType::ClipboardData,
                ChannelId::Control,
                Vec::new(),
                5000,
            )
            .await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn clipboard_set(&self, text: &str) -> Result<(), String> {
        let _ = self
            .request_rpc(
                MsgType::ClipboardData,
                MsgType::ClipboardData,
                ChannelId::Control,
                text.as_bytes().to_vec(),
                5000,
            )
            .await?;
        Ok(())
    }

    pub async fn clipboard_get_image_base64(&self) -> Result<String, String> {
        let resp = self
            .request_rpc(
                MsgType::ClipboardImageGet,
                MsgType::ClipboardImageData,
                ChannelId::Control,
                Vec::new(),
                12_000,
            )
            .await?;
        if resp.len() < 8 || &resp[0..8] != b"\x89PNG\r\n\x1a\n" {
            return Ok(String::new());
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(&resp))
    }

    pub async fn clipboard_set_image_bytes(&self, png_bytes: Vec<u8>) -> Result<u32, String> {
        if png_bytes.len() < 8 || &png_bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
            return Err("Invalid PNG image bytes".into());
        }
        let resp = self
            .request_rpc(
                MsgType::ClipboardImageData,
                MsgType::ClipboardImageData,
                ChannelId::Control,
                png_bytes,
                15_000,
            )
            .await?;
        if resp.len() >= 4 {
            Ok(u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]))
        } else {
            Ok(0)
        }
    }

    pub async fn file_action(
        &self,
        action: &str,
        remote_path: &str,
        new_name: Option<String>,
    ) -> Result<String, String> {
        let req = FileActionPayload {
            action: action.to_string(),
            path: remote_path.to_string(),
            new_name,
        };
        let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        let resp = self
            .request_rpc(
                MsgType::FileActionReq,
                MsgType::FileActionResp,
                ChannelId::File,
                payload,
                8_000,
            )
            .await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn list_dir(&self, rel: &str) -> Result<String, String> {
        let resp = self
            .request_rpc(
                MsgType::FileListReq,
                MsgType::FileListResp,
                ChannelId::File,
                rel.as_bytes().to_vec(),
                5000,
            )
            .await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn upload_bytes(&self, remote_path: &str, data: &[u8]) -> Result<String, String> {
        let upload_id = format!("mem-{:08x}", rand::random::<u32>());
        let total = data.len() as u64;
        if data.is_empty() {
            return self
                .upload_chunk(&upload_id, remote_path, 0, 0, true, &[])
                .await;
        }
        let mut offset = 0usize;
        let mut last_resp = String::new();
        while offset < data.len() {
            let end = (offset + FILE_CHUNK_SIZE).min(data.len());
            let eof = end >= data.len();
            last_resp = self
                .upload_chunk(
                    &upload_id,
                    remote_path,
                    offset as u64,
                    total,
                    eof,
                    &data[offset..end],
                )
                .await?;
            offset = end;
        }
        Ok(last_resp)
    }

    pub async fn upload_chunk(
        &self,
        upload_id: &str,
        remote_path: &str,
        offset: u64,
        total_size: u64,
        eof: bool,
        chunk: &[u8],
    ) -> Result<String, String> {
        let expected_sha256 = {
            let mut map = self.upload_hashers.lock().await;
            if offset == 0 {
                map.insert(upload_id.to_string(), Sha256::new());
            }
            let hasher = map.entry(upload_id.to_string()).or_insert_with(Sha256::new);
            hasher.update(chunk);
            if eof {
                let finished = map.remove(upload_id).unwrap_or_else(Sha256::new);
                Some(hex::encode(finished.finalize()))
            } else {
                None
            }
        };

        let meta = UploadChunkMeta {
            upload_id: upload_id.to_string(),
            path: remote_path.to_string(),
            offset,
            total_size,
            eof,
            expected_sha256,
        };
        let meta_bytes = serde_json::to_vec(&meta).map_err(|e| e.to_string())?;
        let mut payload = Vec::with_capacity(2 + meta_bytes.len() + chunk.len());
        payload.extend_from_slice(&(meta_bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(&meta_bytes);
        payload.extend_from_slice(chunk);

        let resp = self
            .request_rpc(
                MsgType::FileUploadChunk,
                MsgType::FileUploadChunkAck,
                ChannelId::File,
                payload,
                20_000,
            )
            .await?;
        let resp_str = String::from_utf8_lossy(&resp).to_string();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&resp_str) {
            if val.get("ok").and_then(|v| v.as_bool()) == Some(false) {
                let err = val
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Upload chunk rejected");
                return Err(err.to_string());
            }
        }
        Ok(resp_str)
    }

    pub async fn download_file_to_path<P>(
        &self,
        remote_path: &str,
        local_dest: &Path,
        mut on_progress: P,
    ) -> Result<DownloadResult, String>
    where
        P: FnMut(u64, u64),
    {
        if let Some(parent) = local_dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        self.download_cancel.store(false, Ordering::Release);
        let part_path = PathBuf::from(format!("{}.ctdpart", local_dest.to_string_lossy()));
        let mut out_file = std::fs::File::create(&part_path).map_err(|e| e.to_string())?;
        let mut full_hasher = Sha256::new();
        let mut offset = 0u64;

        loop {
            if self.download_cancel.load(Ordering::Acquire) {
                drop(out_file);
                let _ = std::fs::remove_file(&part_path);
                return Err("传输已取消".into());
            }
            let req = DownloadChunkReq {
                path: remote_path.to_string(),
                offset,
                max_bytes: FILE_CHUNK_SIZE as u32,
            };
            let req_bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
            let resp = self
                .request_rpc(
                    MsgType::FileDownloadChunkReq,
                    MsgType::FileDownloadChunkResp,
                    ChannelId::File,
                    req_bytes,
                    20_000,
                )
                .await?;
            if resp.is_empty() {
                let _ = std::fs::remove_file(&part_path);
                return Err("Empty download response".into());
            }
            if resp[0] == 0 {
                let _ = std::fs::remove_file(&part_path);
                return Err(String::from_utf8_lossy(&resp[1..]).to_string());
            }
            if resp.len() < 50 {
                let _ = std::fs::remove_file(&part_path);
                return Err("Truncated download chunk header".into());
            }
            let total_size = u64::from_le_bytes(resp[1..9].try_into().unwrap());
            let eof = resp[17] == 1;
            let expected_chunk_hash = &resp[18..50];
            let chunk = &resp[50..];

            let actual_chunk_hash = Sha256::digest(chunk);
            if actual_chunk_hash.as_slice() != expected_chunk_hash {
                let _ = std::fs::remove_file(&part_path);
                return Err("Download chunk SHA-256 verification failed".into());
            }

            if !chunk.is_empty() {
                out_file.write_all(chunk).map_err(|e| e.to_string())?;
                full_hasher.update(chunk);
                offset += chunk.len() as u64;
            }
            on_progress(offset, total_size);

            if eof || offset >= total_size {
                break;
            }
        }

        out_file.flush().map_err(|e| e.to_string())?;
        drop(out_file);

        if local_dest.exists() {
            let _ = std::fs::remove_file(local_dest);
        }
        std::fs::rename(&part_path, local_dest).map_err(|e| e.to_string())?;

        Ok(DownloadResult {
            ok: true,
            remote_path: remote_path.to_string(),
            local_path: local_dest.to_string_lossy().to_string(),
            bytes: offset,
            sha256: hex::encode(full_hasher.finalize()),
        })
    }

    pub async fn exec_guarded(&self, command: &str, timeout_ms: u32) -> Result<String, String> {
        let body =
            serde_json::json!({ "command": command, "timeoutMs": timeout_ms }).to_string();
        let resp = self
            .request_rpc(
                MsgType::ExecReq,
                MsgType::ExecResp,
                ChannelId::Control,
                body.into_bytes(),
                12000,
            )
            .await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.manual_disconnect.store(true, Ordering::Release);
        self.cancel.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostConfig, HostHandle};

    #[tokio::test]
    async fn e2ee_dual_token_monitor_switch_and_chunked_file_roundtrip() {
        let tmp_root = std::env::temp_dir().join(format!("ctd-test-{}", rand::random::<u32>()));
        let host_files = tmp_root.join("host");
        let client_dl = tmp_root.join("downloads");
        std::fs::create_dir_all(&host_files).unwrap();
        std::fs::create_dir_all(&client_dl).unwrap();

        let cfg = HostConfig {
            bind_addr: "127.0.0.1:0".into(),
            token: "p2-temp-secret".into(),
            unattended_token: "p2-unattended-perm".into(),
            device_id: "999888777".into(),
            device_secret: "owner-secret-123".into(),
            device_name: "TestHost".into(),
            relay_addr: None,
            fps: 20,
            jpeg_quality: 70,
            force_synthetic: true,
            file_root: host_files.clone(),
            allow_input: false,
            allow_exec: true,
            exec_allowlist: vec!["echo".into()],
            prevent_sleep: false,
            auto_clipboard_sync: false,
        };
        let host = HostHandle::start(cfg).await.unwrap();
        let addr = host.status().bind_addr;

        // 1. Wrong password must fail E2EE auth
        let bad_params = ConnectParams {
            direct_addr: Some(addr.clone()),
            relay_addr: None,
            device_id: None,
            token: "wrong-pass".into(),
            client_name: Some("BadClient".into()),
            initial_max_width: Some(640),
            initial_quality: Some(60),
        };
        assert!(
            RemoteClient::connect_streaming(&bad_params, |_| {}, |_| {}, |_| {}, |_| {}, |_| {})
                .await
                .is_err()
        );

        // 2. Unattended permanent password establishes E2EE session & lists 2 synthetic monitors
        let perm_params = ConnectParams {
            token: "p2-unattended-perm".into(),
            client_name: Some("UnattendedViewer".into()),
            ..bad_params
        };
        let disc_fired = Arc::new(AtomicBool::new(false));
        let disc_flag = disc_fired.clone();
        let clip_pushed = Arc::new(Mutex::new(String::new()));
        let clip_pushed_cb = clip_pushed.clone();
        let got_raw_frame = Arc::new(AtomicBool::new(false));
        let got_raw_cb = got_raw_frame.clone();
        let client = RemoteClient::connect_streaming(
            &perm_params,
            move |raw| {
                if RemoteClient::decode_raw_frame_to_event(&raw).is_some() {
                    got_raw_cb.store(true, Ordering::Release);
                }
            },
            |_| {},
            |_| {},
            move |txt| {
                if let Ok(mut g) = clip_pushed_cb.try_lock() {
                    *g = txt;
                }
            },
            move |_| {
                disc_flag.store(true, Ordering::Release);
            },
        )
        .await
        .unwrap();
        assert!(client.info.encrypted);
        assert_eq!(client.info.ack.monitors.len(), 2);
        assert_eq!(client.info.ack.width, 1280);
        assert_eq!(host.status().active_sessions, vec!["UnattendedViewer".to_string()]);

        // 3. Hot-switch to Monitor #1 (1024x768) without reconnecting!
        let mon_ack = client.select_monitor(1).await.unwrap();
        assert!(mon_ack.ok);
        assert_eq!(mon_ack.active_monitor, 1);
        assert_eq!(mon_ack.width, 1024);
        assert_eq!(mon_ack.height, 768);

        // 4. Multi-chunk binary upload + download verification
        let big_data: Vec<u8> = (0..(600 * 1024)).map(|i| ((i * 31 + 7) & 0xff) as u8).collect();
        let expected_sha = hex::encode(Sha256::digest(&big_data));

        let up_json = client
            .upload_bytes("sub/big_binary.bin", &big_data)
            .await
            .unwrap();
        let up_val: serde_json::Value = serde_json::from_str(&up_json).unwrap();
        assert_eq!(up_val["ok"], true);
        assert_eq!(up_val["sha256"], expected_sha);

        let dl_target = client_dl.join("big_binary.bin");
        let dl_res = client
            .download_file_to_path("sub/big_binary.bin", &dl_target, |_, _| {})
            .await
            .unwrap();
        assert_eq!(dl_res.bytes, big_data.len() as u64);
        assert_eq!(dl_res.sha256, expected_sha);
        assert_eq!(std::fs::read(&dl_target).unwrap(), big_data);

        // 5. Sandbox file actions (mkdir + rename + delete) & PNG image clipboard sync
        let mk = client.file_action("mkdir", "docs_v070", None).await.unwrap();
        assert!(mk.contains("\"ok\":true"));
        let rn = client
            .file_action("rename", "sub/big_binary.bin", Some("renamed.bin".into()))
            .await
            .unwrap();
        assert!(rn.contains("\"ok\":true"));
        let rm = client
            .file_action("delete", "sub/renamed.bin", None)
            .await
            .unwrap();
        assert!(rm.contains("\"ok\":true"));

        let (_, _, mini_png) =
            crate::codec::encode_full_screenshot(&vec![200u8; 4 * 4 * 4], 4, 4, 0, 80, true)
                .unwrap();
        let ack_len = client.clipboard_set_image_bytes(mini_png.clone()).await.unwrap();
        assert_eq!(ack_len, mini_png.len() as u32);
        let got_b64 = client.clipboard_get_image_base64().await.unwrap();
        assert!(!got_b64.is_empty());

        // 5b. Passive ClipboardPush delta delivery + binary frame verification
        host.push_clipboard_text("auto-copied-on-remote-v080");
        for _ in 0..25 {
            if clip_pushed.lock().await.as_str() == "auto-copied-on-remote-v080"
                && got_raw_frame.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(clip_pushed.lock().await.as_str(), "auto-copied-on-remote-v080");
        assert!(got_raw_frame.load(Ordering::Acquire));

        // 6. Kicking viewers from Host triggers remote disconnect callback immediately
        host.kick_viewers();
        for _ in 0..20 {
            if disc_fired.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(disc_fired.load(Ordering::Acquire));

        client.disconnect();
        host.stop();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }
}
