using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

namespace CopyToDesk.Core
{
    public interface IScreenCapturer : IDisposable
    {
        string BackendName { get; }
        int Width { get; }
        int Height { get; }
        bool TryCaptureBgra(byte[] targetBuffer, out ulong captureTsUs);
    }

    public static class HighResClock
    {
        private static readonly Stopwatch _sw = Stopwatch.StartNew();
        public static ulong NowUs()
        {
            return (ulong)(_sw.ElapsedTicks * 1000000L / Stopwatch.Frequency);
        }
    }

    /// <summary>
    /// Tier-1: Windows DXGI Desktop Duplication via direct COM vtable calls (zero external SDK deps).
    /// </summary>
    public class DxgiDesktopDuplicator : IScreenCapturer
    {
        private IntPtr _device = IntPtr.Zero;
        private IntPtr _context = IntPtr.Zero;
        private IntPtr _duplication = IntPtr.Zero;
        private IntPtr _stagingTex = IntPtr.Zero;
        private int _width;
        private int _height;
        private bool _disposed;

        public string BackendName { get { return "DXGI-DD"; } }
        public int Width { get { return _width; } }
        public int Height { get { return _height; } }

        private static readonly Guid IID_IDXGIDevice = new Guid("54ec77fa-1377-44e6-8c32-88fd5f44c84c");
        private static readonly Guid IID_IDXGIOutput1 = new Guid("00cddea8-939b-4b83-a340-a685226666cc");
        private static readonly Guid IID_ID3D11Texture2D = new Guid("6f15aaf2-d208-4e89-9ab4-489535d34f9c");

        [DllImport("d3d11.dll")]
        private static extern int D3D11CreateDevice(
            IntPtr pAdapter, int driverType, IntPtr software, uint flags,
            int[] pFeatureLevels, int featureLevels, uint sdkVersion,
            out IntPtr ppDevice, out int pFeatureLevel, out IntPtr ppImmediateContext);

        [StructLayout(LayoutKind.Sequential)]
        private struct D3D11_TEXTURE2D_DESC
        {
            public uint Width;
            public uint Height;
            public uint MipLevels;
            public uint ArraySize;
            public uint Format;
            public uint SampleCount;
            public uint SampleQuality;
            public uint Usage;
            public uint BindFlags;
            public uint CPUAccessFlags;
            public uint MiscFlags;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct D3D11_MAPPED_SUBRESOURCE
        {
            public IntPtr pData;
            public uint RowPitch;
            public uint DepthPitch;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct DXGI_OUTDUPL_DESC
        {
            public uint Width;
            public uint Height;
            public uint RefreshNum;
            public uint RefreshDen;
            public uint Format;
            public uint ScanlineOrdering;
            public uint Scaling;
            public uint Rotation;
            public int DesktopImageInSystemMemory;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct DXGI_OUTDUPL_FRAME_INFO
        {
            public long LastPresentTime;
            public long LastMouseUpdateTime;
            public uint AccumulatedFrames;
            public int RectsCoalesced;
            public int ProtectedContentMaskedOut;
            public int PointerX;
            public int PointerY;
            public int PointerVisible;
            public uint TotalMetadataBufferSize;
            public uint PointerShapeBufferSize;
        }

        // COM vtable delegates
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int GetAdapterDel(IntPtr dxgiDevice, out IntPtr adapter);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int EnumOutputsDel(IntPtr adapter, uint output, out IntPtr dxgiOutput);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int DuplicateOutputDel(IntPtr output1, IntPtr device, out IntPtr duplication);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate void GetDescDuplDel(IntPtr dupl, out DXGI_OUTDUPL_DESC desc);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int AcquireNextFrameDel(IntPtr dupl, uint timeoutMs, out DXGI_OUTDUPL_FRAME_INFO info, out IntPtr desktopResource);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int ReleaseFrameDel(IntPtr dupl);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int CreateTexture2DDel(IntPtr device, ref D3D11_TEXTURE2D_DESC desc, IntPtr initData, out IntPtr tex);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate void CopyResourceDel(IntPtr ctx, IntPtr dst, IntPtr src);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int MapDel(IntPtr ctx, IntPtr res, uint subresource, uint mapType, uint mapFlags, out D3D11_MAPPED_SUBRESOURCE mapped);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate void UnmapDel(IntPtr ctx, IntPtr res, uint subresource);

        private static T GetVtbl<T>(IntPtr comPtr, int index) where T : class
        {
            IntPtr vtbl = Marshal.ReadIntPtr(comPtr);
            IntPtr fnPtr = Marshal.ReadIntPtr(vtbl, index * IntPtr.Size);
            return Marshal.GetDelegateForFunctionPointer(fnPtr, typeof(T)) as T;
        }

        public static DxgiDesktopDuplicator TryCreate(out string error)
        {
            error = null;
            DxgiDesktopDuplicator cap = new DxgiDesktopDuplicator();
            IntPtr dxgiDevice = IntPtr.Zero;
            IntPtr adapter = IntPtr.Zero;
            IntPtr output = IntPtr.Zero;
            IntPtr output1 = IntPtr.Zero;
            try
            {
                int fl;
                int[] levels = new int[] { 0xb100, 0xb000, 0xa100, 0xa000 };
                int hr = D3D11CreateDevice(IntPtr.Zero, 1 /* HARDWARE */, IntPtr.Zero, 0, levels, levels.Length, 7, out cap._device, out fl, out cap._context);
                if (hr < 0) { error = "D3D11CreateDevice failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                Guid iid = IID_IDXGIDevice;
                hr = Marshal.QueryInterface(cap._device, ref iid, out dxgiDevice);
                if (hr < 0) { error = "QI IDXGIDevice failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                // IDXGIDevice::GetAdapter is vtable slot 7
                hr = GetVtbl<GetAdapterDel>(dxgiDevice, 7)(dxgiDevice, out adapter);
                if (hr < 0) { error = "GetAdapter failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                // IDXGIAdapter::EnumOutputs is vtable slot 7
                hr = GetVtbl<EnumOutputsDel>(adapter, 7)(adapter, 0, out output);
                if (hr < 0) { error = "EnumOutputs(0) failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                Guid iidOut1 = IID_IDXGIOutput1;
                hr = Marshal.QueryInterface(output, ref iidOut1, out output1);
                if (hr < 0) { error = "QI IDXGIOutput1 failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                // IDXGIOutput1::DuplicateOutput is vtable slot 22
                hr = GetVtbl<DuplicateOutputDel>(output1, 22)(output1, cap._device, out cap._duplication);
                if (hr < 0) { error = "DuplicateOutput failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                // IDXGIOutputDuplication::GetDesc is vtable slot 7
                DXGI_OUTDUPL_DESC desc;
                GetVtbl<GetDescDuplDel>(cap._duplication, 7)(cap._duplication, out desc);
                cap._width = (int)desc.Width;
                cap._height = (int)desc.Height;
                if (cap._width <= 0 || cap._height <= 0) { error = "Invalid DXGI output dimensions"; cap.Dispose(); return null; }

                D3D11_TEXTURE2D_DESC td = new D3D11_TEXTURE2D_DESC();
                td.Width = desc.Width;
                td.Height = desc.Height;
                td.MipLevels = 1;
                td.ArraySize = 1;
                td.Format = 87; // DXGI_FORMAT_B8G8R8A8_UNORM
                td.SampleCount = 1;
                td.Usage = 3; // D3D11_USAGE_STAGING
                td.CPUAccessFlags = 0x20000; // D3D11_CPU_ACCESS_READ

                // ID3D11Device::CreateTexture2D is vtable slot 5
                hr = GetVtbl<CreateTexture2DDel>(cap._device, 5)(cap._device, ref td, IntPtr.Zero, out cap._stagingTex);
                if (hr < 0) { error = "CreateTexture2D staging failed: 0x" + hr.ToString("X8"); cap.Dispose(); return null; }

                return cap;
            }
            catch (Exception ex)
            {
                error = ex.Message;
                cap.Dispose();
                return null;
            }
            finally
            {
                if (output1 != IntPtr.Zero) Marshal.Release(output1);
                if (output != IntPtr.Zero) Marshal.Release(output);
                if (adapter != IntPtr.Zero) Marshal.Release(adapter);
                if (dxgiDevice != IntPtr.Zero) Marshal.Release(dxgiDevice);
            }
        }

        public bool TryCaptureBgra(byte[] targetBuffer, out ulong captureTsUs)
        {
            captureTsUs = HighResClock.NowUs();
            if (_disposed || _duplication == IntPtr.Zero) return false;

            DXGI_OUTDUPL_FRAME_INFO info;
            IntPtr desktopRes = IntPtr.Zero;
            // IDXGIOutputDuplication::AcquireNextFrame is vtable slot 8
            int hr = GetVtbl<AcquireNextFrameDel>(_duplication, 8)(_duplication, 16, out info, out desktopRes);
            if (hr == unchecked((int)0x887A0027)) // DXGI_ERROR_WAIT_TIMEOUT
                return false;
            if (hr < 0)
                return false;

            IntPtr desktopTex = IntPtr.Zero;
            try
            {
                Guid iidTex = IID_ID3D11Texture2D;
                hr = Marshal.QueryInterface(desktopRes, ref iidTex, out desktopTex);
                if (hr < 0) return false;

                // ID3D11DeviceContext::CopyResource is vtable slot 47
                GetVtbl<CopyResourceDel>(_context, 47)(_context, _stagingTex, desktopTex);

                // ID3D11DeviceContext::Map is vtable slot 14, Unmap is slot 15
                D3D11_MAPPED_SUBRESOURCE mapped;
                hr = GetVtbl<MapDel>(_context, 14)(_context, _stagingTex, 0, 1 /* READ */, 0, out mapped);
                if (hr < 0) return false;
                try
                {
                    int rowBytes = _width * 4;
                    IntPtr srcPtr = mapped.pData;
                    for (int y = 0; y < _height; y++)
                    {
                        Marshal.Copy(srcPtr, targetBuffer, y * rowBytes, rowBytes);
                        srcPtr = new IntPtr(srcPtr.ToInt64() + mapped.RowPitch);
                    }
                    captureTsUs = HighResClock.NowUs();
                    return true;
                }
                finally
                {
                    GetVtbl<UnmapDel>(_context, 15)(_context, _stagingTex, 0);
                }
            }
            finally
            {
                if (desktopTex != IntPtr.Zero) Marshal.Release(desktopTex);
                if (desktopRes != IntPtr.Zero) Marshal.Release(desktopRes);
                // IDXGIOutputDuplication::ReleaseFrame is vtable slot 14
                GetVtbl<ReleaseFrameDel>(_duplication, 14)(_duplication);
            }
        }

        public void Dispose()
        {
            _disposed = true;
            if (_stagingTex != IntPtr.Zero) { Marshal.Release(_stagingTex); _stagingTex = IntPtr.Zero; }
            if (_duplication != IntPtr.Zero) { Marshal.Release(_duplication); _duplication = IntPtr.Zero; }
            if (_context != IntPtr.Zero) { Marshal.Release(_context); _context = IntPtr.Zero; }
            if (_device != IntPtr.Zero) { Marshal.Release(_device); _device = IntPtr.Zero; }
        }
    }

    /// <summary>
    /// Tier-3 fallback: Win32 GDI BitBlt screen capturer.
    /// </summary>
    public class GdiBitBltCapturer : IScreenCapturer
    {
        private readonly int _width;
        private readonly int _height;
        private readonly Bitmap _bmp;

        public string BackendName { get { return "GDI-BitBlt"; } }
        public int Width { get { return _width; } }
        public int Height { get { return _height; } }

        [DllImport("user32.dll")] private static extern int GetSystemMetrics(int nIndex);

        public GdiBitBltCapturer()
        {
            int w = GetSystemMetrics(0);
            int h = GetSystemMetrics(1);
            if (w <= 0 || h <= 0) { w = 1280; h = 720; }
            _width = w;
            _height = h;
            _bmp = new Bitmap(_width, _height, PixelFormat.Format32bppRgb);
        }

        private bool _errorLogged;

        public bool TryCaptureBgra(byte[] targetBuffer, out ulong captureTsUs)
        {
            captureTsUs = HighResClock.NowUs();
            try
            {
                using (Graphics g = Graphics.FromImage(_bmp))
                {
                    g.CopyFromScreen(0, 0, 0, 0, new Size(_width, _height), CopyPixelOperation.SourceCopy);
                }
                BitmapData bd = _bmp.LockBits(new Rectangle(0, 0, _width, _height), ImageLockMode.ReadOnly, PixelFormat.Format32bppRgb);
                try
                {
                    Marshal.Copy(bd.Scan0, targetBuffer, 0, _width * _height * 4);
                }
                finally { _bmp.UnlockBits(bd); }
                captureTsUs = HighResClock.NowUs();
                return true;
            }
            catch (Exception ex)
            {
                if (!_errorLogged)
                {
                    _errorLogged = true;
                    Console.WriteLine("[Capture] GDI-BitBlt transient error (desktop locked or non-interactive): " + ex.Message);
                }
                return false;
            }
        }

        public void Dispose()
        {
            try { _bmp.Dispose(); } catch { }
        }
    }

    /// <summary>
    /// Deterministic animated test pattern generator for headless CI / loopback verification.
    /// Mixes moving colored blocks (triggers Pass-1 lossy tiles) and sharp static text (triggers Pass-2 lossless refinement).
    /// </summary>
    public class SyntheticTestCapturer : IScreenCapturer
    {
        private readonly int _width;
        private readonly int _height;
        private readonly Bitmap _bmp;
        private readonly Font _font;
        private int _tick;

        public string BackendName { get { return "Synthetic-Pattern"; } }
        public int Width { get { return _width; } }
        public int Height { get { return _height; } }

        public SyntheticTestCapturer(int width, int height)
        {
            _width = width;
            _height = height;
            _bmp = new Bitmap(_width, _height, PixelFormat.Format32bppRgb);
            _font = new Font("Consolas", 14f, FontStyle.Regular);
        }

        public bool TryCaptureBgra(byte[] targetBuffer, out ulong captureTsUs)
        {
            _tick++;
            using (Graphics g = Graphics.FromImage(_bmp))
            {
                if (_tick == 1)
                {
                    g.Clear(Color.FromArgb(24, 28, 36));
                    // Sharp static text area (left side -> triggers Pass 2 lossless PNG refinement after 3 static ticks)
                    using (SolidBrush br = new SolidBrush(Color.FromArgb(220, 230, 240)))
                    {
                        g.DrawString("CopyToDesk Clean-Room Phase 1 Verification", _font, br, 32, 32);
                        g.DrawString("Static Text Region: Lossy -> Lossless PNG Refinement", _font, br, 32, 68);
                        g.DrawString("Protocol: CTD1 | TileSize: 128 | Cursor: Independent", _font, br, 32, 104);
                    }
                }

                // Moving region only on bottom strip (keeps top text static so Pass-2 refinement fires!)
                int stripY = _height - 160;
                using (SolidBrush bg = new SolidBrush(Color.FromArgb(30, 36, 48)))
                    g.FillRectangle(bg, 0, stripY, _width, 160);

                // Freeze motion briefly every 20 ticks to also test 0-byte idle & refinement transitions
                int boxX = ((_tick * 18) % (_width - 140)) + 20;
                using (SolidBrush accent = new SolidBrush(Color.FromArgb(64, 158, 255)))
                    g.FillRectangle(accent, boxX, stripY + 30, 100, 100);
                using (SolidBrush white = new SolidBrush(Color.White))
                    g.DrawString("F#" + _tick, _font, white, boxX + 12, stripY + 65);
            }

            BitmapData bd = _bmp.LockBits(new Rectangle(0, 0, _width, _height), ImageLockMode.ReadOnly, PixelFormat.Format32bppRgb);
            try
            {
                Marshal.Copy(bd.Scan0, targetBuffer, 0, _width * _height * 4);
            }
            finally { _bmp.UnlockBits(bd); }
            captureTsUs = HighResClock.NowUs();
            return true;
        }

        public void Dispose()
        {
            try { _font.Dispose(); } catch { }
            try { _bmp.Dispose(); } catch { }
        }
    }

    public static class CursorMonitor
    {
        [StructLayout(LayoutKind.Sequential)]
        private struct POINT { public int X; public int Y; }

        [StructLayout(LayoutKind.Sequential)]
        private struct CURSORINFO
        {
            public int cbSize;
            public int flags;
            public IntPtr hCursor;
            public POINT ptScreenPos;
        }

        [DllImport("user32.dll")]
        private static extern bool GetCursorInfo(ref CURSORINFO pci);

        public static bool TryPoll(out int x, out int y, out bool visible, out ulong shapeHash)
        {
            CURSORINFO ci = new CURSORINFO();
            ci.cbSize = Marshal.SizeOf(typeof(CURSORINFO));
            if (GetCursorInfo(ref ci))
            {
                x = ci.ptScreenPos.X;
                y = ci.ptScreenPos.Y;
                visible = (ci.flags & 1) != 0;
                shapeHash = (ulong)ci.hCursor.ToInt64();
                return true;
            }
            x = 0; y = 0; visible = false; shapeHash = 0;
            return false;
        }
    }
}
