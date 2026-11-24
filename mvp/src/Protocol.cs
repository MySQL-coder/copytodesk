using System;
using System.Collections.Generic;
using System.IO;
using System.Net.Sockets;
using System.Text;

namespace CopyToDesk.Core
{
    public enum MsgType : byte
    {
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
        ExecResp = 0x71
    }

    public enum ChannelId : byte
    {
        Control = 0,
        Video = 1,
        Cursor = 2,
        Input = 3,
        Audio = 4,
        File = 5
    }

    [Flags]
    public enum FrameFlags : byte
    {
        None = 0,
        KeyFrame = 1 << 0,
        LosslessRefinement = 1 << 1,
        Compressed = 1 << 2
    }

    public enum TileEncoding : byte
    {
        RawBgra = 0,
        JpegLossy = 1,
        PngLossless = 2
    }

    public enum MouseAction : byte
    {
        Move = 0,
        Down = 1,
        Up = 2,
        Wheel = 3
    }

    public enum MouseButton : byte
    {
        None = 0,
        Left = 1,
        Right = 2,
        Middle = 3
    }

    public enum KeyAction : byte
    {
        Down = 0,
        Up = 1,
        UnicodeChar = 2
    }

    public struct FrameHeader
    {
        public const int Size = 16;
        public const uint MagicValue = 0x31445443; // "CTD1" little-endian
        public const int MaxPayloadBytes = 16 * 1024 * 1024;

        public byte Version;
        public MsgType Type;
        public FrameFlags Flags;
        public ChannelId Channel;
        public uint PayloadLength;
        public uint SeqId;

        public void WriteTo(byte[] buf, int offset)
        {
            WriteUInt32LE(buf, offset + 0, MagicValue);
            buf[offset + 4] = Version;
            buf[offset + 5] = (byte)Type;
            buf[offset + 6] = (byte)Flags;
            buf[offset + 7] = (byte)Channel;
            WriteUInt32LE(buf, offset + 8, PayloadLength);
            WriteUInt32LE(buf, offset + 12, SeqId);
        }

        public static FrameHeader ReadFrom(byte[] buf, int offset)
        {
            uint magic = ReadUInt32LE(buf, offset + 0);
            if (magic != MagicValue)
                throw new InvalidDataException("Invalid protocol magic: 0x" + magic.ToString("X8"));
            FrameHeader h = new FrameHeader();
            h.Version = buf[offset + 4];
            h.Type = (MsgType)buf[offset + 5];
            h.Flags = (FrameFlags)buf[offset + 6];
            h.Channel = (ChannelId)buf[offset + 7];
            h.PayloadLength = ReadUInt32LE(buf, offset + 8);
            h.SeqId = ReadUInt32LE(buf, offset + 12);
            if (h.Version != 1)
                throw new InvalidDataException("Unsupported protocol version: " + h.Version);
            if (h.PayloadLength > MaxPayloadBytes)
                throw new InvalidDataException("Payload exceeds 16MB limit: " + h.PayloadLength);
            return h;
        }

        public static void WriteUInt16LE(byte[] b, int o, ushort v)
        {
            b[o] = (byte)(v & 0xFF);
            b[o + 1] = (byte)((v >> 8) & 0xFF);
        }

        public static void WriteInt16LE(byte[] b, int o, short v)
        {
            WriteUInt16LE(b, o, (ushort)v);
        }

        public static void WriteUInt32LE(byte[] b, int o, uint v)
        {
            b[o] = (byte)(v & 0xFF);
            b[o + 1] = (byte)((v >> 8) & 0xFF);
            b[o + 2] = (byte)((v >> 16) & 0xFF);
            b[o + 3] = (byte)((v >> 24) & 0xFF);
        }

        public static void WriteInt32LE(byte[] b, int o, int v)
        {
            WriteUInt32LE(b, o, (uint)v);
        }

        public static void WriteUInt64LE(byte[] b, int o, ulong v)
        {
            WriteUInt32LE(b, o, (uint)(v & 0xFFFFFFFFUL));
            WriteUInt32LE(b, o + 4, (uint)(v >> 32));
        }

        public static ushort ReadUInt16LE(byte[] b, int o)
        {
            return (ushort)(b[o] | (b[o + 1] << 8));
        }

        public static short ReadInt16LE(byte[] b, int o)
        {
            return (short)ReadUInt16LE(b, o);
        }

        public static uint ReadUInt32LE(byte[] b, int o)
        {
            return (uint)(b[o] | (b[o + 1] << 8) | (b[o + 2] << 16) | (b[o + 3] << 24));
        }

        public static int ReadInt32LE(byte[] b, int o)
        {
            return (int)ReadUInt32LE(b, o);
        }

        public static ulong ReadUInt64LE(byte[] b, int o)
        {
            ulong lo = ReadUInt32LE(b, o);
            ulong hi = ReadUInt32LE(b, o + 4);
            return lo | (hi << 32);
        }
    }

    public class EncodedTile
    {
        public ushort X;
        public ushort Y;
        public ushort W;
        public ushort H;
        public TileEncoding Encoding;
        public byte Flags;
        public byte[] Data;
    }

    public class VideoTileFramePacket
    {
        public ulong CaptureTsUs;
        public uint FrameId;
        public ushort ScreenW;
        public ushort ScreenH;
        public ushort TileSize;
        public uint EncodeUs;
        public List<EncodedTile> Tiles = new List<EncodedTile>();

        public byte[] Serialize()
        {
            int total = 24;
            for (int i = 0; i < Tiles.Count; i++)
                total += 14 + Tiles[i].Data.Length;
            byte[] buf = new byte[total];
            FrameHeader.WriteUInt64LE(buf, 0, CaptureTsUs);
            FrameHeader.WriteUInt32LE(buf, 8, FrameId);
            FrameHeader.WriteUInt16LE(buf, 12, ScreenW);
            FrameHeader.WriteUInt16LE(buf, 14, ScreenH);
            FrameHeader.WriteUInt16LE(buf, 16, TileSize);
            FrameHeader.WriteUInt16LE(buf, 18, (ushort)Tiles.Count);
            FrameHeader.WriteUInt32LE(buf, 20, EncodeUs);
            int pos = 24;
            for (int i = 0; i < Tiles.Count; i++)
            {
                EncodedTile t = Tiles[i];
                FrameHeader.WriteUInt16LE(buf, pos + 0, t.X);
                FrameHeader.WriteUInt16LE(buf, pos + 2, t.Y);
                FrameHeader.WriteUInt16LE(buf, pos + 4, t.W);
                FrameHeader.WriteUInt16LE(buf, pos + 6, t.H);
                buf[pos + 8] = (byte)t.Encoding;
                buf[pos + 9] = t.Flags;
                FrameHeader.WriteUInt32LE(buf, pos + 10, (uint)t.Data.Length);
                Buffer.BlockCopy(t.Data, 0, buf, pos + 14, t.Data.Length);
                pos += 14 + t.Data.Length;
            }
            return buf;
        }

        public static VideoTileFramePacket Deserialize(byte[] buf)
        {
            if (buf.Length < 24) throw new InvalidDataException("Short VideoTileFrame");
            VideoTileFramePacket p = new VideoTileFramePacket();
            p.CaptureTsUs = FrameHeader.ReadUInt64LE(buf, 0);
            p.FrameId = FrameHeader.ReadUInt32LE(buf, 8);
            p.ScreenW = FrameHeader.ReadUInt16LE(buf, 12);
            p.ScreenH = FrameHeader.ReadUInt16LE(buf, 14);
            p.TileSize = FrameHeader.ReadUInt16LE(buf, 16);
            ushort count = FrameHeader.ReadUInt16LE(buf, 18);
            p.EncodeUs = FrameHeader.ReadUInt32LE(buf, 20);
            int pos = 24;
            for (int i = 0; i < count; i++)
            {
                if (pos + 14 > buf.Length) throw new InvalidDataException("Truncated tile header");
                EncodedTile t = new EncodedTile();
                t.X = FrameHeader.ReadUInt16LE(buf, pos + 0);
                t.Y = FrameHeader.ReadUInt16LE(buf, pos + 2);
                t.W = FrameHeader.ReadUInt16LE(buf, pos + 4);
                t.H = FrameHeader.ReadUInt16LE(buf, pos + 6);
                t.Encoding = (TileEncoding)buf[pos + 8];
                t.Flags = buf[pos + 9];
                int len = (int)FrameHeader.ReadUInt32LE(buf, pos + 10);
                if (pos + 14 + len > buf.Length) throw new InvalidDataException("Truncated tile payload");
                t.Data = new byte[len];
                Buffer.BlockCopy(buf, pos + 14, t.Data, 0, len);
                pos += 14 + len;
                p.Tiles.Add(t);
            }
            return p;
        }
    }

    public struct CursorPacket
    {
        public int X;
        public int Y;
        public bool Visible;
        public ulong ShapeHash;
        public ushort HotspotX;
        public ushort HotspotY;
        public ushort Width;
        public ushort Height;
        public byte[] Rgba;

        public byte[] Serialize()
        {
            int rgbaLen = Rgba == null ? 0 : Rgba.Length;
            byte[] buf = new byte[29 + rgbaLen];
            FrameHeader.WriteInt32LE(buf, 0, X);
            FrameHeader.WriteInt32LE(buf, 4, Y);
            buf[8] = Visible ? (byte)1 : (byte)0;
            FrameHeader.WriteUInt64LE(buf, 9, ShapeHash);
            FrameHeader.WriteUInt16LE(buf, 17, HotspotX);
            FrameHeader.WriteUInt16LE(buf, 19, HotspotY);
            FrameHeader.WriteUInt16LE(buf, 21, Width);
            FrameHeader.WriteUInt16LE(buf, 23, Height);
            FrameHeader.WriteUInt32LE(buf, 25, (uint)rgbaLen);
            if (rgbaLen > 0) Buffer.BlockCopy(Rgba, 0, buf, 29, rgbaLen);
            return buf;
        }

        public static CursorPacket Deserialize(byte[] buf)
        {
            CursorPacket c = new CursorPacket();
            c.X = FrameHeader.ReadInt32LE(buf, 0);
            c.Y = FrameHeader.ReadInt32LE(buf, 4);
            c.Visible = buf[8] != 0;
            c.ShapeHash = FrameHeader.ReadUInt64LE(buf, 9);
            c.HotspotX = FrameHeader.ReadUInt16LE(buf, 17);
            c.HotspotY = FrameHeader.ReadUInt16LE(buf, 19);
            c.Width = FrameHeader.ReadUInt16LE(buf, 21);
            c.Height = FrameHeader.ReadUInt16LE(buf, 23);
            int rgbaLen = (int)FrameHeader.ReadUInt32LE(buf, 25);
            if (rgbaLen > 0 && 29 + rgbaLen <= buf.Length)
            {
                c.Rgba = new byte[rgbaLen];
                Buffer.BlockCopy(buf, 29, c.Rgba, 0, rgbaLen);
            }
            return c;
        }
    }

    public struct MouseEventPacket
    {
        public MouseAction Action;
        public MouseButton Button;
        public ushort NormX;
        public ushort NormY;
        public short WheelDelta;

        public byte[] Serialize()
        {
            byte[] b = new byte[8];
            b[0] = (byte)Action;
            b[1] = (byte)Button;
            FrameHeader.WriteUInt16LE(b, 2, NormX);
            FrameHeader.WriteUInt16LE(b, 4, NormY);
            FrameHeader.WriteInt16LE(b, 6, WheelDelta);
            return b;
        }

        public static MouseEventPacket Deserialize(byte[] b)
        {
            MouseEventPacket m = new MouseEventPacket();
            m.Action = (MouseAction)b[0];
            m.Button = (MouseButton)b[1];
            m.NormX = FrameHeader.ReadUInt16LE(b, 2);
            m.NormY = FrameHeader.ReadUInt16LE(b, 4);
            m.WheelDelta = FrameHeader.ReadInt16LE(b, 6);
            return m;
        }
    }

    public struct KeyEventPacket
    {
        public KeyAction Action;
        public ushort Vk;
        public ushort ScanCode;
        public ushort Flags;
        public uint UnicodeChar;

        public byte[] Serialize()
        {
            byte[] b = new byte[11];
            b[0] = (byte)Action;
            FrameHeader.WriteUInt16LE(b, 1, Vk);
            FrameHeader.WriteUInt16LE(b, 3, ScanCode);
            FrameHeader.WriteUInt16LE(b, 5, Flags);
            FrameHeader.WriteUInt32LE(b, 7, UnicodeChar);
            return b;
        }

        public static KeyEventPacket Deserialize(byte[] b)
        {
            KeyEventPacket k = new KeyEventPacket();
            k.Action = (KeyAction)b[0];
            k.Vk = FrameHeader.ReadUInt16LE(b, 1);
            k.ScanCode = FrameHeader.ReadUInt16LE(b, 3);
            k.Flags = FrameHeader.ReadUInt16LE(b, 5);
            k.UnicodeChar = FrameHeader.ReadUInt32LE(b, 7);
            return k;
        }
    }

    public class TcpFrameTransport : IDisposable
    {
        private readonly TcpClient _client;
        private readonly NetworkStream _stream;
        private readonly object _writeLock = new object();
        private readonly byte[] _hdrWrite = new byte[FrameHeader.Size];
        private readonly byte[] _hdrRead = new byte[FrameHeader.Size];
        private uint _seq;

        public TcpFrameTransport(TcpClient client)
        {
            _client = client;
            _client.NoDelay = true;
            _client.SendBufferSize = 256 * 1024;
            _client.ReceiveBufferSize = 512 * 1024;
            _stream = client.GetStream();
        }

        public bool Connected { get { return _client != null && _client.Connected; } }

        public void Send(MsgType type, ChannelId ch, byte[] payload, FrameFlags flags)
        {
            if (payload == null) payload = new byte[0];
            lock (_writeLock)
            {
                FrameHeader h = new FrameHeader();
                h.Version = 1;
                h.Type = type;
                h.Flags = flags;
                h.Channel = ch;
                h.PayloadLength = (uint)payload.Length;
                h.SeqId = ++_seq;
                h.WriteTo(_hdrWrite, 0);
                _stream.Write(_hdrWrite, 0, FrameHeader.Size);
                if (payload.Length > 0)
                    _stream.Write(payload, 0, payload.Length);
            }
        }

        public void SendUtf8(MsgType type, ChannelId ch, string text)
        {
            Send(type, ch, Encoding.UTF8.GetBytes(text), FrameFlags.None);
        }

        public bool TryRead(out FrameHeader header, out byte[] payload)
        {
            header = new FrameHeader();
            payload = null;
            if (!ReadExact(_hdrRead, FrameHeader.Size)) return false;
            header = FrameHeader.ReadFrom(_hdrRead, 0);
            payload = new byte[header.PayloadLength];
            if (header.PayloadLength > 0 && !ReadExact(payload, (int)header.PayloadLength))
                return false;
            return true;
        }

        private bool ReadExact(byte[] buf, int count)
        {
            int got = 0;
            while (got < count)
            {
                int n = _stream.Read(buf, got, count - got);
                if (n <= 0) return false;
                got += n;
            }
            return true;
        }

        public void Dispose()
        {
            try { _stream.Close(); } catch { }
            try { _client.Close(); } catch { }
        }
    }

    public static class RelayClientHelper
    {
        public static string ReadAsciiLine(NetworkStream stream)
        {
            List<byte> bytes = new List<byte>(64);
            while (bytes.Count < 4096)
            {
                int b = stream.ReadByte();
                if (b < 0) return null;
                if (b == '\n') break;
                if (b != '\r') bytes.Add((byte)b);
            }
            return Encoding.UTF8.GetString(bytes.ToArray());
        }

        public static void WriteAsciiLine(NetworkStream stream, string line)
        {
            byte[] data = Encoding.UTF8.GetBytes(line + "\n");
            stream.Write(data, 0, data.Length);
        }

        public static TcpClient ConnectViaRelay(string relayHost, int relayPort, string deviceId)
        {
            TcpClient tcp = new TcpClient(relayHost, relayPort);
            tcp.NoDelay = true;
            NetworkStream ns = tcp.GetStream();
            WriteAsciiLine(ns, "CONNECT " + deviceId);
            string resp = ReadAsciiLine(ns);
            if (resp == null || !resp.StartsWith("OK TUNNEL", StringComparison.Ordinal))
            {
                tcp.Close();
                throw new IOException("Relay connect failed: " + (resp ?? "EOF"));
            }
            return tcp;
        }
    }

    public static class MiniJson
    {
        public static string Escape(string s)
        {
            if (s == null) return "";
            StringBuilder sb = new StringBuilder(s.Length + 8);
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                if (c == '\\') sb.Append("\\\\");
                else if (c == '"') sb.Append("\\\"");
                else if (c == '\n') sb.Append("\\n");
                else if (c == '\r') sb.Append("\\r");
                else if (c == '\t') sb.Append("\\t");
                else sb.Append(c);
            }
            return sb.ToString();
        }

        public static string GetString(string json, string key)
        {
            if (string.IsNullOrEmpty(json)) return "";
            string needle = "\"" + key + "\"";
            int idx = json.IndexOf(needle, StringComparison.Ordinal);
            if (idx < 0) return "";
            int colon = json.IndexOf(':', idx + needle.Length);
            if (colon < 0) return "";
            int q1 = json.IndexOf('"', colon + 1);
            if (q1 < 0) return "";
            StringBuilder sb = new StringBuilder();
            for (int i = q1 + 1; i < json.Length; i++)
            {
                char c = json[i];
                if (c == '\\' && i + 1 < json.Length)
                {
                    i++;
                    sb.Append(json[i]);
                }
                else if (c == '"') break;
                else sb.Append(c);
            }
            return sb.ToString();
        }

        public static int GetInt(string json, string key, int defVal)
        {
            if (string.IsNullOrEmpty(json)) return defVal;
            string needle = "\"" + key + "\"";
            int idx = json.IndexOf(needle, StringComparison.Ordinal);
            if (idx < 0) return defVal;
            int colon = json.IndexOf(':', idx + needle.Length);
            if (colon < 0) return defVal;
            int p = colon + 1;
            while (p < json.Length && (json[p] == ' ' || json[p] == '\t')) p++;
            int start = p;
            if (p < json.Length && json[p] == '-') p++;
            while (p < json.Length && char.IsDigit(json[p])) p++;
            int v;
            if (p > start && int.TryParse(json.Substring(start, p - start), out v))
                return v;
            return defVal;
        }
    }
}
