//! Cross-platform async Client (`RemoteClient`, v0.3.0 P0):
//! - Smart LAN-first P2P direct race (`LOOKUP` -> 400ms LAN probe -> seamless Relay fallback)
//! - Lock-free split `OwnedReadHalf` / `OwnedWriteHalf` (0ms non-blocking mouse/key input!)
//! - Background incremental `VideoTileFrame` push decoder + continuous 2s RTT `Ping/Pong` probe

use crate::capture::now_us;
use base64::Engine;
use ctd_protocol::{
    connect_via_relay, frame_flags, lookup_lan_via_relay, read_frame, write_frame, ChannelId,
    HelloAckPayload, HelloPayload, KeyEventPacket, MouseEventPacket, MsgType, TileEncoding,
    VideoTileFramePacket,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{Duration, timeout};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TileItem {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub mime: &'static str,
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

type RpcWaiters = Arc<Mutex<HashMap<u8, VecDeque<oneshot::Sender<Vec<u8>>>>>>;

pub struct RemoteClient {
    tx: mpsc::UnboundedSender<(MsgType, ChannelId, Vec<u8>)>,
    waiters: RpcWaiters,
    cancel: Arc<Notify>,
    pub info: ConnectInfo,
}

async fn perform_hello(
    mut stream: TcpStream,
    token: &str,
    client_name: &str,
) -> Result<(TcpStream, HelloAckPayload), String> {
    let hello = HelloPayload {
        client_name: client_name.to_string(),
        token: token.to_string(),
    };
    let hello_bytes = serde_json::to_vec(&hello).map_err(|e| e.to_string())?;
    write_frame(&mut stream, MsgType::Hello, ChannelId::Control, 0, 1, &hello_bytes)
        .await
        .map_err(|e| e.to_string())?;

    let (hdr, payload) = timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .map_err(|_| "Handshake timeout".to_string())?
        .map_err(|e| e.to_string())?;
    if hdr.msg_type != MsgType::HelloAck {
        return Err("Handshake rejected (wrong password or invalid response)".into());
    }
    let ack: HelloAckPayload = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
    Ok((stream, ack))
}

impl RemoteClient {
    pub async fn connect_streaming<F, R>(
        params: &ConnectParams,
        on_frame: F,
        on_rtt: R,
    ) -> Result<Self, String>
    where
        F: Fn(TilePushEvent) + Send + Sync + 'static,
        R: Fn(f64) + Send + Sync + 'static,
    {
        let cname = params
            .client_name
            .as_deref()
            .unwrap_or("CopyToDesk-Desktop");

        // 1. Smart LAN-First P2P Race vs Relay / Direct
        let (stream, ack, route, endpoint) =
            if let (Some(relay), Some(dev)) = (&params.relay_addr, &params.device_id) {
                if !relay.is_empty() && !dev.is_empty() {
                    let mut lan_hit = None;
                    let lan_candidates = lookup_lan_via_relay(relay, dev).await;
                    for cand in lan_candidates {
                        if let Ok(Ok(s)) = timeout(Duration::from_millis(380), TcpStream::connect(&cand)).await {
                            let _ = s.set_nodelay(true);
                            if let Ok(Ok((s2, ack))) = timeout(Duration::from_millis(600), perform_hello(s, &params.token, cname)).await {
                                lan_hit = Some((s2, ack, "lan-p2p".to_string(), cand));
                                break;
                            }
                        }
                    }
                    if let Some(hit) = lan_hit {
                        hit
                    } else {
                        let s = connect_via_relay(relay, dev).await.map_err(|e| e.to_string())?;
                        let (s2, ack) = perform_hello(s, &params.token, cname).await?;
                        (s2, ack, "relay".to_string(), relay.clone())
                    }
                } else {
                    let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
                    let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
                    let _ = s.set_nodelay(true);
                    let (s2, ack) = perform_hello(s, &params.token, cname).await?;
                    (s2, ack, "direct".to_string(), addr.to_string())
                }
            } else {
                let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
                let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
                let _ = s.set_nodelay(true);
                let (s2, ack) = perform_hello(s, &params.token, cname).await?;
                (s2, ack, "direct".to_string(), addr.to_string())
            };

        let info = ConnectInfo { ack, route, endpoint };
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<(MsgType, ChannelId, Vec<u8>)>();
        let waiters: RpcWaiters = Arc::new(Mutex::new(HashMap::new()));
        let cancel = Arc::new(Notify::new());

        // 2. Dedicated Lock-Free Writer Task (mouse/key/control write immediately with zero read-blocking!)
        let cancel_wr = cancel.clone();
        tokio::spawn(async move {
            let mut seq = 1u32;
            loop {
                tokio::select! {
                    _ = cancel_wr.notified() => break,
                    item = rx.recv() => {
                        let Some((mtype, ch, payload)) = item else { break };
                        seq = seq.wrapping_add(1);
                        if write_frame(&mut wr, mtype, ch, 0, seq, &payload).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // 3. Dedicated Continuous Reader Task (pushes VideoTileFrame + resolves RPC oneshots)
        let cancel_rd = cancel.clone();
        let waiters_rd = waiters.clone();
        tokio::spawn(async move {
            loop {
                let frame_res = tokio::select! {
                    _ = cancel_rd.notified() => break,
                    r = read_frame(&mut rd) => r,
                };
                let Ok((hdr, payload)) = frame_res else { break };

                match hdr.msg_type {
                    MsgType::VideoTileFrame => {
                        let byte_len = payload.len() as u32;
                        if let Ok(pkt) = VideoTileFramePacket::deserialize(&payload) {
                            let tiles = pkt
                                .tiles
                                .into_iter()
                                .map(|t| {
                                    let (mime, is_lossless) = match t.encoding {
                                        TileEncoding::PngLossless => ("image/png", true),
                                        _ => ("image/jpeg", false),
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
                            on_frame(TilePushEvent {
                                frame_id: pkt.frame_id,
                                screen_w: pkt.screen_w,
                                screen_h: pkt.screen_h,
                                encode_us: pkt.encode_us,
                                byte_len,
                                is_key_frame: (hdr.flags & frame_flags::KEY_FRAME) != 0,
                                has_lossless: (hdr.flags & frame_flags::LOSSLESS_REFINEMENT) != 0,
                                tiles,
                            });
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
        });

        // 4. Periodic 2s Ping Task + Initial Stream Quality / KeyFrame request
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

        let client = Self { tx, waiters, cancel, info };
        let mw = params.initial_max_width.unwrap_or(1280);
        let q = params.initial_quality.unwrap_or(68);
        let _ = client.set_stream_quality(mw, q);
        Ok(client)
    }

    pub fn disconnect(&self) {
        self.cancel.notify_waiters();
    }

    fn send_fast(&self, msg: MsgType, ch: ChannelId, payload: Vec<u8>) -> Result<(), String> {
        self.tx
            .send((msg, ch, payload))
            .map_err(|_| "Connection closed".to_string())
    }

    async fn request_rpc(&self, req_type: MsgType, resp_type: MsgType, ch: ChannelId, payload: Vec<u8>, timeout_ms: u64) -> Result<Vec<u8>, String> {
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

    pub fn mouse_event_norm(&self, action: u8, button: u8, norm_x: u16, norm_y: u16, wheel_delta: i16) -> Result<(), String> {
        let ev = MouseEventPacket { action, button, norm_x, norm_y, wheel_delta };
        self.send_fast(MsgType::MouseEvent, ChannelId::Input, ev.serialize().to_vec())
    }

    pub fn mouse_event(&self, action: u8, button: u8, x: u32, y: u32, wheel_delta: i16) -> Result<(), String> {
        let nx = ((x as u64 * 65535) / (self.info.ack.width.max(1) as u64)).min(65535) as u16;
        let ny = ((y as u64 * 65535) / (self.info.ack.height.max(1) as u64)).min(65535) as u16;
        self.mouse_event_norm(action, button, nx, ny, wheel_delta)
    }

    pub fn click(&self, x: u32, y: u32, button: u8) -> Result<(), String> {
        self.mouse_event(1, button, x, y, 0)?;
        self.mouse_event(2, button, x, y, 0)
    }

    pub fn key_event(&self, action: u8, vk: u16, unicode_char: u32) -> Result<(), String> {
        let ev = KeyEventPacket { action, vk, scan_code: 0, flags: 0, unicode_char };
        self.send_fast(MsgType::KeyEvent, ChannelId::Input, ev.serialize().to_vec())
    }

    pub fn type_text(&self, text: &str) -> Result<(), String> {
        for ch in text.chars() {
            self.key_event(2, 0, ch as u32)?;
        }
        Ok(())
    }

    pub async fn screenshot(&self, max_w: u16, quality: u8, as_png: bool) -> Result<ScreenshotResult, String> {
        let mut req = vec![0u8; 4];
        req[0..2].copy_from_slice(&max_w.to_le_bytes());
        req[2] = quality;
        req[3] = if as_png { 1 } else { 0 };
        let resp = self.request_rpc(MsgType::ScreenshotReq, MsgType::ScreenshotResp, ChannelId::Control, req, 6000).await?;
        if resp.len() < 13 {
            return Err("Short screenshot response".into());
        }
        let orig_w = u16::from_le_bytes([resp[0], resp[1]]);
        let orig_h = u16::from_le_bytes([resp[2], resp[3]]);
        let out_w = u16::from_le_bytes([resp[4], resp[5]]);
        let out_h = u16::from_le_bytes([resp[6], resp[7]]);
        let mime_type = if resp[8] == 1 { "image/png" } else { "image/jpeg" }.to_string();
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
        let resp = self.request_rpc(MsgType::ClipboardGet, MsgType::ClipboardData, ChannelId::Control, Vec::new(), 5000).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn clipboard_set(&self, text: &str) -> Result<(), String> {
        let _ = self.request_rpc(MsgType::ClipboardData, MsgType::ClipboardData, ChannelId::Control, text.as_bytes().to_vec(), 5000).await?;
        Ok(())
    }

    pub async fn list_dir(&self, rel: &str) -> Result<String, String> {
        let resp = self.request_rpc(MsgType::FileListReq, MsgType::FileListResp, ChannelId::File, rel.as_bytes().to_vec(), 5000).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn upload_bytes(&self, remote_path: &str, data: &[u8]) -> Result<String, String> {
        let sha = hex::encode(Sha256::digest(data));
        let path_bytes = remote_path.as_bytes();
        let mut payload = Vec::with_capacity(2 + path_bytes.len() + 64 + data.len());
        payload.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(path_bytes);
        payload.extend_from_slice(sha.as_bytes());
        payload.extend_from_slice(data);
        let resp = self.request_rpc(MsgType::FileUploadReq, MsgType::FileUploadResp, ChannelId::File, payload, 10000).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn exec_guarded(&self, command: &str, timeout_ms: u32) -> Result<String, String> {
        let body = serde_json::json!({ "command": command, "timeoutMs": timeout_ms }).to_string();
        let resp = self.request_rpc(MsgType::ExecReq, MsgType::ExecResp, ChannelId::Control, body.into_bytes(), 12000).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        self.cancel.notify_waiters();
    }
}
