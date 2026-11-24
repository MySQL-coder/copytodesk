using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace CopyToDesk.CodecProbe
{
    /// <summary>
    /// Isolated out-of-process GPU / DXGI / Media Foundation hardware codec capability probe.
    /// Mirrors ToDesk's codec_checker.exe crash-isolation architecture.
    /// </summary>
    public static class Program
    {
        private static readonly Guid IID_IDXGIFactory1 = new Guid("770aae78-f26f-4dba-a829-253c83d1b387");
        private static readonly Guid MFT_CATEGORY_VIDEO_ENCODER = new Guid("f79eac7d-e545-4387-bdee-d647d7bde42a");
        private static readonly Guid MFVideoFormat_H264 = new Guid("34363248-0000-0010-8000-00aa00389b71");
        private static readonly Guid MFVideoFormat_HEVC = new Guid("43564548-0000-0010-8000-00aa00389b71");

        [DllImport("dxgi.dll")]
        private static extern int CreateDXGIFactory1(ref Guid riid, out IntPtr ppFactory);

        [DllImport("d3d11.dll")]
        private static extern int D3D11CreateDevice(
            IntPtr pAdapter, int driverType, IntPtr software, uint flags,
            int[] pFeatureLevels, int featureLevels, uint sdkVersion,
            out IntPtr ppDevice, out int pFeatureLevel, out IntPtr ppImmediateContext);

        [DllImport("mfplat.dll")]
        private static extern int MFStartup(uint version, uint dwFlags);

        [DllImport("mfplat.dll")]
        private static extern int MFShutdown();

        [DllImport("mfplat.dll")]
        private static extern int MFTEnumEx(
            Guid guidCategory, uint flags, IntPtr pInputType, ref MFT_REGISTER_TYPE_INFO pOutputType,
            out IntPtr pppMFTActivate, out uint pnumMFTActivate);

        [StructLayout(LayoutKind.Sequential)]
        private struct MFT_REGISTER_TYPE_INFO
        {
            public Guid guidMajorType;
            public Guid guidSubtype;
        }

        private static readonly Guid MFMediaType_Video = new Guid("73646976-0000-0010-8000-00aa00389b71");

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct DXGI_ADAPTER_DESC1
        {
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)]
            public string Description;
            public uint VendorId;
            public uint DeviceId;
            public uint SubSysId;
            public uint Revision;
            public UIntPtr DedicatedVideoMemory;
            public UIntPtr DedicatedSystemMemory;
            public UIntPtr SharedSystemMemory;
            public uint LuidLow;
            public int LuidHigh;
            public uint Flags;
        }

        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int EnumAdapters1Del(IntPtr factory, uint index, out IntPtr adapter);
        [UnmanagedFunctionPointer(CallingConvention.StdCall)]
        private delegate int GetDesc1Del(IntPtr adapter, out DXGI_ADAPTER_DESC1 desc);

        private static T GetVtbl<T>(IntPtr comPtr, int index) where T : class
        {
            IntPtr vtbl = Marshal.ReadIntPtr(comPtr);
            IntPtr fnPtr = Marshal.ReadIntPtr(vtbl, index * IntPtr.Size);
            return Marshal.GetDelegateForFunctionPointer(fnPtr, typeof(T)) as T;
        }

        public static int Main(string[] args)
        {
            List<string> adapters = new List<string>();
            bool d3d11Hw = false;
            int featureLevel = 0;
            bool mfH264Hw = false;
            bool mfHevcHw = false;

            // 1. Enumerate DXGI Adapters
            IntPtr factory = IntPtr.Zero;
            try
            {
                Guid iid = IID_IDXGIFactory1;
                if (CreateDXGIFactory1(ref iid, out factory) >= 0 && factory != IntPtr.Zero)
                {
                    EnumAdapters1Del enumAdapters = GetVtbl<EnumAdapters1Del>(factory, 12);
                    for (uint i = 0; i < 8; i++)
                    {
                        IntPtr ad = IntPtr.Zero;
                        int hr = enumAdapters(factory, i, out ad);
                        if (hr < 0 || ad == IntPtr.Zero) break;
                        try
                        {
                            DXGI_ADAPTER_DESC1 desc;
                            if (GetVtbl<GetDesc1Del>(ad, 10)(ad, out desc) >= 0)
                            {
                                string vendor = VendorName(desc.VendorId);
                                ulong vramMb = desc.DedicatedVideoMemory.ToUInt64() / (1024UL * 1024UL);
                                adapters.Add(string.Format(
                                    "{{\"name\":\"{0}\",\"vendor\":\"{1}\",\"vendorId\":\"0x{2:X4}\",\"vramMB\":{3}}}",
                                    Escape(desc.Description), vendor, desc.VendorId, vramMb));
                            }
                        }
                        finally { Marshal.Release(ad); }
                    }
                }
            }
            catch { }
            finally { if (factory != IntPtr.Zero) Marshal.Release(factory); }

            // 2. Test D3D11 hardware device
            IntPtr dev = IntPtr.Zero, ctx = IntPtr.Zero;
            try
            {
                int[] levels = new int[] { 0xb100, 0xb000, 0xa100, 0xa000 };
                if (D3D11CreateDevice(IntPtr.Zero, 1, IntPtr.Zero, 0, levels, levels.Length, 7, out dev, out featureLevel, out ctx) >= 0)
                {
                    d3d11Hw = true;
                }
            }
            catch { }
            finally
            {
                if (ctx != IntPtr.Zero) Marshal.Release(ctx);
                if (dev != IntPtr.Zero) Marshal.Release(dev);
            }

            // 3. Probe Media Foundation Hardware H.264 / HEVC Encoders (MFT_ENUM_FLAG_HARDWARE = 0x4)
            try
            {
                if (MFStartup(0x00020070, 0) >= 0)
                {
                    try
                    {
                        mfH264Hw = CountHwMft(MFVideoFormat_H264) > 0;
                        mfHevcHw = CountHwMft(MFVideoFormat_HEVC) > 0;
                    }
                    finally { MFShutdown(); }
                }
            }
            catch { }

            StringBuilder sb = new StringBuilder();
            sb.Append("{");
            sb.AppendFormat("\"d3d11Hardware\":{0},", d3d11Hw ? "true" : "false");
            sb.AppendFormat("\"d3d11FeatureLevel\":\"0x{0:X4}\",", featureLevel);
            sb.AppendFormat("\"mfH264HardwareEncoder\":{0},", mfH264Hw ? "true" : "false");
            sb.AppendFormat("\"mfHevcHardwareEncoder\":{0},", mfHevcHw ? "true" : "false");
            sb.Append("\"adapters\":[");
            sb.Append(string.Join(",", adapters.ToArray()));
            sb.Append("]}");

            Console.WriteLine(sb.ToString());
            return 0;
        }

        private static uint CountHwMft(Guid subtype)
        {
            MFT_REGISTER_TYPE_INFO outType = new MFT_REGISTER_TYPE_INFO();
            outType.guidMajorType = MFMediaType_Video;
            outType.guidSubtype = subtype;
            IntPtr arrPtr = IntPtr.Zero;
            uint count = 0;
            // MFT_ENUM_FLAG_HARDWARE (0x4) | MFT_ENUM_FLAG_SORTANDFILTER (0x40)
            int hr = MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, 0x44, IntPtr.Zero, ref outType, out arrPtr, out count);
            if (hr >= 0 && arrPtr != IntPtr.Zero)
            {
                for (uint i = 0; i < count; i++)
                {
                    IntPtr item = Marshal.ReadIntPtr(arrPtr, (int)i * IntPtr.Size);
                    if (item != IntPtr.Zero) Marshal.Release(item);
                }
                Marshal.FreeCoTaskMem(arrPtr);
                return count;
            }
            return 0;
        }

        private static string VendorName(uint id)
        {
            if (id == 0x10DE) return "NVIDIA";
            if (id == 0x1002 || id == 0x1022) return "AMD";
            if (id == 0x8086) return "Intel";
            if (id == 0x1414) return "Microsoft-Basic";
            return "Other";
        }

        private static string Escape(string s)
        {
            if (s == null) return "";
            return s.Replace("\\", "\\\\").Replace("\"", "\\\"");
        }
    }
}
