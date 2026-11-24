using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;

namespace CopyToDesk.Core
{
    public class CompoundTileEncoder : IDisposable
    {
        private readonly int _width;
        private readonly int _height;
        private readonly int _tileSize;
        private readonly int _cols;
        private readonly int _rows;
        private readonly int _jpegQuality;
        private readonly int _refineAfterStaticTicks;

        private readonly ulong[] _prevHashes;
        private readonly bool[] _needsRefinement;
        private readonly int[] _staticTicks;
        private readonly ImageCodecInfo _jpegCodec;
        private readonly EncoderParameters _jpegParams;
        private readonly Bitmap _tileBmp;
        private uint _frameId;

        public int TileSize { get { return _tileSize; } }
        public long TotalLossyTiles { get; private set; }
        public long TotalLosslessTiles { get; private set; }

        public CompoundTileEncoder(int width, int height, int tileSize, int jpegQuality, int refineAfterStaticTicks)
        {
            _width = width;
            _height = height;
            _tileSize = tileSize;
            _jpegQuality = jpegQuality;
            _refineAfterStaticTicks = refineAfterStaticTicks;
            _cols = (width + tileSize - 1) / tileSize;
            _rows = (height + tileSize - 1) / tileSize;

            int n = _cols * _rows;
            _prevHashes = new ulong[n];
            _needsRefinement = new bool[n];
            _staticTicks = new int[n];
            _jpegCodec = FindCodec("image/jpeg");
            _jpegParams = new EncoderParameters(1);
            _jpegParams.Param[0] = new EncoderParameter(Encoder.Quality, (long)_jpegQuality);
            _tileBmp = new Bitmap(tileSize, tileSize, PixelFormat.Format32bppRgb);
        }

        public void ForceKeyFrame()
        {
            for (int i = 0; i < _prevHashes.Length; i++)
            {
                _prevHashes[i] = 0;
                _needsRefinement[i] = false;
                _staticTicks[i] = 0;
            }
        }

        public VideoTileFramePacket EncodeFrame(byte[] bgra, ulong captureTsUs, bool forceKeyFrame, out FrameFlags outFlags)
        {
            Stopwatch sw = Stopwatch.StartNew();
            outFlags = FrameFlags.None;
            if (forceKeyFrame)
            {
                ForceKeyFrame();
                outFlags |= FrameFlags.KeyFrame;
            }

            VideoTileFramePacket pkt = new VideoTileFramePacket();
            pkt.CaptureTsUs = captureTsUs;
            pkt.FrameId = ++_frameId;
            pkt.ScreenW = (ushort)_width;
            pkt.ScreenH = (ushort)_height;
            pkt.TileSize = (ushort)_tileSize;

            int stride = _width * 4;
            bool hasRefinement = false;

            for (int r = 0; r < _rows; r++)
            {
                int ty = r * _tileSize;
                int th = Math.Min(_tileSize, _height - ty);
                for (int c = 0; c < _cols; c++)
                {
                    int tx = c * _tileSize;
                    int tw = Math.Min(_tileSize, _width - tx);
                    int idx = r * _cols + c;

                    ulong h = HashTile(bgra, stride, tx, ty, tw, th);
                    if (forceKeyFrame || h != _prevHashes[idx])
                    {
                        _prevHashes[idx] = h;
                        _needsRefinement[idx] = true;
                        _staticTicks[idx] = 0;

                        byte[] jpg = EncodeTile(bgra, stride, tx, ty, tw, th, TileEncoding.JpegLossy);
                        EncodedTile et = new EncodedTile();
                        et.X = (ushort)tx;
                        et.Y = (ushort)ty;
                        et.W = (ushort)tw;
                        et.H = (ushort)th;
                        et.Encoding = TileEncoding.JpegLossy;
                        et.Flags = 0;
                        et.Data = jpg;
                        pkt.Tiles.Add(et);
                        TotalLossyTiles++;
                    }
                    else if (_needsRefinement[idx])
                    {
                        _staticTicks[idx]++;
                        if (_staticTicks[idx] >= _refineAfterStaticTicks)
                        {
                            _needsRefinement[idx] = false;
                            byte[] png = EncodeTile(bgra, stride, tx, ty, tw, th, TileEncoding.PngLossless);
                            EncodedTile et = new EncodedTile();
                            et.X = (ushort)tx;
                            et.Y = (ushort)ty;
                            et.W = (ushort)tw;
                            et.H = (ushort)th;
                            et.Encoding = TileEncoding.PngLossless;
                            et.Flags = 1; // RefinementPass
                            et.Data = png;
                            pkt.Tiles.Add(et);
                            TotalLosslessTiles++;
                            hasRefinement = true;
                        }
                    }
                }
            }

            if (hasRefinement) outFlags |= FrameFlags.LosslessRefinement;
            sw.Stop();
            pkt.EncodeUs = (uint)(sw.ElapsedTicks * 1000000L / Stopwatch.Frequency);
            return pkt;
        }

        public static byte[] EncodeFullScreenshot(byte[] bgra, int width, int height, int maxWidth, int quality, bool png, out int outW, out int outH)
        {
            outW = width;
            outH = height;
            if (maxWidth > 0 && width > maxWidth)
            {
                outW = maxWidth;
                outH = Math.Max(1, (int)((long)height * maxWidth / width));
            }
            using (Bitmap src = new Bitmap(width, height, PixelFormat.Format32bppRgb))
            {
                BitmapData bd = src.LockBits(new Rectangle(0, 0, width, height), ImageLockMode.WriteOnly, PixelFormat.Format32bppRgb);
                try
                {
                    Marshal.Copy(bgra, 0, bd.Scan0, width * height * 4);
                }
                finally { src.UnlockBits(bd); }

                Bitmap target = src;
                Bitmap resized = null;
                if (outW != width || outH != height)
                {
                    resized = new Bitmap(outW, outH, PixelFormat.Format24bppRgb);
                    using (Graphics g = Graphics.FromImage(resized))
                    {
                        g.InterpolationMode = System.Drawing.Drawing2D.InterpolationMode.Bilinear;
                        g.DrawImage(src, 0, 0, outW, outH);
                    }
                    target = resized;
                }
                try
                {
                    using (MemoryStream ms = new MemoryStream())
                    {
                        if (png)
                        {
                            target.Save(ms, ImageFormat.Png);
                        }
                        else
                        {
                            ImageCodecInfo jpg = FindCodec("image/jpeg");
                            using (EncoderParameters ep = new EncoderParameters(1))
                            {
                                ep.Param[0] = new EncoderParameter(Encoder.Quality, (long)Math.Max(10, Math.Min(95, quality)));
                                target.Save(ms, jpg, ep);
                            }
                        }
                        return ms.ToArray();
                    }
                }
                finally
                {
                    if (resized != null) resized.Dispose();
                }
            }
        }

        private byte[] EncodeTile(byte[] bgra, int stride, int tx, int ty, int tw, int th, TileEncoding enc)
        {
            Bitmap bmp = (tw == _tileSize && th == _tileSize)
                ? _tileBmp
                : new Bitmap(tw, th, PixelFormat.Format32bppRgb);
            bool disposeBmp = (bmp != _tileBmp);
            try
            {
                BitmapData bd = bmp.LockBits(new Rectangle(0, 0, tw, th), ImageLockMode.WriteOnly, PixelFormat.Format32bppRgb);
                try
                {
                    int rowBytes = tw * 4;
                    IntPtr dstPtr = bd.Scan0;
                    for (int y = 0; y < th; y++)
                    {
                        int srcOff = (ty + y) * stride + tx * 4;
                        Marshal.Copy(bgra, srcOff, dstPtr, rowBytes);
                        dstPtr = new IntPtr(dstPtr.ToInt64() + bd.Stride);
                    }
                }
                finally { bmp.UnlockBits(bd); }

                using (MemoryStream ms = new MemoryStream(4096))
                {
                    if (enc == TileEncoding.JpegLossy)
                        bmp.Save(ms, _jpegCodec, _jpegParams);
                    else
                        bmp.Save(ms, ImageFormat.Png);
                    return ms.ToArray();
                }
            }
            finally
            {
                if (disposeBmp) bmp.Dispose();
            }
        }

        private static ulong HashTile(byte[] bgra, int stride, int tx, int ty, int tw, int th)
        {
            ulong hash = 14695981039346656037UL;
            for (int y = 0; y < th; y++)
            {
                int row = (ty + y) * stride + tx * 4;
                int end = row + tw * 4;
                for (int i = row; i < end; i += 4)
                {
                    uint px = (uint)(bgra[i] | (bgra[i + 1] << 8) | (bgra[i + 2] << 16));
                    hash ^= px;
                    hash *= 1099511628211UL;
                }
            }
            return hash == 0 ? 1UL : hash;
        }

        private static ImageCodecInfo FindCodec(string mime)
        {
            ImageCodecInfo[] codecs = ImageCodecInfo.GetImageEncoders();
            for (int i = 0; i < codecs.Length; i++)
            {
                if (string.Equals(codecs[i].MimeType, mime, StringComparison.OrdinalIgnoreCase))
                    return codecs[i];
            }
            throw new InvalidOperationException("Codec not found: " + mime);
        }

        public void Dispose()
        {
            try { _tileBmp.Dispose(); } catch { }
            try { _jpegParams.Dispose(); } catch { }
        }
    }
}
