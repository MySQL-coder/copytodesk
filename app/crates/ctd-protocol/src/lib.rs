//! Wire-compatible Rust implementation of the CopyToDesk `CTD1` v1 binary protocol.
//! Byte-for-byte compatible with the Windows C# MVP Host/Viewer, `signaling-relay.mjs`, and `mcp-server/index.mjs`.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub const MAGIC: u32 = 0x3144_5443; // "CTD1" little-endian
pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 16;
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid protocol magic: 0x{0:08x}")]
    InvalidMagic(u32),
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u8),
    #[error("payload exceeds 16MB limit: {0}")]
    PayloadTooLarge(u32),
    #[error("unknown message type: 0x{0:02x}")]
    UnknownMsgType(u8),
    #[error("truncated packet: {0}")]
    Truncated(&'static str),
    #[error("relay handshake failed: {0}")]
    RelayHandshake(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Hello = 0x01,
    HelloAck = 0x02,
    Ping = 0x03,
    Pong = 0x04,
    VideoTileFrame = 0x10,
    RequestKeyFrame = 0x11,
    CursorState = 0x20,
    MouseEvent = 0x30,
    KeyEvent = 0x31,
    ScreenshotReq = 0x40,
    ScreenshotResp = 0x41,
    ClipboardGet = 0x50,
    ClipboardData = 0x51,
    FileListReq = 0x60,
    FileListResp = 0x61,
    FileUploadReq = 0x62,
    FileUploadResp = 0x63,
    FileDownloadReq = 0x64,
    FileDownloadResp = 0x65,
    ExecReq = 0x70,
    ExecResp = 0x71,
}

impl TryFrom<u8> for MsgType {
    type Error = ProtocolError;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x03 => Ok(Self::Ping),
            0x04 => Ok(Self::Pong),
            0x10 => Ok(Self::VideoTileFrame),
            0x11 => Ok(Self::RequestKeyFrame),
            0x20 => Ok(Self::CursorState),
            0x30 => Ok(Self::MouseEvent),
            0x31 => Ok(Self::KeyEvent),
            0x40 => Ok(Self::ScreenshotReq),
            0x41 => Ok(Self::ScreenshotResp),
            0x50 => Ok(Self::ClipboardGet),
            0x51 => Ok(Self::ClipboardData),
            0x60 => Ok(Self::FileListReq),
            0x61 => Ok(Self::FileListResp),
            0x62 => Ok(Self::FileUploadReq),
            0x63 => Ok(Self::FileUploadResp),
            0x64 => Ok(Self::FileDownloadReq),
            0x65 => Ok(Self::FileDownloadResp),
            0x70 => Ok(Self::ExecReq),
            0x71 => Ok(Self::ExecResp),
            other => Err(ProtocolError::UnknownMsgType(other)),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelId {
    Control = 0,
    Video = 1,
    Cursor = 2,
    Input = 3,
    Audio = 4,
    File = 5,
}

pub mod frame_flags {
    pub const NONE: u8 = 0;
    pub const KEY_FRAME: u8 = 1 << 0;
    pub const LOSSLESS_REFINEMENT: u8 = 1 << 1;
    pub const COMPRESSED: u8 = 1 << 2;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileEncoding {
    RawBgra = 0,
    JpegLossy = 1,
    PngLossless = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub msg_type: MsgType,
    pub flags: u8,
    pub channel: u8,
    pub payload_len: u32,
    pub seq_id: u32,
}

impl FrameHeader {
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = self.version;
        buf[5] = self.msg_type as u8;
        buf[6] = self.flags;
        buf[7] = self.channel;
        buf[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq_id.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; HEADER_SIZE]) -> Result<Self, ProtocolError> {
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != MAGIC {
            return Err(ProtocolError::InvalidMagic(magic));
        }
        let version = buf[4];
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let msg_type = MsgType::try_from(buf[5])?;
        let payload_len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        if (payload_len as usize) > MAX_PAYLOAD_BYTES {
            return Err(ProtocolError::PayloadTooLarge(payload_len));
        }
        let seq_id = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Ok(Self {
            version,
            msg_type,
            flags: buf[6],
            channel: buf[7],
            payload_len,
            seq_id,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EncodedTile {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub encoding: TileEncoding,
    pub flags: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct VideoTileFramePacket {
    pub capture_ts_us: u64,
    pub frame_id: u32,
    pub screen_w: u16,
    pub screen_h: u16,
    pub tile_size: u16,
    pub encode_us: u32,
    pub tiles: Vec<EncodedTile>,
}

impl VideoTileFramePacket {
    pub fn serialize(&self) -> Vec<u8> {
        let total = 24 + self.tiles.iter().map(|t| 14 + t.data.len()).sum::<usize>();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&self.capture_ts_us.to_le_bytes());
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.screen_w.to_le_bytes());
        out.extend_from_slice(&self.screen_h.to_le_bytes());
        out.extend_from_slice(&self.tile_size.to_le_bytes());
        out.extend_from_slice(&(self.tiles.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.encode_us.to_le_bytes());
        for t in &self.tiles {
            out.extend_from_slice(&t.x.to_le_bytes());
            out.extend_from_slice(&t.y.to_le_bytes());
            out.extend_from_slice(&t.w.to_le_bytes());
            out.extend_from_slice(&t.h.to_le_bytes());
            out.push(t.encoding as u8);
            out.push(t.flags);
            out.extend_from_slice(&(t.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&t.data);
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < 24 {
            return Err(ProtocolError::Truncated("VideoTileFrame header"));
        }
        let capture_ts_us = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let frame_id = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let screen_w = u16::from_le_bytes(buf[12..14].try_into().unwrap());
        let screen_h = u16::from_le_bytes(buf[14..16].try_into().unwrap());
        let tile_size = u16::from_le_bytes(buf[16..18].try_into().unwrap());
        let count = u16::from_le_bytes(buf[18..20].try_into().unwrap()) as usize;
        let encode_us = u32::from_le_bytes(buf[20..24].try_into().unwrap());
        let mut pos = 24;
        let mut tiles = Vec::with_capacity(count);
        for _ in 0..count {
            if pos + 14 > buf.len() {
                return Err(ProtocolError::Truncated("tile header"));
            }
            let x = u16::from_le_bytes(buf[pos..pos + 2].try_into().unwrap());
            let y = u16::from_le_bytes(buf[pos + 2..pos + 4].try_into().unwrap());
            let w = u16::from_le_bytes(buf[pos + 4..pos + 6].try_into().unwrap());
            let h = u16::from_le_bytes(buf[pos + 6..pos + 8].try_into().unwrap());
            let encoding = match buf[pos + 8] {
                0 => TileEncoding::RawBgra,
                1 => TileEncoding::JpegLossy,
                _ => TileEncoding::PngLossless,
            };
            let flags = buf[pos + 9];
            let len = u32::from_le_bytes(buf[pos + 10..pos + 14].try_into().unwrap()) as usize;
            pos += 14;
            if pos + len > buf.len() {
                return Err(ProtocolError::Truncated("tile payload"));
            }
            tiles.push(EncodedTile {
                x,
                y,
                w,
                h,
                encoding,
                flags,
                data: buf[pos..pos + len].to_vec(),
            });
            pos += len;
        }
        Ok(Self {
            capture_ts_us,
            frame_id,
            screen_w,
            screen_h,
            tile_size,
            encode_us,
            tiles,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MouseEventPacket {
    pub action: u8,
    pub button: u8,
    pub norm_x: u16,
    pub norm_y: u16,
    pub wheel_delta: i16,
}

impl MouseEventPacket {
    pub fn serialize(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0] = self.action;
        b[1] = self.button;
        b[2..4].copy_from_slice(&self.norm_x.to_le_bytes());
        b[4..6].copy_from_slice(&self.norm_y.to_le_bytes());
        b[6..8].copy_from_slice(&self.wheel_delta.to_le_bytes());
        b
    }

    pub fn deserialize(b: &[u8]) -> Result<Self, ProtocolError> {
        if b.len() < 8 {
            return Err(ProtocolError::Truncated("MouseEvent"));
        }
        Ok(Self {
            action: b[0],
            button: b[1],
            norm_x: u16::from_le_bytes([b[2], b[3]]),
            norm_y: u16::from_le_bytes([b[4], b[5]]),
            wheel_delta: i16::from_le_bytes([b[6], b[7]]),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEventPacket {
    pub action: u8,
    pub vk: u16,
    pub scan_code: u16,
    pub flags: u16,
    pub unicode_char: u32,
}

impl KeyEventPacket {
    pub fn serialize(&self) -> [u8; 11] {
        let mut b = [0u8; 11];
        b[0] = self.action;
        b[1..3].copy_from_slice(&self.vk.to_le_bytes());
        b[3..5].copy_from_slice(&self.scan_code.to_le_bytes());
        b[5..7].copy_from_slice(&self.flags.to_le_bytes());
        b[7..11].copy_from_slice(&self.unicode_char.to_le_bytes());
        b
    }

    pub fn deserialize(b: &[u8]) -> Result<Self, ProtocolError> {
        if b.len() < 11 {
            return Err(ProtocolError::Truncated("KeyEvent"));
        }
        Ok(Self {
            action: b[0],
            vk: u16::from_le_bytes([b[1], b[2]]),
            scan_code: u16::from_le_bytes([b[3], b[4]]),
            flags: u16::from_le_bytes([b[5], b[6]]),
            unicode_char: u32::from_le_bytes([b[7], b[8], b[9], b[10]]),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloPayload {
    pub client_name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloAckPayload {
    pub session_id: String,
    pub width: u16,
    pub height: u16,
    pub tile_size: u16,
    pub capture_backend: String,
    pub input_backend: String,
    #[serde(default)]
    pub exec_enabled: bool,
    #[serde(default)]
    pub codec_probe: serde_json::Value,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg_type: MsgType,
    channel: ChannelId,
    flags: u8,
    seq_id: u32,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    let hdr = FrameHeader {
        version: PROTOCOL_VERSION,
        msg_type,
        flags,
        channel: channel as u8,
        payload_len: payload.len() as u32,
        seq_id,
    };
    writer.write_all(&hdr.to_bytes()).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(FrameHeader, Vec<u8>), ProtocolError> {
    let mut hdr_buf = [0u8; HEADER_SIZE];
    reader.read_exact(&mut hdr_buf).await?;
    let hdr = FrameHeader::from_bytes(&hdr_buf)?;
    let mut payload = vec![0u8; hdr.payload_len as usize];
    if !payload.is_empty() {
        reader.read_exact(&mut payload).await?;
    }
    Ok((hdr, payload))
}

/// Performs the line-based `CONNECT <deviceId>\n` -> `OK TUNNEL\n` handshake against `signaling-relay`.
pub async fn connect_via_relay(
    relay_addr: &str,
    device_id: &str,
) -> Result<tokio::net::TcpStream, ProtocolError> {
    let stream = tokio::net::TcpStream::connect(relay_addr).await?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("CONNECT {device_id}\n").as_bytes())
        .await?;
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if !line.starts_with("OK TUNNEL") {
        return Err(ProtocolError::RelayHandshake(line.trim().to_string()));
    }
    Ok(reader.into_inner())
}

/// Queries `LOOKUP <deviceId>\n` -> `OK LAN <ip1:port,ip2:port>\n` for fast LAN-first P2P direct race.
pub async fn lookup_lan_via_relay(relay_addr: &str, device_id: &str) -> Vec<String> {
    let fut = async {
        let stream = tokio::net::TcpStream::connect(relay_addr).await.ok()?;
        let _ = stream.set_nodelay(true);
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(format!("LOOKUP {device_id}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        reader.read_line(&mut line).await.ok()?;
        let rest = line.trim().strip_prefix("OK LAN")?;
        let addrs: Vec<String> = rest
            .trim()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Some(addrs)
    };
    tokio::time::timeout(std::time::Duration::from_millis(1200), fut)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header_and_tiles() {
        let hdr = FrameHeader {
            version: 1,
            msg_type: MsgType::VideoTileFrame,
            flags: frame_flags::KEY_FRAME | frame_flags::LOSSLESS_REFINEMENT,
            channel: ChannelId::Video as u8,
            payload_len: 1234,
            seq_id: 42,
        };
        let decoded = FrameHeader::from_bytes(&hdr.to_bytes()).unwrap();
        assert_eq!(hdr, decoded);

        let pkt = VideoTileFramePacket {
            capture_ts_us: 987_654_321,
            frame_id: 7,
            screen_w: 1920,
            screen_h: 1080,
            tile_size: 128,
            encode_us: 3200,
            tiles: vec![EncodedTile {
                x: 128,
                y: 256,
                w: 128,
                h: 128,
                encoding: TileEncoding::PngLossless,
                flags: 1,
                data: vec![1, 2, 3, 4],
            }],
        };
        let raw = pkt.serialize();
        let pkt2 = VideoTileFramePacket::deserialize(&raw).unwrap();
        assert_eq!(pkt2.frame_id, 7);
        assert_eq!(pkt2.tiles.len(), 1);
        assert_eq!(pkt2.tiles[0].data, vec![1, 2, 3, 4]);
    }
}
