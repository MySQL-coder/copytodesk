//! Cross-platform async Client (`RemoteClient`) used by Tauri 2 desktop commands and CLI tools.

use base64::Engine;
use ctd_protocol::{
    connect_via_relay, read_frame, write_frame, ChannelId, HelloAckPayload, HelloPayload,
    KeyEventPacket, MouseEventPacket, MsgType,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub direct_addr: Option<String>,
    pub relay_addr: Option<String>,
    pub device_id: Option<String>,
    pub token: String,
    pub client_name: Option<String>,
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

pub struct RemoteClient {
    stream: TcpStream,
    seq: u32,
    pub ack: HelloAckPayload,
}

impl RemoteClient {
    pub async fn connect(params: &ConnectParams) -> Result<Self, String> {
        let mut stream = if let (Some(relay), Some(dev)) = (&params.relay_addr, &params.device_id) {
            if !relay.is_empty() && !dev.is_empty() {
                connect_via_relay(relay, dev).await.map_err(|e| e.to_string())?
            } else {
                let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
                let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
                let _ = s.set_nodelay(true);
                s
            }
        } else {
            let addr = params.direct_addr.as_deref().unwrap_or("127.0.0.1:19800");
            let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            let _ = s.set_nodelay(true);
            s
        };

        let hello = HelloPayload {
            client_name: params.client_name.clone().unwrap_or_else(|| "CopyToDesk-Tauri".into()),
            token: params.token.clone(),
        };
        let hello_bytes = serde_json::to_vec(&hello).map_err(|e| e.to_string())?;
        write_frame(&mut stream, MsgType::Hello, ChannelId::Control, 0, 1, &hello_bytes)
            .await
            .map_err(|e| e.to_string())?;

        let (hdr, payload) = read_frame(&mut stream).await.map_err(|e| e.to_string())?;
        if hdr.msg_type != MsgType::HelloAck {
            return Err("Handshake rejected".into());
        }
        let ack: HelloAckPayload = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        Ok(Self { stream, seq: 1, ack })
    }

    async fn send(&mut self, msg: MsgType, ch: ChannelId, payload: &[u8]) -> Result<(), String> {
        self.seq = self.seq.wrapping_add(1);
        write_frame(&mut self.stream, msg, ch, 0, self.seq, payload)
            .await
            .map_err(|e| e.to_string())
    }

    async fn wait_for(&mut self, target: MsgType) -> Result<Vec<u8>, String> {
        for _ in 0..64 {
            let (hdr, payload) = read_frame(&mut self.stream).await.map_err(|e| e.to_string())?;
            if hdr.msg_type == target {
                return Ok(payload);
            }
        }
        Err("Timed out waiting for response frame".into())
    }

    pub async fn screenshot(&mut self, max_w: u16, quality: u8, as_png: bool) -> Result<ScreenshotResult, String> {
        let mut req = [0u8; 4];
        req[0..2].copy_from_slice(&max_w.to_le_bytes());
        req[2] = quality;
        req[3] = if as_png { 1 } else { 0 };
        self.send(MsgType::ScreenshotReq, ChannelId::Control, &req).await?;
        let resp = self.wait_for(MsgType::ScreenshotResp).await?;
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

    pub async fn click(&mut self, x: u32, y: u32, button: u8) -> Result<(), String> {
        let nx = ((x as u64 * 65535) / (self.ack.width.max(1) as u64)).min(65535) as u16;
        let ny = ((y as u64 * 65535) / (self.ack.height.max(1) as u64)).min(65535) as u16;
        for act in [1u8, 2u8] {
            let ev = MouseEventPacket { action: act, button, norm_x: nx, norm_y: ny, wheel_delta: 0 };
            self.send(MsgType::MouseEvent, ChannelId::Input, &ev.serialize()).await?;
        }
        Ok(())
    }

    pub async fn type_text(&mut self, text: &str) -> Result<(), String> {
        for ch in text.chars() {
            let ev = KeyEventPacket { action: 2, vk: 0, scan_code: 0, flags: 0, unicode_char: ch as u32 };
            self.send(MsgType::KeyEvent, ChannelId::Input, &ev.serialize()).await?;
        }
        Ok(())
    }

    pub async fn clipboard_get(&mut self) -> Result<String, String> {
        self.send(MsgType::ClipboardGet, ChannelId::Control, &[]).await?;
        let resp = self.wait_for(MsgType::ClipboardData).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn clipboard_set(&mut self, text: &str) -> Result<(), String> {
        self.send(MsgType::ClipboardData, ChannelId::Control, text.as_bytes()).await?;
        let _ = self.wait_for(MsgType::ClipboardData).await?;
        Ok(())
    }

    pub async fn list_dir(&mut self, rel: &str) -> Result<String, String> {
        self.send(MsgType::FileListReq, ChannelId::File, rel.as_bytes()).await?;
        let resp = self.wait_for(MsgType::FileListResp).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn upload_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<String, String> {
        let sha = hex::encode(Sha256::digest(data));
        let path_bytes = remote_path.as_bytes();
        let mut payload = Vec::with_capacity(2 + path_bytes.len() + 64 + data.len());
        payload.extend_from_slice(&(path_bytes.len() as u16).to_le_bytes());
        payload.extend_from_slice(path_bytes);
        payload.extend_from_slice(sha.as_bytes());
        payload.extend_from_slice(data);
        self.send(MsgType::FileUploadReq, ChannelId::File, &payload).await?;
        let resp = self.wait_for(MsgType::FileUploadResp).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }

    pub async fn exec_guarded(&mut self, command: &str, timeout_ms: u32) -> Result<String, String> {
        let body = serde_json::json!({ "command": command, "timeoutMs": timeout_ms }).to_string();
        self.send(MsgType::ExecReq, ChannelId::Control, body.as_bytes()).await?;
        let resp = self.wait_for(MsgType::ExecResp).await?;
        Ok(String::from_utf8_lossy(&resp).to_string())
    }
}
