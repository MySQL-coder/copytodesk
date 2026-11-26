//! Zero-GPU pure-CPU multi-core 128x128 dirty-tile compound encoder (v0.8.0 P5):
//! - Multi-core parallel `std::thread::scope` dirty-tile hashing, JPEG/PNG encoding, and 2x2 subpixel RGBA downsampling
//! - Pass 1 (motion): fast lossy JPEG -> Pass 2 (static >= 3 ticks): paced fast-zlib PNG refinement (max 16 tiles/frame)

use ctd_protocol::{frame_flags, EncodedTile, TileEncoding, VideoTileFramePacket};
use image::{
    codecs::jpeg::JpegEncoder,
    codecs::png::{CompressionType, FilterType as PngFilterType, PngEncoder},
    ColorType, ImageEncoder,
};
use std::io::Cursor;
use std::time::Instant;

const MAX_REFINEMENTS_PER_FRAME: usize = 16;

#[derive(Clone, Copy)]
struct TileEncodeJob {
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    is_png: bool,
}

fn worker_budget() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

pub struct CompoundTileEncoder {
    width: u32,
    height: u32,
    tile_size: u32,
    cols: usize,
    rows: usize,
    jpeg_quality: u8,
    refine_ticks: u32,
    prev_hashes: Vec<u64>,
    needs_refinement: Vec<bool>,
    static_ticks: Vec<u32>,
    frame_id: u32,
}

impl CompoundTileEncoder {
    pub fn new(width: u32, height: u32, tile_size: u32, jpeg_quality: u8, refine_ticks: u32) -> Self {
        let cols = width.div_ceil(tile_size) as usize;
        let rows = height.div_ceil(tile_size) as usize;
        let n = cols * rows;
        Self {
            width,
            height,
            tile_size,
            cols,
            rows,
            jpeg_quality: jpeg_quality.clamp(10, 95),
            refine_ticks,
            prev_hashes: vec![0; n],
            needs_refinement: vec![false; n],
            static_ticks: vec![0; n],
            frame_id: 0,
        }
    }

    pub fn force_keyframe(&mut self) {
        self.prev_hashes.fill(0);
        self.needs_refinement.fill(false);
        self.static_ticks.fill(0);
    }

    /// Expects `rgba` slice of length `width * height * 4`.
    pub fn encode_rgba(
        &mut self,
        rgba: &[u8],
        capture_ts_us: u64,
        force_key: bool,
    ) -> (VideoTileFramePacket, u8) {
        let start = Instant::now();
        let mut flags = frame_flags::NONE;
        if force_key {
            self.force_keyframe();
            flags |= frame_flags::KEY_FRAME;
        }
        self.frame_id = self.frame_id.wrapping_add(1);

        let n = self.rows * self.cols;
        let stride = (self.width * 4) as usize;
        let cols = self.cols;
        let tile_size = self.tile_size;
        let width = self.width;
        let height = self.height;

        // Step 1: Compute per-tile FNV-1a hashes (parallel across cores on full-HD/4K grids)
        let mut hashes = vec![0u64; n];
        let workers = worker_budget();
        if n >= 24 && workers > 1 {
            let chunk_len = n.div_ceil(workers);
            std::thread::scope(|s| {
                for (chunk_idx, out_slice) in hashes.chunks_mut(chunk_len).enumerate() {
                    let base_idx = chunk_idx * chunk_len;
                    s.spawn(move || {
                        for (local_i, slot) in out_slice.iter_mut().enumerate() {
                            let idx = base_idx + local_i;
                            let r = idx / cols;
                            let c = idx % cols;
                            let tx = (c as u32) * tile_size;
                            let ty = (r as u32) * tile_size;
                            let tw = tile_size.min(width - tx);
                            let th = tile_size.min(height - ty);
                            *slot = fnv1a_tile(
                                rgba,
                                stride,
                                tx as usize,
                                ty as usize,
                                tw as usize,
                                th as usize,
                            );
                        }
                    });
                }
            });
        } else {
            for (idx, slot) in hashes.iter_mut().enumerate() {
                let r = idx / cols;
                let c = idx % cols;
                let tx = (c as u32) * tile_size;
                let ty = (r as u32) * tile_size;
                let tw = tile_size.min(width - tx);
                let th = tile_size.min(height - ty);
                *slot = fnv1a_tile(
                    rgba,
                    stride,
                    tx as usize,
                    ty as usize,
                    tw as usize,
                    th as usize,
                );
            }
        }

        // Step 2: Deterministic row-major dirty & lossless-refinement state update
        let mut jobs = Vec::new();
        let mut refined_this_frame = 0usize;
        for (idx, &h) in hashes.iter().enumerate() {
            let r = idx / cols;
            let c = idx % cols;
            let tx = (c as u32) * tile_size;
            let ty = (r as u32) * tile_size;
            let tw = tile_size.min(width - tx);
            let th = tile_size.min(height - ty);

            if force_key || h != self.prev_hashes[idx] {
                self.prev_hashes[idx] = h;
                self.needs_refinement[idx] = true;
                self.static_ticks[idx] = 0;
                jobs.push(TileEncodeJob {
                    tx,
                    ty,
                    tw,
                    th,
                    is_png: false,
                });
            } else if self.needs_refinement[idx] {
                self.static_ticks[idx] = self.static_ticks[idx].saturating_add(1);
                if self.static_ticks[idx] >= self.refine_ticks
                    && refined_this_frame < MAX_REFINEMENTS_PER_FRAME
                {
                    self.needs_refinement[idx] = false;
                    refined_this_frame += 1;
                    jobs.push(TileEncodeJob {
                        tx,
                        ty,
                        tw,
                        th,
                        is_png: true,
                    });
                }
            }
        }

        // Step 3: Multi-core parallel JPEG / PNG encoding (inline when <= 2 dirty tiles to avoid thread wake overhead)
        let jpeg_q = self.jpeg_quality;
        let encode_one = |job: &TileEncodeJob| -> Option<EncodedTile> {
            if job.is_png {
                encode_tile_png(rgba, stride, job.tx, job.ty, job.tw, job.th)
                    .ok()
                    .map(|data| EncodedTile {
                        x: job.tx as u16,
                        y: job.ty as u16,
                        w: job.tw as u16,
                        h: job.th as u16,
                        encoding: TileEncoding::PngLossless,
                        flags: 1,
                        data,
                    })
            } else {
                encode_tile_jpeg(rgba, stride, job.tx, job.ty, job.tw, job.th, jpeg_q)
                    .ok()
                    .map(|data| EncodedTile {
                        x: job.tx as u16,
                        y: job.ty as u16,
                        w: job.tw as u16,
                        h: job.th as u16,
                        encoding: TileEncoding::JpegLossy,
                        flags: 0,
                        data,
                    })
            }
        };

        let tiles: Vec<EncodedTile> = if jobs.len() <= 2 || workers <= 1 {
            jobs.iter().filter_map(encode_one).collect()
        } else {
            let chunk_len = jobs.len().div_ceil(workers);
            let mut groups: Vec<Vec<EncodedTile>> = Vec::with_capacity(workers);
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                for job_slice in jobs.chunks(chunk_len) {
                    handles.push(s.spawn(move || {
                        job_slice.iter().filter_map(encode_one).collect::<Vec<_>>()
                    }));
                }
                for h in handles {
                    if let Ok(part) = h.join() {
                        groups.push(part);
                    }
                }
            });
            groups.into_iter().flatten().collect()
        };

        if refined_this_frame > 0 {
            flags |= frame_flags::LOSSLESS_REFINEMENT;
        }
        let pkt = VideoTileFramePacket {
            capture_ts_us: capture_ts_us,
            frame_id: self.frame_id,
            screen_w: self.width as u16,
            screen_h: self.height as u16,
            tile_size: self.tile_size as u16,
            encode_us: start.elapsed().as_micros() as u32,
            tiles,
        };
        (pkt, flags)
    }
}

fn downsample_rows(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    start_dy: usize,
    dst_slice: &mut [u8],
) {
    let src_stride = sw * 4;
    let dst_stride = dw * 4;
    let rows = dst_slice.len() / dst_stride;
    for local_y in 0..rows {
        let dy = start_dy + local_y;
        let sy0 = (dy * sh) / dh;
        let sy1 = (((dy * 2 + 1) * sh) / (dh * 2)).min(sh - 1);
        let row0 = sy0 * src_stride;
        let row1 = sy1 * src_stride;
        let dst_row = local_y * dst_stride;

        for dx in 0..dw {
            let sx0 = ((dx * sw) / dw) * 4;
            let sx1 = ((((dx * 2 + 1) * sw) / (dw * 2)).min(sw - 1)) * 4;

            let p00 = row0 + sx0;
            let p10 = row0 + sx1;
            let p01 = row1 + sx0;
            let p11 = row1 + sx1;
            let out_p = dst_row + dx * 4;

            dst_slice[out_p] =
                ((src[p00] as u16 + src[p10] as u16 + src[p01] as u16 + src[p11] as u16 + 2) >> 2)
                    as u8;
            dst_slice[out_p + 1] = ((src[p00 + 1] as u16
                + src[p10 + 1] as u16
                + src[p01 + 1] as u16
                + src[p11 + 1] as u16
                + 2)
                >> 2) as u8;
            dst_slice[out_p + 2] = ((src[p00 + 2] as u16
                + src[p10 + 2] as u16
                + src[p01 + 2] as u16
                + src[p11 + 2] as u16
                + 2)
                >> 2) as u8;
            dst_slice[out_p + 3] = 255;
        }
    }
}

/// Zero-allocation (reuses `dst` capacity) multi-core 2x2 subpixel box/area downsampler.
pub fn downsample_rgba_crisp(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    dst: &mut Vec<u8>,
) {
    let dw = dst_w.max(1) as usize;
    let dh = dst_h.max(1) as usize;
    let sw = src_w.max(1) as usize;
    let sh = src_h.max(1) as usize;
    let needed = dw * dh * 4;
    if src.len() < sw * sh * 4 {
        dst.clear();
        dst.resize(needed, 0);
        return;
    }
    dst.resize(needed, 255);

    let workers = worker_budget();
    if dh >= 256 && workers > 1 {
        let rows_per_chunk = dh.div_ceil(workers);
        let bytes_per_chunk = rows_per_chunk * dw * 4;
        std::thread::scope(|s| {
            for (chunk_idx, out_chunk) in dst.chunks_mut(bytes_per_chunk).enumerate() {
                let start_dy = chunk_idx * rows_per_chunk;
                s.spawn(move || {
                    downsample_rows(src, sw, sh, dw, dh, start_dy, out_chunk);
                });
            }
        });
    } else {
        downsample_rows(src, sw, sh, dw, dh, 0, dst);
    }
}

pub fn encode_full_screenshot(
    rgba: &[u8],
    width: u32,
    height: u32,
    max_width: u32,
    quality: u8,
    as_png: bool,
) -> Result<(u16, u16, Vec<u8>), String> {
    if rgba.len() < (width * height * 4) as usize {
        return Err("invalid rgba dimensions".to_string());
    }
    let mut scaled = Vec::new();
    let (out_w, out_h, target_rgba): (u32, u32, &[u8]) = if max_width > 0 && width > max_width {
        let nh = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;
        downsample_rgba_crisp(rgba, width, height, max_width, nh, &mut scaled);
        (max_width, nh, &scaled)
    } else {
        (width, height, rgba)
    };

    let mut out = Cursor::new(Vec::new());
    if as_png {
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilterType::Adaptive)
            .write_image(target_rgba, out_w, out_h, ColorType::Rgba8.into())
            .map_err(|e| e.to_string())?;
    } else {
        let rgb = extract_tile_rgb(target_rgba, (out_w * 4) as usize, 0, 0, out_w, out_h);
        JpegEncoder::new_with_quality(&mut out, quality.clamp(10, 95))
            .write_image(&rgb, out_w, out_h, ColorType::Rgb8.into())
            .map_err(|e| e.to_string())?;
    }
    Ok((out_w as u16, out_h as u16, out.into_inner()))
}

fn extract_tile_rgb(rgba: &[u8], stride: usize, tx: u32, ty: u32, tw: u32, th: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((tw * th * 3) as usize);
    for y in 0..th as usize {
        let row = (ty as usize + y) * stride + (tx as usize) * 4;
        for x in 0..tw as usize {
            let p = row + x * 4;
            rgb.push(rgba[p]);
            rgb.push(rgba[p + 1]);
            rgb.push(rgba[p + 2]);
        }
    }
    rgb
}

fn encode_tile_jpeg(
    rgba: &[u8],
    stride: usize,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
    q: u8,
) -> Result<Vec<u8>, String> {
    let rgb = extract_tile_rgb(rgba, stride, tx, ty, tw, th);
    let mut out = Cursor::new(Vec::with_capacity(4096));
    JpegEncoder::new_with_quality(&mut out, q)
        .write_image(&rgb, tw, th, ColorType::Rgb8.into())
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

fn encode_tile_png(
    rgba: &[u8],
    stride: usize,
    tx: u32,
    ty: u32,
    tw: u32,
    th: u32,
) -> Result<Vec<u8>, String> {
    let rgb = extract_tile_rgb(rgba, stride, tx, ty, tw, th);
    let mut out = Cursor::new(Vec::with_capacity(4096));
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilterType::Adaptive)
        .write_image(&rgb, tw, th, ColorType::Rgb8.into())
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

fn fnv1a_tile(rgba: &[u8], stride: usize, tx: usize, ty: usize, tw: usize, th: usize) -> u64 {
    let mut h: u64 = 14695981039346656037;
    let row_bytes = tw * 4;
    for y in 0..th {
        let start = (ty + y) * stride + tx * 4;
        let line = &rgba[start..start + row_bytes];
        for px_bytes in line.chunks_exact(4) {
            let px = (px_bytes[0] as u64)
                | ((px_bytes[1] as u64) << 8)
                | ((px_bytes[2] as u64) << 16);
            h ^= px;
            h = h.wrapping_mul(1099511628211);
        }
    }
    if h == 0 {
        1
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicore_crisp_downsampler_and_paced_png_refinement() {
        // Test dh >= 256 multi-core downsampler path
        let src = vec![120u8; 1280 * 720 * 4];
        let mut dst = Vec::new();
        downsample_rgba_crisp(&src, 1280, 720, 640, 360, &mut dst);
        assert_eq!(dst.len(), 640 * 360 * 4);
        assert_eq!(dst[0], 120);
        assert_eq!(dst[dst.len() - 4], 120);

        // 8x4 grid = 32 tiles (1024x512); exercises both parallel hashing (n=32>=24) and parallel encoding (jobs=32>2)
        let frame = vec![90u8; 1024 * 512 * 4];
        let mut enc = CompoundTileEncoder::new(1024, 512, 128, 70, 2);
        let (p1, _) = enc.encode_rgba(&frame, 1, true);
        assert_eq!(p1.tiles.len(), 32);
        assert_eq!(p1.tiles[0].x, 0);
        assert_eq!(p1.tiles[0].y, 0);
        assert_eq!(p1.tiles[31].x, 896);
        assert_eq!(p1.tiles[31].y, 384);

        let (p2, _) = enc.encode_rgba(&frame, 2, false);
        assert_eq!(p2.tiles.len(), 0);
        let (p3, f3) = enc.encode_rgba(&frame, 3, false);
        assert_eq!(p3.tiles.len(), MAX_REFINEMENTS_PER_FRAME);
        assert_ne!(f3 & frame_flags::LOSSLESS_REFINEMENT, 0);
        let (p4, _) = enc.encode_rgba(&frame, 4, false);
        assert_eq!(p4.tiles.len(), 32 - MAX_REFINEMENTS_PER_FRAME);
    }
}
