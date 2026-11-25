//! Cross-platform async Client (`RemoteClient`, v0.4.0 P1):
//! - End-to-End Encryption (`X25519` + `HKDF-SHA256` + `ChaCha20-Poly1305`) negotiated automatically
//! - Smart LAN-first P2P direct race (`LOOKUP` -> 380ms LAN probe -> seamless Relay fallback)
//! - Lock-free split `OwnedReadHalf` / `OwnedWriteHalf` (0ms non-blocking mouse/key input)
//! - Streaming 256 KB chunked upload & download with incremental SHA-256 verification

use crate::capture::now_us;
use base64::Engine;
use ctd_protocol::{
    connect_via_relay, frame_flags, lookup_lan_via_relay, read_frame, read_frame_opt_cipher,
    write_frame, write_frame_opt_cipher, ChannelId, DownloadChunkReq, E2eeClientHandshake,
    HelloAckPayload, HelloPayload, KeyEventPacket, MouseEventPacket, MsgType, RecvCipher,
    SendCipher, TileEncoding, UploadChunkMeta, VideoTileFramePacket, FILE_CHUNK_SIZE,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
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

        // 1. Smart LAN-First P2P Race vs Relay / Direct (with full E2EE handshake)
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

        let info = ConnectInfo {
            ack,
            route,
            endpoint,
            encrypted: true,
        };
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<(MsgType, ChannelId, Vec<u8>)>();
        let waiters: RpcWaiters = Arc::new(Mutex::new(HashMap::new()));
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
        tokio::spawn(async move {
            loop {
                let frame_res = tokio::select! {
                    _ = cancel_rd.notified() => break,
                    r = read_frame_opt_cipher(&mut rd, Some(&mut recv_cipher)) => r,
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
                                        data_base64: base64::engine::general_purpose::STANDARD
                                            .encode(&t.data),
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
            cancel,
            info,
        };
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
        let nx = ((x as u64 * 65535) / (self.info.ack.width.max(1) as u64)).min(65535) as u16;
        let ny = ((y as u64 * 65535) / (self.info.ack.height.max(1) as u64)).min(65535) as u16;
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
        let part_path = PathBuf::from(format!("{}.ctdpart", local_dest.to_string_lossy()));
        let mut out_file = std::fs::File::create(&part_path).map_err(|e| e.to_string())?;
        let mut full_hasher = Sha256::new();
        let mut offset = 0u64;

        loop {
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
        self.cancel.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{HostConfig, HostHandle};

    #[tokio::test]
    async fn e2ee_session_and_chunked_file_transfer_roundtrip() {
        let tmp_root = std::env::temp_dir()
            .join(format!("ctd-test-{}", rand::random::<u32>()));
        let host_files = tmp_root.join("host");
        let client_dl = tmp_root.join("downloads");
        std::fs::create_dir_all(&host_files).unwrap();
        std::fs::create_dir_all(&client_dl).unwrap();

        let cfg = HostConfig {
            bind_addr: "127.0.0.1:0".into(),
            token: "p1-e2ee-secret".into(),
            device_id: "999888777".into(),
            device_secret: "owner-secret-123".into(),
            device_name: "TestHost".into(),
            relay_addr: None,
            fps: 10,
            jpeg_quality: 70,
            force_synthetic: true,
            file_root: host_files.clone(),
            allow_input: false,
            allow_exec: true,
            exec_allowlist: vec!["echo".into()],
        };
        let host = HostHandle::start(cfg).await.unwrap();
        let addr = host.status().bind_addr;

        // 1. Wrong password must fail E2EE auth
        let bad_params = ConnectParams {
            direct_addr: Some(addr.clone()),
            relay_addr: None,
            device_id: None,
            token: "wrong-pass".into(),
            client_name: Some("TestClient".into()),
            initial_max_width: Some(640),
            initial_quality: Some(60),
        };
        assert!(RemoteClient::connect_streaming(&bad_params, |_| {}, |_| {})
            .await
            .is_err());

        // 2. Correct password establishes E2EE session
        let ok_params = ConnectParams {
            token: "p1-e2ee-secret".into(),
            ..bad_params
        };
        let client = RemoteClient::connect_streaming(&ok_params, |_| {}, |_| {})
            .await
            .unwrap();
        assert!(client.info.encrypted);

        // 3. Multi-chunk binary upload (> 600 KB -> 3 chunks of 256 KB) + download verification
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

        client.disconnect();
        host.stop();
        let _ = std::fs::remove_dir_all(&tmp_root);
    }
}
