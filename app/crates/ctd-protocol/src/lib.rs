//! Wire-compatible Rust implementation of the CopyToDesk `CTD1` v1 binary protocol (v0.4.0 P1).
//! Includes:
//! - End-to-End Encryption (E2EE): Ephemeral `X25519` + `HKDF-SHA256` (bound to token) + `ChaCha20-Poly1305` AEAD
//! - Chunked streaming file upload & download frames (`0x66..=0x69`) with per-chunk & whole-file SHA-256 verification
//! - Backward-compatible framing with Windows C# MVP, `signaling-relay`, and `mcp-server`.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use x25519_dalek::{PublicKey, StaticSecret};

pub const MAGIC: u32 = 0x3144_5443; // "CTD1" little-endian
pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_SIZE: usize = 16;
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const E2EE_HANDSHAKE_LEN: usize = 48; // 32B X25519 pubkey + 16B random salt
pub const FILE_CHUNK_SIZE: usize = 256 * 1024; // 256 KB streaming chunk size

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
    #[error("e2ee crypto authentication failed (wrong password or tampered stream)")]
    CryptoAuthFailed,
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
    E2eeInit = 0x05,
    E2eeAck = 0x06,
    VideoTileFrame = 0x10,
    RequestKeyFrame = 0x11,
    MonitorSelect = 0x12,
    MonitorSelectAck = 0x13,
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
    FileUploadChunk = 0x66,
    FileUploadChunkAck = 0x67,
    FileDownloadChunkReq = 0x68,
    FileDownloadChunkResp = 0x69,
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
            0x05 => Ok(Self::E2eeInit),
            0x06 => Ok(Self::E2eeAck),
            0x10 => Ok(Self::VideoTileFrame),
            0x11 => Ok(Self::RequestKeyFrame),
            0x12 => Ok(Self::MonitorSelect),
            0x13 => Ok(Self::MonitorSelectAck),
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
            0x66 => Ok(Self::FileUploadChunk),
            0x67 => Ok(Self::FileUploadChunkAck),
            0x68 => Ok(Self::FileDownloadChunkReq),
            0x69 => Ok(Self::FileDownloadChunkResp),
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
    pub const ENCRYPTED: u8 = 1 << 3;
}

pub mod key_mod_flags {
    pub const EXTENDED: u16 = 1 << 0;
    pub const SHIFT: u16 = 1 << 1;
    pub const CTRL: u16 = 1 << 2;
    pub const ALT: u16 = 1 << 3;
    pub const META: u16 = 1 << 4;
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

    pub fn aad_bytes(&self) -> [u8; 8] {
        let s = self.seq_id.to_le_bytes();
        [
            self.version,
            self.msg_type as u8,
            self.flags,
            self.channel,
            s[0],
            s[1],
            s[2],
            s[3],
        ]
    }
}

// ============================================================================
// End-to-End Encryption (X25519 + HKDF-SHA256 + ChaCha20-Poly1305)
// ============================================================================

pub struct SendCipher {
    aead: ChaCha20Poly1305,
    dir: u8,
    counter: u64,
}

pub struct RecvCipher {
    aead: ChaCha20Poly1305,
    dir: u8,
    counter: u64,
}

impl SendCipher {
    pub fn seal(
        &mut self,
        msg_type: MsgType,
        channel: ChannelId,
        flags: u8,
        seq_id: u32,
        plaintext: &[u8],
    ) -> Result<(u8, Vec<u8>), ProtocolError> {
        let enc_flags = flags | frame_flags::ENCRYPTED;
        let hdr = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type,
            flags: enc_flags,
            channel: channel as u8,
            payload_len: 0,
            seq_id,
        };
        let aad = hdr.aad_bytes();
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[0] = self.dir;
        nonce_bytes[4..12].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self.counter.wrapping_add(1);

        let ct = self
            .aead
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| ProtocolError::CryptoAuthFailed)?;
        Ok((enc_flags, ct))
    }
}

impl RecvCipher {
    pub fn open(&mut self, hdr: &FrameHeader, ciphertext: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        if (hdr.flags & frame_flags::ENCRYPTED) == 0 {
            return Err(ProtocolError::CryptoAuthFailed);
        }
        let aad = hdr.aad_bytes();
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[0] = self.dir;
        nonce_bytes[4..12].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self.counter.wrapping_add(1);

        self.aead
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| ProtocolError::CryptoAuthFailed)
    }
}

fn derive_directional_ciphers(
    shared_secret: &[u8; 32],
    client_pub: &[u8; 32],
    host_pub: &[u8; 32],
    client_salt: &[u8; 16],
    host_salt: &[u8; 16],
    token: &str,
    is_client: bool,
) -> (SendCipher, RecvCipher) {
    let token_hash = Sha256::digest(token.as_bytes());
    let mut ikm = [0u8; 64];
    ikm[0..32].copy_from_slice(shared_secret);
    ikm[32..64].copy_from_slice(&token_hash);

    let mut salt_hasher = Sha256::new();
    salt_hasher.update(b"CTD1-E2EE-SALT-v1");
    salt_hasher.update(client_pub);
    salt_hasher.update(host_pub);
    salt_hasher.update(client_salt);
    salt_hasher.update(host_salt);
    let salt = salt_hasher.finalize();

    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut c2h_key = [0u8; 32];
    let mut h2c_key = [0u8; 32];
    hk.expand(b"CTD1-E2EE-C2H-KEY", &mut c2h_key)
        .expect("valid HKDF output len");
    hk.expand(b"CTD1-E2EE-H2C-KEY", &mut h2c_key)
        .expect("valid HKDF output len");

    let c2h_aead = ChaCha20Poly1305::new_from_slice(&c2h_key).expect("32-byte key");
    let h2c_aead = ChaCha20Poly1305::new_from_slice(&h2c_key).expect("32-byte key");

    if is_client {
        (
            SendCipher {
                aead: c2h_aead,
                dir: 1,
                counter: 0,
            },
            RecvCipher {
                aead: h2c_aead,
                dir: 2,
                counter: 0,
            },
        )
    } else {
        (
            SendCipher {
                aead: h2c_aead,
                dir: 2,
                counter: 0,
            },
            RecvCipher {
                aead: c2h_aead,
                dir: 1,
                counter: 0,
            },
        )
    }
}

pub struct E2eeClientHandshake {
    secret: StaticSecret,
    client_pub: [u8; 32],
    client_salt: [u8; 16],
}

impl E2eeClientHandshake {
    pub fn new() -> (Self, [u8; E2EE_HANDSHAKE_LEN]) {
        let sec_bytes: [u8; 32] = rand::random();
        let client_salt: [u8; 16] = rand::random();
        let secret = StaticSecret::from(sec_bytes);
        let pub_key = PublicKey::from(&secret);
        let client_pub = *pub_key.as_bytes();

        let mut init_payload = [0u8; E2EE_HANDSHAKE_LEN];
        init_payload[0..32].copy_from_slice(&client_pub);
        init_payload[32..48].copy_from_slice(&client_salt);
        (
            Self {
                secret,
                client_pub,
                client_salt,
            },
            init_payload,
        )
    }

    pub fn finish(
        self,
        ack_payload: &[u8],
        token: &str,
    ) -> Result<(SendCipher, RecvCipher), ProtocolError> {
        if ack_payload.len() < E2EE_HANDSHAKE_LEN {
            return Err(ProtocolError::Truncated("E2eeAck"));
        }
        let mut host_pub = [0u8; 32];
        let mut host_salt = [0u8; 16];
        host_pub.copy_from_slice(&ack_payload[0..32]);
        host_salt.copy_from_slice(&ack_payload[32..48]);

        let peer = PublicKey::from(host_pub);
        let shared = self.secret.diffie_hellman(&peer);
        Ok(derive_directional_ciphers(
            shared.as_bytes(),
            &self.client_pub,
            &host_pub,
            &self.client_salt,
            &host_salt,
            token,
            true,
        ))
    }
}

pub struct ServerE2eeHandshake {
    shared_bytes: [u8; 32],
    client_pub: [u8; 32],
    host_pub: [u8; 32],
    client_salt: [u8; 16],
    host_salt: [u8; 16],
}

impl ServerE2eeHandshake {
    pub fn begin(init_payload: &[u8]) -> Result<(Self, [u8; E2EE_HANDSHAKE_LEN]), ProtocolError> {
        if init_payload.len() < E2EE_HANDSHAKE_LEN {
            return Err(ProtocolError::Truncated("E2eeInit"));
        }
        let mut client_pub = [0u8; 32];
        let mut client_salt = [0u8; 16];
        client_pub.copy_from_slice(&init_payload[0..32]);
        client_salt.copy_from_slice(&init_payload[32..48]);

        let sec_bytes: [u8; 32] = rand::random();
        let host_salt: [u8; 16] = rand::random();
        let secret = StaticSecret::from(sec_bytes);
        let host_pub = *PublicKey::from(&secret).as_bytes();

        let peer = PublicKey::from(client_pub);
        let shared = secret.diffie_hellman(&peer);

        let mut ack_payload = [0u8; E2EE_HANDSHAKE_LEN];
        ack_payload[0..32].copy_from_slice(&host_pub);
        ack_payload[32..48].copy_from_slice(&host_salt);

        Ok((
            Self {
                shared_bytes: *shared.as_bytes(),
                client_pub,
                host_pub,
                client_salt,
                host_salt,
            },
            ack_payload,
        ))
    }

    pub fn try_open_first_frame(
        &self,
        candidate_token: &str,
        hdr: &FrameHeader,
        ciphertext: &[u8],
    ) -> Result<(SendCipher, RecvCipher, Vec<u8>), ProtocolError> {
        let (send_c, mut recv_c) = derive_directional_ciphers(
            &self.shared_bytes,
            &self.client_pub,
            &self.host_pub,
            &self.client_salt,
            &self.host_salt,
            candidate_token,
            false,
        );
        let pt = recv_c.open(hdr, ciphertext)?;
        Ok((send_c, recv_c, pt))
    }
}

pub fn accept_e2ee_server(
    init_payload: &[u8],
    token: &str,
) -> Result<([u8; E2EE_HANDSHAKE_LEN], SendCipher, RecvCipher), ProtocolError> {
    let (hs, ack_payload) = ServerE2eeHandshake::begin(init_payload)?;
    let (send_c, recv_c) = derive_directional_ciphers(
        &hs.shared_bytes,
        &hs.client_pub,
        &hs.host_pub,
        &hs.client_salt,
        &hs.host_salt,
        token,
        false,
    );
    Ok((ack_payload, send_c, recv_c))
}

// ============================================================================
// Video, Input, Control & Chunked File Payloads
// ============================================================================

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStatePacket {
    pub norm_x: u16,
    pub norm_y: u16,
    pub shape: u8,
    pub visible: u8,
}

impl CursorStatePacket {
    pub fn serialize(&self) -> [u8; 6] {
        let mut b = [0u8; 6];
        b[0..2].copy_from_slice(&self.norm_x.to_le_bytes());
        b[2..4].copy_from_slice(&self.norm_y.to_le_bytes());
        b[4] = self.shape;
        b[5] = self.visible;
        b
    }

    pub fn deserialize(b: &[u8]) -> Result<Self, ProtocolError> {
        if b.len() < 6 {
            return Err(ProtocolError::Truncated("CursorState"));
        }
        Ok(Self {
            norm_x: u16::from_le_bytes([b[0], b[1]]),
            norm_y: u16::from_le_bytes([b[2], b[3]]),
            shape: b[4],
            visible: b[5],
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: u16,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSelectAckPayload {
    pub ok: bool,
    pub active_monitor: u16,
    pub width: u16,
    pub height: u16,
    pub monitors: Vec<MonitorInfo>,
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
    #[serde(default)]
    pub monitors: Vec<MonitorInfo>,
    #[serde(default)]
    pub active_monitor: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadChunkMeta {
    pub upload_id: String,
    pub path: String,
    pub offset: u64,
    pub total_size: u64,
    pub eof: bool,
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadChunkReq {
    pub path: String,
    pub offset: u64,
    pub max_bytes: u32,
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

pub async fn write_frame_opt_cipher<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cipher: Option<&mut SendCipher>,
    msg_type: MsgType,
    channel: ChannelId,
    flags: u8,
    seq_id: u32,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    if let Some(c) = cipher {
        let (enc_flags, ct) = c.seal(msg_type, channel, flags, seq_id, payload)?;
        write_frame(writer, msg_type, channel, enc_flags, seq_id, &ct).await
    } else {
        write_frame(writer, msg_type, channel, flags, seq_id, payload).await
    }
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

pub async fn read_frame_opt_cipher<R: AsyncRead + Unpin>(
    reader: &mut R,
    cipher: Option<&mut RecvCipher>,
) -> Result<(FrameHeader, Vec<u8>), ProtocolError> {
    let (hdr, raw) = read_frame(reader).await?;
    if let Some(c) = cipher {
        let pt = c.open(&hdr, &raw)?;
        Ok((hdr, pt))
    } else {
        Ok((hdr, raw))
    }
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

    #[test]
    fn e2ee_x25519_chacha20poly1305_handshake_and_tamper_rejection() {
        let (client_hs, init_bytes) = E2eeClientHandshake::new();
        let (ack_bytes, mut host_tx, mut host_rx) =
            accept_e2ee_server(&init_bytes, "secret-pass-999").unwrap();
        let (mut client_tx, mut client_rx) =
            client_hs.finish(&ack_bytes, "secret-pass-999").unwrap();

        // Client -> Host encrypted frame
        let msg = b"hello encrypted remote desktop";
        let (flags, ct) = client_tx
            .seal(MsgType::Hello, ChannelId::Control, 0, 1, msg)
            .unwrap();
        assert_ne!(flags & frame_flags::ENCRYPTED, 0);
        assert_eq!(ct.len(), msg.len() + 16);

        let hdr = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::Hello,
            flags,
            channel: ChannelId::Control as u8,
            payload_len: ct.len() as u32,
            seq_id: 1,
        };
        let pt = host_rx.open(&hdr, &ct).unwrap();
        assert_eq!(pt, msg);

        // Host -> Client encrypted frame + AAD header tamper test
        let (h_flags, h_ct) = host_tx
            .seal(MsgType::VideoTileFrame, ChannelId::Video, frame_flags::KEY_FRAME, 2, b"tile-bytes")
            .unwrap();
        let bad_hdr = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::VideoTileFrame,
            flags: h_flags,
            channel: ChannelId::Video as u8,
            payload_len: h_ct.len() as u32,
            seq_id: 99, // tampered seq_id!
        };
        // Note: failed open advances rx counter, so test wrong token separately too
        let (client_hs2, init2) = E2eeClientHandshake::new();
        let (ack2, _, mut host_rx_wrong) = accept_e2ee_server(&init2, "wrong-token").unwrap();
        let (mut client_tx2, _) = client_hs2.finish(&ack2, "right-token").unwrap();
        let (f2, ct2) = client_tx2
            .seal(MsgType::Hello, ChannelId::Control, 0, 1, b"auth")
            .unwrap();
        let hdr2 = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::Hello,
            flags: f2,
            channel: 0,
            payload_len: ct2.len() as u32,
            seq_id: 1,
        };
        assert!(host_rx_wrong.open(&hdr2, &ct2).is_err());

        assert!(client_rx.open(&bad_hdr, &h_ct).is_err());

        // Dual-token ServerE2eeHandshake (temporary vs unattended password) + CursorStatePacket
        let (c_hs, c_init) = E2eeClientHandshake::new();
        let (srv_pending, srv_ack) = ServerE2eeHandshake::begin(&c_init).unwrap();
        let (mut c_tx, _) = c_hs.finish(&srv_ack, "unattended-fixed-pwd").unwrap();
        let (cf, cct) = c_tx
            .seal(MsgType::Hello, ChannelId::Control, 0, 1, b"unattended-auth")
            .unwrap();
        let chdr = FrameHeader {
            version: PROTOCOL_VERSION,
            msg_type: MsgType::Hello,
            flags: cf,
            channel: 0,
            payload_len: cct.len() as u32,
            seq_id: 1,
        };
        assert!(srv_pending.try_open_first_frame("temp-pwd", &chdr, &cct).is_err());
        let (_, _, opened) = srv_pending
            .try_open_first_frame("unattended-fixed-pwd", &chdr, &cct)
            .unwrap();
        assert_eq!(opened, b"unattended-auth");

        let cur = CursorStatePacket {
            norm_x: 32768,
            norm_y: 16384,
            shape: 2,
            visible: 1,
        };
        assert_eq!(CursorStatePacket::deserialize(&cur.serialize()).unwrap(), cur);
    }
}
