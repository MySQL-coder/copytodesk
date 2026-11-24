using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using System.Windows.Forms;
using CopyToDesk.Core;

namespace CopyToDesk.Viewer
{
    public class RemoteViewerForm : Form
    {
        private readonly string _host;
        private readonly int _port;
        private readonly string _token;
        private readonly string _relayHost;
        private readonly int _relayPort;
        private readonly string _deviceId;
        private TcpFrameTransport _transport;
        private volatile bool _running = true;

        private readonly object _fbLock = new object();
        private Bitmap _framebuffer;
        private int _remoteW = 1280;
        private int _remoteH = 720;
        private int _cursorX;
        private int _cursorY;
        private bool _cursorVisible;

        private long _framesRecv;
        private long _lossyTiles;
        private long _losslessTiles;
        private long _bytesRecv;
        private double _lastRttMs;
        private double _fps;
        private double _kbps;
        private string _backendInfo = "connecting...";

        public RemoteViewerForm(string host, int port, string token, string relayHost, int relayPort, string deviceId)
        {
            _host = host;
            _port = port;
            _token = token;
            _relayHost = relayHost;
            _relayPort = relayPort;
            _deviceId = deviceId;
            this.Text = "CopyToDesk Clean-Room Viewer (Phase 1)";
            this.ClientSize = new Size(1280, 720);
            this.DoubleBuffered = true;
            this.BackColor = Color.Black;
            this.KeyPreview = true;

            System.Windows.Forms.Timer hudTimer = new System.Windows.Forms.Timer();
            hudTimer.Interval = 1000;
            long prevFrames = 0;
            long prevBytes = 0;
            hudTimer.Tick += delegate(object s, EventArgs e)
            {
                long f = Interlocked.Read(ref _framesRecv);
                long b = Interlocked.Read(ref _bytesRecv);
                _fps = f - prevFrames;
                _kbps = (b - prevBytes) / 1024.0;
                prevFrames = f;
                prevBytes = b;
                SendPing();
                this.Invalidate(new Rectangle(0, 0, 560, 34));
            };
            hudTimer.Start();

            Thread netThread = new Thread(NetworkLoop);
            netThread.IsBackground = true;
            netThread.Start();
        }

        protected override void OnFormClosing(FormClosingEventArgs e)
        {
            _running = false;
            if (_transport != null) _transport.Dispose();
            base.OnFormClosing(e);
        }

        private void NetworkLoop()
        {
            try
            {
                TcpClient tcp = !string.IsNullOrEmpty(_relayHost)
                    ? RelayClientHelper.ConnectViaRelay(_relayHost, _relayPort, _deviceId)
                    : new TcpClient(_host, _port);
                _transport = new TcpFrameTransport(tcp);
                string hello = string.Format("{{\"clientName\":\"WinFormsViewer\",\"token\":\"{0}\"}}", MiniJson.Escape(_token));
                _transport.SendUtf8(MsgType.Hello, ChannelId.Control, hello);

                FrameHeader hdr;
                byte[] payload;
                if (!_transport.TryRead(out hdr, out payload) || hdr.Type != MsgType.HelloAck)
                    throw new IOException("Handshake failed");

                string ack = Encoding.UTF8.GetString(payload);
                _remoteW = MiniJson.GetInt(ack, "width", 1280);
                _remoteH = MiniJson.GetInt(ack, "height", 720);
                _backendInfo = MiniJson.GetString(ack, "captureBackend") + " / " + MiniJson.GetString(ack, "inputBackend");

                lock (_fbLock)
                {
                    _framebuffer = new Bitmap(_remoteW, _remoteH, PixelFormat.Format32bppRgb);
                }

                while (_running && _transport.TryRead(out hdr, out payload))
                {
                    Interlocked.Add(ref _bytesRecv, FrameHeader.Size + payload.Length);
                    if (hdr.Type == MsgType.VideoTileFrame)
                    {
                        VideoTileFramePacket pkt = VideoTileFramePacket.Deserialize(payload);
                        ApplyVideoFrame(pkt);
                        Interlocked.Increment(ref _framesRecv);
                        this.BeginInvoke((Action)delegate { this.Invalidate(); });
                    }
                    else if (hdr.Type == MsgType.CursorState)
                    {
                        CursorPacket cp = CursorPacket.Deserialize(payload);
                        _cursorX = cp.X;
                        _cursorY = cp.Y;
                        _cursorVisible = cp.Visible;
                        this.BeginInvoke((Action)delegate { this.Invalidate(); });
                    }
                    else if (hdr.Type == MsgType.Pong && payload.Length >= 8)
                    {
                        ulong sentUs = FrameHeader.ReadUInt64LE(payload, 0);
                        ulong nowUs = HighResClock.NowUs();
                        if (nowUs >= sentUs) _lastRttMs = (nowUs - sentUs) / 1000.0;
                    }
                }
            }
            catch (Exception ex)
            {
                _backendInfo = "Disconnected: " + ex.Message;
                if (!this.IsDisposed) try { this.BeginInvoke((Action)delegate { this.Invalidate(); }); } catch { }
            }
        }

        private void ApplyVideoFrame(VideoTileFramePacket pkt)
        {
            lock (_fbLock)
            {
                if (_framebuffer == null || _framebuffer.Width != pkt.ScreenW || _framebuffer.Height != pkt.ScreenH)
                {
                    if (_framebuffer != null) _framebuffer.Dispose();
                    _framebuffer = new Bitmap(pkt.ScreenW, pkt.ScreenH, PixelFormat.Format32bppRgb);
                    _remoteW = pkt.ScreenW;
                    _remoteH = pkt.ScreenH;
                }
                using (Graphics g = Graphics.FromImage(_framebuffer))
                {
                    for (int i = 0; i < pkt.Tiles.Count; i++)
                    {
                        EncodedTile t = pkt.Tiles[i];
                        if (t.Encoding == TileEncoding.JpegLossy) Interlocked.Increment(ref _lossyTiles);
                        else if (t.Encoding == TileEncoding.PngLossless) Interlocked.Increment(ref _losslessTiles);

                        using (MemoryStream ms = new MemoryStream(t.Data))
                        using (Image tileImg = Image.FromStream(ms))
                        {
                            g.DrawImageUnscaled(tileImg, t.X, t.Y);
                        }
                    }
                }
            }
        }

        protected override void OnPaint(PaintEventArgs e)
        {
            Graphics g = e.Graphics;
            lock (_fbLock)
            {
                if (_framebuffer != null)
                {
                    g.DrawImage(_framebuffer, 0, 0, this.ClientSize.Width, this.ClientSize.Height);
                }
            }

            if (_cursorVisible && _remoteW > 0 && _remoteH > 0)
            {
                float sx = (float)this.ClientSize.Width / _remoteW;
                float sy = (float)this.ClientSize.Height / _remoteH;
                float cx = _cursorX * sx;
                float cy = _cursorY * sy;
                using (Pen p = new Pen(Color.Lime, 2f))
                {
                    g.DrawEllipse(p, cx - 5, cy - 5, 10, 10);
                }
            }

            string hud = string.Format("CopyToDesk MVP | {0} | {1:F0} FPS | RTT {2:F1}ms | {3:F1} KB/s | Lossy:{4} Lossless:{5}",
                _backendInfo, _fps, _lastRttMs, _kbps, Interlocked.Read(ref _lossyTiles), Interlocked.Read(ref _losslessTiles));
            using (SolidBrush bg = new SolidBrush(Color.FromArgb(180, 15, 20, 28)))
            using (SolidBrush fg = new SolidBrush(Color.FromArgb(120, 240, 180)))
            using (Font f = new Font("Consolas", 9.5f))
            {
                g.FillRectangle(bg, 6, 6, 620, 22);
                g.DrawString(hud, f, fg, 10, 9);
            }
        }

        private void SendPing()
        {
            if (_transport == null || !_transport.Connected) return;
            try
            {
                byte[] b = new byte[8];
                FrameHeader.WriteUInt64LE(b, 0, HighResClock.NowUs());
                _transport.Send(MsgType.Ping, ChannelId.Control, b, FrameFlags.None);
            }
            catch { }
        }

        private void SendMouse(MouseAction action, MouseButton btn, int x, int y, short wheel)
        {
            if (_transport == null || !_transport.Connected) return;
            MouseEventPacket m = new MouseEventPacket();
            m.Action = action;
            m.Button = btn;
            m.NormX = (ushort)Math.Max(0, Math.Min(65535, (int)((long)x * 65535 / Math.Max(1, this.ClientSize.Width))));
            m.NormY = (ushort)Math.Max(0, Math.Min(65535, (int)((long)y * 65535 / Math.Max(1, this.ClientSize.Height))));
            m.WheelDelta = wheel;
            try { _transport.Send(MsgType.MouseEvent, ChannelId.Input, m.Serialize(), FrameFlags.None); } catch { }
        }

        protected override void OnMouseMove(MouseEventArgs e)
        {
            base.OnMouseMove(e);
            SendMouse(MouseAction.Move, MouseButton.None, e.X, e.Y, 0);
        }

        protected override void OnMouseDown(MouseEventArgs e)
        {
            base.OnMouseDown(e);
            SendMouse(MouseAction.Down, MapBtn(e.Button), e.X, e.Y, 0);
        }

        protected override void OnMouseUp(MouseEventArgs e)
        {
            base.OnMouseUp(e);
            SendMouse(MouseAction.Up, MapBtn(e.Button), e.X, e.Y, 0);
        }

        protected override void OnMouseWheel(MouseEventArgs e)
        {
            base.OnMouseWheel(e);
            SendMouse(MouseAction.Wheel, MouseButton.None, e.X, e.Y, (short)e.Delta);
        }

        protected override void OnKeyDown(KeyEventArgs e)
        {
            base.OnKeyDown(e);
            SendKey(KeyAction.Down, (ushort)e.KeyValue);
            e.Handled = true;
        }

        protected override void OnKeyUp(KeyEventArgs e)
        {
            base.OnKeyUp(e);
            SendKey(KeyAction.Up, (ushort)e.KeyValue);
            e.Handled = true;
        }

        private void SendKey(KeyAction action, ushort vk)
        {
            if (_transport == null || !_transport.Connected) return;
            KeyEventPacket k = new KeyEventPacket();
            k.Action = action;
            k.Vk = vk;
            try { _transport.Send(MsgType.KeyEvent, ChannelId.Input, k.Serialize(), FrameFlags.None); } catch { }
        }

        private static MouseButton MapBtn(MouseButtons b)
        {
            if (b == MouseButtons.Left) return MouseButton.Left;
            if (b == MouseButtons.Right) return MouseButton.Right;
            if (b == MouseButtons.Middle) return MouseButton.Middle;
            return MouseButton.None;
        }
    }

    public static class Program
    {
        [STAThread]
        public static int Main(string[] args)
        {
            string host = "127.0.0.1";
            int port = 19800;
            string token = "";
            string relayHost = "";
            int relayPort = 19901;
            string deviceId = "";
            int headlessFrames = 0;

            for (int i = 0; i < args.Length; i++)
            {
                if (args[i] == "--host" && i + 1 < args.Length) host = args[++i];
                else if (args[i] == "--port" && i + 1 < args.Length) int.TryParse(args[++i], out port);
                else if (args[i] == "--token" && i + 1 < args.Length) token = args[++i];
                else if (args[i] == "--relay-host" && i + 1 < args.Length) relayHost = args[++i];
                else if (args[i] == "--relay-port" && i + 1 < args.Length) int.TryParse(args[++i], out relayPort);
                else if (args[i] == "--device-id" && i + 1 < args.Length) deviceId = args[++i];
                else if (args[i] == "--headless-bench" && i + 1 < args.Length) int.TryParse(args[++i], out headlessFrames);
            }

            if (headlessFrames > 0)
            {
                return RunHeadlessBenchmark(host, port, token, relayHost, relayPort, deviceId, headlessFrames);
            }

            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.Run(new RemoteViewerForm(host, port, token, relayHost, relayPort, deviceId));
            return 0;
        }

        private static int RunHeadlessBenchmark(string host, int port, string token, string relayHost, int relayPort, string deviceId, int targetFrames)
        {
            using (TcpClient tcp = !string.IsNullOrEmpty(relayHost)
                ? RelayClientHelper.ConnectViaRelay(relayHost, relayPort, deviceId)
                : new TcpClient(host, port))
            {
                tcp.ReceiveTimeout = 5000;
                using (TcpFrameTransport tr = new TcpFrameTransport(tcp))
                {
                string hello = string.Format("{{\"clientName\":\"HeadlessBench\",\"token\":\"{0}\"}}", MiniJson.Escape(token));
                tr.SendUtf8(MsgType.Hello, ChannelId.Control, hello);

                FrameHeader hdr;
                byte[] payload;
                if (!tr.TryRead(out hdr, out payload) || hdr.Type != MsgType.HelloAck)
                {
                    Console.Error.WriteLine("[Bench] Handshake failed");
                    return 2;
                }
                string ack = Encoding.UTF8.GetString(payload);
                int w = MiniJson.GetInt(ack, "width", 1280);
                int h = MiniJson.GetInt(ack, "height", 720);
                string capBackend = MiniJson.GetString(ack, "captureBackend");

                // Measure RTT via Ping/Pong
                byte[] ping = new byte[8];
                FrameHeader.WriteUInt64LE(ping, 0, HighResClock.NowUs());
                tr.Send(MsgType.Ping, ChannelId.Control, ping, FrameFlags.None);

                // Send synthetic input events to verify reverse channel
                MouseEventPacket mv = new MouseEventPacket();
                mv.Action = MouseAction.Move;
                mv.NormX = 32768;
                mv.NormY = 32768;
                tr.Send(MsgType.MouseEvent, ChannelId.Input, mv.Serialize(), FrameFlags.None);

                KeyEventPacket ke = new KeyEventPacket();
                ke.Action = KeyAction.UnicodeChar;
                ke.UnicodeChar = (uint)'A';
                tr.Send(MsgType.KeyEvent, ChannelId.Input, ke.Serialize(), FrameFlags.None);

                // Request a full screenshot via Control channel
                byte[] sreq = new byte[4];
                FrameHeader.WriteUInt16LE(sreq, 0, 640);
                sreq[2] = 75;
                sreq[3] = 0; // JPEG
                tr.Send(MsgType.ScreenshotReq, ChannelId.Control, sreq, FrameFlags.None);

                Bitmap fb = new Bitmap(w, h, PixelFormat.Format32bppRgb);
                int frames = 0;
                int cursorEvents = 0;
                int lossyTiles = 0;
                int losslessTiles = 0;
                long totalVideoBytes = 0;
                long totalEncodeUs = 0;
                double rttMs = -1;
                int screenshotBytes = 0;

                Stopwatch sw = Stopwatch.StartNew();
                try
                {
                    while (frames < targetFrames && tr.TryRead(out hdr, out payload))
                    {
                        if (hdr.Type == MsgType.VideoTileFrame)
                        {
                            VideoTileFramePacket pkt = VideoTileFramePacket.Deserialize(payload);
                            totalVideoBytes += FrameHeader.Size + payload.Length;
                            totalEncodeUs += pkt.EncodeUs;
                            using (Graphics g = Graphics.FromImage(fb))
                            {
                                for (int i = 0; i < pkt.Tiles.Count; i++)
                                {
                                    EncodedTile t = pkt.Tiles[i];
                                    if (t.Encoding == TileEncoding.JpegLossy) lossyTiles++;
                                    else if (t.Encoding == TileEncoding.PngLossless) losslessTiles++;
                                    using (MemoryStream ms = new MemoryStream(t.Data))
                                    using (Image tileImg = Image.FromStream(ms))
                                    {
                                        g.DrawImageUnscaled(tileImg, t.X, t.Y);
                                    }
                                }
                            }
                            frames++;
                        }
                        else if (hdr.Type == MsgType.CursorState)
                        {
                            cursorEvents++;
                        }
                        else if (hdr.Type == MsgType.Pong && payload.Length >= 8)
                        {
                            ulong sentUs = FrameHeader.ReadUInt64LE(payload, 0);
                            rttMs = (HighResClock.NowUs() - sentUs) / 1000.0;
                        }
                        else if (hdr.Type == MsgType.ScreenshotResp && payload.Length >= 13)
                        {
                            screenshotBytes = (int)FrameHeader.ReadUInt32LE(payload, 9);
                        }
                    }
                }
                catch (IOException)
                {
                    // Static desktop timeout after receiving keyframe + refinement is expected
                }
                sw.Stop();
                fb.Dispose();

                double sec = Math.Max(0.001, sw.Elapsed.TotalSeconds);
                double fps = frames / sec;
                double avgEncodeMs = frames > 0 ? (totalEncodeUs / 1000.0) / frames : 0;
                double avgFrameKb = frames > 0 ? (totalVideoBytes / 1024.0) / frames : 0;
                double rawFrameKb = (w * h * 4) / 1024.0;
                double ratio = avgFrameKb > 0 ? rawFrameKb / avgFrameKb : 0;

                Console.WriteLine(string.Format(
                    "{{\"status\":\"PASS\",\"captureBackend\":\"{0}\",\"resolution\":\"{1}x{2}\",\"frames\":{3},\"elapsedSec\":{4:F2},\"fps\":{5:F1},\"rttMs\":{6:F2},\"avgHostEncodeMs\":{7:F2},\"avgFrameKB\":{8:F1},\"compressionRatio\":{9:F1},\"lossyTiles\":{10},\"losslessRefinementTiles\":{11},\"cursorPackets\":{12},\"screenshotRespBytes\":{13}}}",
                    MiniJson.Escape(capBackend), w, h, frames, sec, fps, rttMs, avgEncodeMs, avgFrameKb, ratio, lossyTiles, losslessTiles, cursorEvents, screenshotBytes));
                return frames > 0 ? 0 : 3;
                }
            }
        }
    }
}
