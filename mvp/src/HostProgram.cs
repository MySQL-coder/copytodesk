using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using CopyToDesk.Core;

namespace CopyToDesk.Host
{
    internal class ClientSession
    {
        public string Id;
        public string Name;
        public TcpFrameTransport Transport;
        public volatile bool Alive = true;
        public volatile bool NeedsKeyFrame = true;
        public ulong LastCursorHash;
        public int LastCursorX = int.MinValue;
        public int LastCursorY = int.MinValue;
        public bool LastCursorVisible;
    }

    public static class Program
    {
        private static volatile bool _running = true;
        private static readonly object _clientsLock = new object();
        private static readonly List<ClientSession> _clients = new List<ClientSession>();
        private static readonly object _frameLock = new object();
        private static readonly object _clipLock = new object();
        private static byte[] _latestFrameBgra;
        private static string _clipboardText = "";
        private static int _screenW;
        private static int _screenH;
        private static string _captureBackend;
        private static string _codecProbeJson = "{}";
        private static IInputInjector _injector;
        private static string _token;
        private static string _fileRoot;
        private static bool _allowExec;
        private static string[] _execAllowlist = new string[] { "echo", "hostname", "whoami", "ver" };

        public static int Main(string[] args)
        {
            string bindIp = "127.0.0.1";
            int port = 19800;
            int targetFps = 30;
            int tileSize = 128;
            int quality = 75;
            bool allowInput = false;
            bool synthetic = false;
            bool forceGdi = false;
            string relayHost = "";
            int relayPort = 19901;
            string deviceId = "888100200";
            string deviceName = Environment.MachineName;
            _token = GenerateRandomToken();
            _fileRoot = Path.GetFullPath(Path.Combine(Directory.GetCurrentDirectory(), "shared-files"));

            for (int i = 0; i < args.Length; i++)
            {
                string a = args[i];
                if (a == "--bind" && i + 1 < args.Length) bindIp = args[++i];
                else if (a == "--port" && i + 1 < args.Length) int.TryParse(args[++i], out port);
                else if (a == "--fps" && i + 1 < args.Length) int.TryParse(args[++i], out targetFps);
                else if (a == "--tile" && i + 1 < args.Length) int.TryParse(args[++i], out tileSize);
                else if (a == "--quality" && i + 1 < args.Length) int.TryParse(args[++i], out quality);
                else if (a == "--token" && i + 1 < args.Length) _token = args[++i];
                else if (a == "--allow-input") allowInput = true;
                else if (a == "--synthetic") synthetic = true;
                else if (a == "--gdi") forceGdi = true;
                else if (a == "--relay-host" && i + 1 < args.Length) relayHost = args[++i];
                else if (a == "--relay-port" && i + 1 < args.Length) int.TryParse(args[++i], out relayPort);
                else if (a == "--device-id" && i + 1 < args.Length) deviceId = args[++i];
                else if (a == "--device-name" && i + 1 < args.Length) deviceName = args[++i];
                else if (a == "--file-root" && i + 1 < args.Length) _fileRoot = Path.GetFullPath(args[++i]);
                else if (a == "--allow-exec") _allowExec = true;
                else if (a == "--exec-allowlist" && i + 1 < args.Length)
                {
                    string[] raw = args[++i].Split(new char[] { ',' }, StringSplitOptions.RemoveEmptyEntries);
                    List<string> list = new List<string>();
                    for (int k = 0; k < raw.Length; k++)
                    {
                        string trimmed = raw[k].Trim().ToLowerInvariant();
                        if (trimmed.Length > 0) list.Add(trimmed);
                    }
                    if (list.Count > 0) _execAllowlist = list.ToArray();
                }
            }

            Directory.CreateDirectory(_fileRoot);

            Console.CancelKeyPress += delegate(object s, ConsoleCancelEventArgs e)
            {
                e.Cancel = true;
                _running = false;
            };

            // 1. Run isolated out-of-process GPU/Codec probe (mirrors codec_checker.exe)
            _codecProbeJson = RunIsolatedCodecProbe();
            Console.WriteLine("[Host] CodecProbe: " + _codecProbeJson);

            // 2. Initialize Capturer (Tier-1 DXGI-DD -> Tier-3 GDI-BitBlt -> Synthetic)
            IScreenCapturer capturer = CreateCapturer(synthetic, forceGdi);
            _screenW = capturer.Width;
            _screenH = capturer.Height;
            _captureBackend = capturer.BackendName;
            _latestFrameBgra = new byte[_screenW * _screenH * 4];
            _injector = allowInput ? (IInputInjector)new Win32SendInputInjector() : new DryRunAuditInjector();

            Console.WriteLine(string.Format(
                "[Host] Capture={0} ({1}x{2}) | TargetFPS={3} | Input={4} | Exec={5} | FileRoot={6}",
                _captureBackend, _screenW, _screenH, targetFps, _injector.BackendName, _allowExec ? "ALLOWLIST" : "OFF", _fileRoot));
            Console.WriteLine(string.Format("[Host] Listening on {0}:{1} | Token={2}", bindIp, port, _token));

            TcpListener listener = new TcpListener(IPAddress.Parse(bindIp), port);
            listener.Start();

            // 3. Start independent Cursor thread (Channel 2)
            Thread cursorThread = new Thread(delegate() { CursorLoop(synthetic); });
            cursorThread.IsBackground = true;
            cursorThread.Start();

            // 4. Start Capture + Compound Encode broadcast thread (Channel 1)
            Thread captureThread = new Thread(delegate() { CaptureEncodeLoop(capturer, targetFps, tileSize, quality); });
            captureThread.IsBackground = true;
            captureThread.Start();

            // 5. Optional Outbound Signaling/Relay Registration thread (NAT traversal)
            if (!string.IsNullOrEmpty(relayHost))
            {
                Thread relayThread = new Thread(delegate() { RelayControlLoop(relayHost, relayPort, deviceId, deviceName); });
                relayThread.IsBackground = true;
                relayThread.Start();
            }

            // 6. Accept direct LAN/loopback clients
            try
            {
                while (_running)
                {
                    if (!listener.Pending())
                    {
                        Thread.Sleep(20);
                        continue;
                    }
                    TcpClient tcp = listener.AcceptTcpClient();
                    Thread clientThread = new Thread(delegate() { HandleClient(tcp); });
                    clientThread.IsBackground = true;
                    clientThread.Start();
                }
            }
            finally
            {
                _running = false;
                try { listener.Stop(); } catch { }
                capturer.Dispose();
                Console.WriteLine("[Host] Stopped cleanly. InputEventsHandled=" + _injector.TotalEvents);
            }
            return 0;
        }

        private static void RelayControlLoop(string relayHost, int relayPort, string deviceId, string deviceName)
        {
            while (_running)
            {
                try
                {
                    using (TcpClient ctrl = new TcpClient(relayHost, relayPort))
                    {
                        ctrl.NoDelay = true;
                        NetworkStream ns = ctrl.GetStream();
                        RelayClientHelper.WriteAsciiLine(ns, "REGISTER " + deviceId + " " + deviceName);
                        string resp = RelayClientHelper.ReadAsciiLine(ns);
                        if (resp == null || !resp.StartsWith("OK REGISTERED", StringComparison.Ordinal))
                        {
                            Console.WriteLine("[Host] Relay register rejected: " + (resp ?? "EOF"));
                            Thread.Sleep(2000);
                            continue;
                        }
                        Console.WriteLine(string.Format("[Host] Registered to Relay {0}:{1} as Device={2}", relayHost, relayPort, deviceId));

                        while (_running && ctrl.Connected)
                        {
                            string line = RelayClientHelper.ReadAsciiLine(ns);
                            if (line == null) break;
                            if (line.StartsWith("INCOMING ", StringComparison.Ordinal))
                            {
                                string sessionId = line.Substring(9).Trim();
                                Thread t = new Thread(delegate() { AcceptRelaySession(relayHost, relayPort, sessionId); });
                                t.IsBackground = true;
                                t.Start();
                            }
                        }
                    }
                }
                catch (Exception ex)
                {
                    if (_running) Console.WriteLine("[Host] Relay reconnect in 2s: " + ex.Message);
                }
                if (_running) Thread.Sleep(2000);
            }
        }

        private static void AcceptRelaySession(string relayHost, int relayPort, string sessionId)
        {
            try
            {
                TcpClient dataTcp = new TcpClient(relayHost, relayPort);
                dataTcp.NoDelay = true;
                NetworkStream ns = dataTcp.GetStream();
                RelayClientHelper.WriteAsciiLine(ns, "ACCEPT " + sessionId);
                string resp = RelayClientHelper.ReadAsciiLine(ns);
                if (resp == null || !resp.StartsWith("OK TUNNEL", StringComparison.Ordinal))
                {
                    dataTcp.Close();
                    return;
                }
                HandleClient(dataTcp);
            }
            catch (Exception ex)
            {
                if (_running) Console.WriteLine("[Host] Relay session failed: " + ex.Message);
            }
        }

        private static IScreenCapturer CreateCapturer(bool synthetic, bool forceGdi)
        {
            if (synthetic) return new SyntheticTestCapturer(1280, 720);
            if (!forceGdi)
            {
                string err;
                DxgiDesktopDuplicator dxgi = DxgiDesktopDuplicator.TryCreate(out err);
                if (dxgi != null) return dxgi;
                Console.WriteLine("[Host] DXGI-DD unavailable (" + err + "), falling back to GDI-BitBlt.");
            }
            return new GdiBitBltCapturer();
        }

        private static string RunIsolatedCodecProbe()
        {
            try
            {
                string exeDir = Path.GetDirectoryName(typeof(Program).Assembly.Location);
                string probePath = Path.Combine(exeDir, "CopyToDesk.CodecProbe.exe");
                if (!File.Exists(probePath)) return "{\"error\":\"probe_not_found\"}";

                ProcessStartInfo psi = new ProcessStartInfo(probePath);
                psi.UseShellExecute = false;
                psi.RedirectStandardOutput = true;
                psi.CreateNoWindow = true;
                using (Process p = Process.Start(psi))
                {
                    string output = p.StandardOutput.ReadToEnd().Trim();
                    if (!p.WaitForExit(3000))
                    {
                        try { p.Kill(); } catch { }
                        return "{\"error\":\"probe_timeout\"}";
                    }
                    return string.IsNullOrEmpty(output) ? "{\"error\":\"empty_output\"}" : output;
                }
            }
            catch (Exception ex)
            {
                return "{\"error\":\"" + MiniJson.Escape(ex.Message) + "\"}";
            }
        }

        private static void CaptureEncodeLoop(IScreenCapturer capturer, int targetFps, int tileSize, int quality)
        {
            int intervalMs = Math.Max(5, 1000 / Math.Max(1, targetFps));
            byte[] scratch = new byte[_screenW * _screenH * 4];
            bool hasInitialFrame = false;

            using (CompoundTileEncoder encoder = new CompoundTileEncoder(_screenW, _screenH, tileSize, quality, 3))
            {
                Stopwatch sw = new Stopwatch();
                while (_running)
                {
                    sw.Restart();
                    ulong tsUs = HighResClock.NowUs();
                    bool captured = false;
                    try
                    {
                        captured = capturer.TryCaptureBgra(scratch, out tsUs);
                    }
                    catch (Exception ex)
                    {
                        Console.WriteLine("[Host] Capture exception: " + ex.Message);
                    }
                    if (captured)
                    {
                        lock (_frameLock)
                        {
                            Buffer.BlockCopy(scratch, 0, _latestFrameBgra, 0, scratch.Length);
                        }
                        hasInitialFrame = true;
                    }

                    if (hasInitialFrame)
                    {
                        List<ClientSession> snapshot;
                        bool anyKeyFrame = false;
                        lock (_clientsLock)
                        {
                            snapshot = new List<ClientSession>(_clients);
                            for (int i = 0; i < snapshot.Count; i++)
                            {
                                if (snapshot[i].NeedsKeyFrame)
                                {
                                    anyKeyFrame = true;
                                    snapshot[i].NeedsKeyFrame = false;
                                }
                            }
                        }

                        if (snapshot.Count > 0)
                        {
                            FrameFlags flags;
                            VideoTileFramePacket pkt = encoder.EncodeFrame(scratch, tsUs, anyKeyFrame, out flags);
                            if (pkt.Tiles.Count > 0 || anyKeyFrame)
                            {
                                byte[] bytes = pkt.Serialize();
                                for (int i = 0; i < snapshot.Count; i++)
                                {
                                    ClientSession c = snapshot[i];
                                    if (!c.Alive) continue;
                                    try
                                    {
                                        c.Transport.Send(MsgType.VideoTileFrame, ChannelId.Video, bytes, flags);
                                    }
                                    catch { c.Alive = false; }
                                }
                            }
                        }
                    }

                    int elapsed = (int)sw.ElapsedMilliseconds;
                    int sleep = intervalMs - elapsed;
                    if (sleep > 0) Thread.Sleep(sleep);
                }
            }
        }

        private static void CursorLoop(bool synthetic)
        {
            int synthTick = 0;
            while (_running)
            {
                int cx, cy;
                bool vis;
                ulong hash;
                if (synthetic)
                {
                    synthTick++;
                    cx = 100 + ((synthTick * 7) % Math.Max(1, _screenW - 200));
                    cy = 100 + ((synthTick * 3) % Math.Max(1, _screenH - 200));
                    vis = true;
                    hash = 0x1001UL;
                }
                else
                {
                    if (!CursorMonitor.TryPoll(out cx, out cy, out vis, out hash))
                    {
                        Thread.Sleep(16);
                        continue;
                    }
                }

                List<ClientSession> snapshot;
                lock (_clientsLock) snapshot = new List<ClientSession>(_clients);
                for (int i = 0; i < snapshot.Count; i++)
                {
                    ClientSession c = snapshot[i];
                    if (!c.Alive) continue;
                    if (c.LastCursorX != cx || c.LastCursorY != cy || c.LastCursorVisible != vis || c.LastCursorHash != hash)
                    {
                        c.LastCursorX = cx;
                        c.LastCursorY = cy;
                        c.LastCursorVisible = vis;
                        c.LastCursorHash = hash;
                        CursorPacket cp = new CursorPacket();
                        cp.X = cx; cp.Y = cy; cp.Visible = vis; cp.ShapeHash = hash;
                        try
                        {
                            c.Transport.Send(MsgType.CursorState, ChannelId.Cursor, cp.Serialize(), FrameFlags.None);
                        }
                        catch { c.Alive = false; }
                    }
                }
                Thread.Sleep(synthetic ? 33 : 8);
            }
        }

        private static void HandleClient(TcpClient tcp)
        {
            ClientSession session = null;
            using (TcpFrameTransport tr = new TcpFrameTransport(tcp))
            {
                try
                {
                    FrameHeader hdr;
                    byte[] payload;
                    if (!tr.TryRead(out hdr, out payload) || hdr.Type != MsgType.Hello)
                        return;

                    string helloJson = Encoding.UTF8.GetString(payload);
                    string clientToken = MiniJson.GetString(helloJson, "token");
                    string clientName = MiniJson.GetString(helloJson, "clientName");
                    if (!string.Equals(clientToken, _token, StringComparison.Ordinal))
                    {
                        Console.WriteLine("[Host] Rejected client (invalid token): " + clientName);
                        return;
                    }

                    session = new ClientSession();
                    session.Id = Guid.NewGuid().ToString("N").Substring(0, 8);
                    session.Name = string.IsNullOrEmpty(clientName) ? "unknown" : clientName;
                    session.Transport = tr;

                    string ack = string.Format(
                        "{{\"sessionId\":\"{0}\",\"width\":{1},\"height\":{2},\"tileSize\":128,\"captureBackend\":\"{3}\",\"inputBackend\":\"{4}\",\"execEnabled\":{5},\"codecProbe\":{6}}}",
                        session.Id, _screenW, _screenH, MiniJson.Escape(_captureBackend), MiniJson.Escape(_injector.BackendName),
                        _allowExec ? "true" : "false", _codecProbeJson);
                    tr.SendUtf8(MsgType.HelloAck, ChannelId.Control, ack);

                    lock (_clientsLock) _clients.Add(session);
                    Console.WriteLine(string.Format("[Host] Client connected: id={0} name={1}", session.Id, session.Name));

                    while (_running && session.Alive && tr.TryRead(out hdr, out payload))
                    {
                        if (hdr.Type == MsgType.Ping && payload.Length >= 8)
                        {
                            byte[] pong = new byte[16];
                            Buffer.BlockCopy(payload, 0, pong, 0, 8);
                            FrameHeader.WriteUInt64LE(pong, 8, HighResClock.NowUs());
                            tr.Send(MsgType.Pong, ChannelId.Control, pong, FrameFlags.None);
                        }
                        else if (hdr.Type == MsgType.RequestKeyFrame)
                        {
                            session.NeedsKeyFrame = true;
                        }
                        else if (hdr.Type == MsgType.MouseEvent && payload.Length >= 8)
                        {
                            _injector.InjectMouse(MouseEventPacket.Deserialize(payload));
                        }
                        else if (hdr.Type == MsgType.KeyEvent && payload.Length >= 11)
                        {
                            _injector.InjectKey(KeyEventPacket.Deserialize(payload));
                        }
                        else if (hdr.Type == MsgType.ScreenshotReq && payload.Length >= 4)
                        {
                            HandleScreenshot(tr, payload);
                        }
                        else if (hdr.Type == MsgType.ClipboardGet)
                        {
                            string txt;
                            lock (_clipLock) txt = _clipboardText;
                            tr.SendUtf8(MsgType.ClipboardData, ChannelId.Control, txt);
                        }
                        else if (hdr.Type == MsgType.ClipboardData)
                        {
                            string txt = Encoding.UTF8.GetString(payload);
                            lock (_clipLock) _clipboardText = txt;
                            tr.SendUtf8(MsgType.ClipboardData, ChannelId.Control, txt);
                        }
                        else if (hdr.Type == MsgType.FileListReq)
                        {
                            HandleFileList(tr, Encoding.UTF8.GetString(payload));
                        }
                        else if (hdr.Type == MsgType.FileUploadReq)
                        {
                            HandleFileUpload(tr, payload);
                        }
                        else if (hdr.Type == MsgType.FileDownloadReq)
                        {
                            HandleFileDownload(tr, Encoding.UTF8.GetString(payload));
                        }
                        else if (hdr.Type == MsgType.ExecReq)
                        {
                            HandleGuardedExec(tr, Encoding.UTF8.GetString(payload));
                        }
                    }
                }
                catch (Exception ex)
                {
                    if (_running) Console.WriteLine("[Host] Client error: " + ex.Message);
                }
                finally
                {
                    if (session != null)
                    {
                        session.Alive = false;
                        lock (_clientsLock) _clients.Remove(session);
                        Console.WriteLine("[Host] Client disconnected: id=" + session.Id);
                    }
                }
            }
        }

        private static void HandleScreenshot(TcpFrameTransport tr, byte[] payload)
        {
            ushort maxW = FrameHeader.ReadUInt16LE(payload, 0);
            byte q = payload[2];
            bool png = payload[3] == 1;
            byte[] copy = new byte[_latestFrameBgra.Length];
            lock (_frameLock) Buffer.BlockCopy(_latestFrameBgra, 0, copy, 0, copy.Length);
            int outW, outH;
            byte[] img = CompoundTileEncoder.EncodeFullScreenshot(copy, _screenW, _screenH, maxW, q, png, out outW, out outH);
            byte[] resp = new byte[13 + img.Length];
            FrameHeader.WriteUInt16LE(resp, 0, (ushort)_screenW);
            FrameHeader.WriteUInt16LE(resp, 2, (ushort)_screenH);
            FrameHeader.WriteUInt16LE(resp, 4, (ushort)outW);
            FrameHeader.WriteUInt16LE(resp, 6, (ushort)outH);
            resp[8] = (byte)(png ? 1 : 0);
            FrameHeader.WriteUInt32LE(resp, 9, (uint)img.Length);
            Buffer.BlockCopy(img, 0, resp, 13, img.Length);
            tr.Send(MsgType.ScreenshotResp, ChannelId.Control, resp, FrameFlags.None);
        }

        private static string ResolveSandboxedPath(string relPath)
        {
            string clean = (relPath ?? "").Replace('/', Path.DirectorySeparatorChar).TrimStart(Path.DirectorySeparatorChar);
            string full = Path.GetFullPath(Path.Combine(_fileRoot, clean));
            string rootWithSep = _fileRoot.EndsWith(Path.DirectorySeparatorChar.ToString()) ? _fileRoot : _fileRoot + Path.DirectorySeparatorChar;
            if (!full.Equals(_fileRoot, StringComparison.OrdinalIgnoreCase) &&
                !full.StartsWith(rootWithSep, StringComparison.OrdinalIgnoreCase))
            {
                throw new UnauthorizedAccessException("Path escapes sandbox fileRoot");
            }
            return full;
        }

        private static void HandleFileList(TcpFrameTransport tr, string relPath)
        {
            try
            {
                string dir = ResolveSandboxedPath(relPath);
                if (!Directory.Exists(dir))
                {
                    tr.SendUtf8(MsgType.FileListResp, ChannelId.File, "{\"ok\":false,\"error\":\"Directory not found\"}");
                    return;
                }
                List<string> items = new List<string>();
                string[] dirs = Directory.GetDirectories(dir);
                for (int i = 0; i < dirs.Length; i++)
                {
                    DirectoryInfo di = new DirectoryInfo(dirs[i]);
                    items.Add(string.Format("{{\"name\":\"{0}\",\"isDir\":true,\"size\":0}}", MiniJson.Escape(di.Name)));
                }
                string[] files = Directory.GetFiles(dir);
                for (int i = 0; i < files.Length; i++)
                {
                    FileInfo fi = new FileInfo(files[i]);
                    items.Add(string.Format("{{\"name\":\"{0}\",\"isDir\":false,\"size\":{1}}}", MiniJson.Escape(fi.Name), fi.Length));
                }
                string json = string.Format("{{\"ok\":true,\"path\":\"{0}\",\"entries\":[{1}]}}",
                    MiniJson.Escape(relPath ?? ""), string.Join(",", items.ToArray()));
                tr.SendUtf8(MsgType.FileListResp, ChannelId.File, json);
            }
            catch (Exception ex)
            {
                tr.SendUtf8(MsgType.FileListResp, ChannelId.File, "{\"ok\":false,\"error\":\"" + MiniJson.Escape(ex.Message) + "\"}");
            }
        }

        private static void HandleFileUpload(TcpFrameTransport tr, byte[] payload)
        {
            try
            {
                if (payload.Length < 66) throw new InvalidDataException("Short upload packet");
                ushort pathLen = FrameHeader.ReadUInt16LE(payload, 0);
                if (2 + pathLen + 64 > payload.Length) throw new InvalidDataException("Truncated upload header");
                string relPath = Encoding.UTF8.GetString(payload, 2, pathLen);
                string expectedSha = Encoding.ASCII.GetString(payload, 2 + pathLen, 64).ToLowerInvariant();
                int dataOff = 2 + pathLen + 64;
                int dataLen = payload.Length - dataOff;

                byte[] fileBytes = new byte[dataLen];
                Buffer.BlockCopy(payload, dataOff, fileBytes, 0, dataLen);
                string actualSha = ComputeSha256Hex(fileBytes);
                if (!string.Equals(expectedSha, actualSha, StringComparison.OrdinalIgnoreCase))
                {
                    tr.SendUtf8(MsgType.FileUploadResp, ChannelId.File,
                        "{\"ok\":false,\"error\":\"SHA-256 mismatch\"}");
                    return;
                }

                string target = ResolveSandboxedPath(relPath);
                string parent = Path.GetDirectoryName(target);
                if (!string.IsNullOrEmpty(parent)) Directory.CreateDirectory(parent);
                File.WriteAllBytes(target, fileBytes);

                tr.SendUtf8(MsgType.FileUploadResp, ChannelId.File, string.Format(
                    "{{\"ok\":true,\"path\":\"{0}\",\"bytes\":{1},\"sha256\":\"{2}\"}}",
                    MiniJson.Escape(relPath), dataLen, actualSha));
            }
            catch (Exception ex)
            {
                tr.SendUtf8(MsgType.FileUploadResp, ChannelId.File, "{\"ok\":false,\"error\":\"" + MiniJson.Escape(ex.Message) + "\"}");
            }
        }

        private static void HandleFileDownload(TcpFrameTransport tr, string relPath)
        {
            try
            {
                string target = ResolveSandboxedPath(relPath);
                if (!File.Exists(target)) throw new FileNotFoundException("File not found");
                byte[] data = File.ReadAllBytes(target);
                if (data.Length > 15 * 1024 * 1024) throw new InvalidOperationException("File exceeds 15MB single-frame limit");
                string sha = ComputeSha256Hex(data);
                byte[] shaBytes = Encoding.ASCII.GetBytes(sha);
                byte[] resp = new byte[1 + 64 + 2 + data.Length];
                resp[0] = 1; // ok
                Buffer.BlockCopy(shaBytes, 0, resp, 1, 64);
                FrameHeader.WriteUInt16LE(resp, 65, 0);
                Buffer.BlockCopy(data, 0, resp, 67, data.Length);
                tr.Send(MsgType.FileDownloadResp, ChannelId.File, resp, FrameFlags.None);
            }
            catch (Exception ex)
            {
                byte[] errBytes = Encoding.UTF8.GetBytes(ex.Message);
                byte[] resp = new byte[1 + 64 + 2 + errBytes.Length];
                resp[0] = 0; // fail
                FrameHeader.WriteUInt16LE(resp, 65, (ushort)errBytes.Length);
                Buffer.BlockCopy(errBytes, 0, resp, 67, errBytes.Length);
                tr.Send(MsgType.FileDownloadResp, ChannelId.File, resp, FrameFlags.None);
            }
        }

        private static void HandleGuardedExec(TcpFrameTransport tr, string json)
        {
            string cmd = MiniJson.GetString(json, "command").Trim();
            int timeoutMs = Math.Max(500, Math.Min(15000, MiniJson.GetInt(json, "timeoutMs", 5000)));

            if (!_allowExec)
            {
                tr.SendUtf8(MsgType.ExecResp, ChannelId.Control,
                    "{\"ok\":false,\"allowed\":false,\"error\":\"Remote exec is disabled on Host (pass --allow-exec to enable allowlisted mode)\"}");
                return;
            }

            // Guardrail: reject shell chaining metacharacters & enforce prefix allowlist
            char[] forbidden = new char[] { '&', '|', ';', '>', '<', '`', '$', '\r', '\n' };
            if (string.IsNullOrEmpty(cmd) || cmd.IndexOfAny(forbidden) >= 0)
            {
                tr.SendUtf8(MsgType.ExecResp, ChannelId.Control,
                    "{\"ok\":false,\"allowed\":false,\"error\":\"Command blocked by guardrail (empty or shell metacharacters)\"}");
                return;
            }

            string firstToken = cmd.Split(new char[] { ' ' }, StringSplitOptions.RemoveEmptyEntries)[0].ToLowerInvariant();
            bool matched = false;
            for (int i = 0; i < _execAllowlist.Length; i++)
            {
                if (string.Equals(firstToken, _execAllowlist[i], StringComparison.OrdinalIgnoreCase))
                {
                    matched = true;
                    break;
                }
            }
            if (!matched)
            {
                tr.SendUtf8(MsgType.ExecResp, ChannelId.Control, string.Format(
                    "{{\"ok\":false,\"allowed\":false,\"error\":\"Command '{0}' not in allowlist\"}}", MiniJson.Escape(firstToken)));
                return;
            }

            try
            {
                ProcessStartInfo psi = new ProcessStartInfo("cmd.exe", "/c " + cmd);
                psi.UseShellExecute = false;
                psi.RedirectStandardOutput = true;
                psi.RedirectStandardError = true;
                psi.CreateNoWindow = true;
                using (Process p = Process.Start(psi))
                {
                    string stdout = p.StandardOutput.ReadToEnd();
                    string stderr = p.StandardError.ReadToEnd();
                    if (!p.WaitForExit(timeoutMs))
                    {
                        try { p.Kill(); } catch { }
                        tr.SendUtf8(MsgType.ExecResp, ChannelId.Control,
                            "{\"ok\":false,\"allowed\":true,\"error\":\"Command timed out\"}");
                        return;
                    }
                    if (stdout.Length > 8192) stdout = stdout.Substring(0, 8192);
                    if (stderr.Length > 4096) stderr = stderr.Substring(0, 4096);
                    tr.SendUtf8(MsgType.ExecResp, ChannelId.Control, string.Format(
                        "{{\"ok\":true,\"allowed\":true,\"exitCode\":{0},\"stdout\":\"{1}\",\"stderr\":\"{2}\"}}",
                        p.ExitCode, MiniJson.Escape(stdout), MiniJson.Escape(stderr)));
                }
            }
            catch (Exception ex)
            {
                tr.SendUtf8(MsgType.ExecResp, ChannelId.Control,
                    "{\"ok\":false,\"allowed\":true,\"error\":\"" + MiniJson.Escape(ex.Message) + "\"}");
            }
        }

        private static string ComputeSha256Hex(byte[] data)
        {
            using (SHA256 sha = SHA256.Create())
            {
                byte[] h = sha.ComputeHash(data);
                return BitConverter.ToString(h).Replace("-", "").ToLowerInvariant();
            }
        }

        private static string GenerateRandomToken()
        {
            byte[] b = new byte[16];
            using (RNGCryptoServiceProvider rng = new RNGCryptoServiceProvider()) rng.GetBytes(b);
            return BitConverter.ToString(b).Replace("-", "").ToLowerInvariant();
        }
    }
}
