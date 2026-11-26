//! Zero-GPU pure-CPU 128x128 dirty-tile compound encoder (v0.6.0 P3):
//! - Zero-allocation 2x2 subpixel box/area RGBA downsampler for crisp 4K/Retina scaling
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
        let mut tiles = Vec::new();
        let mut refined_this_frame = 0usize;
        let stride = (self.width * 4) as usize;

        for r in 0..self.rows {
            let ty = (r as u32) * self.tile_size;
            let th = self.tile_size.min(self.height - ty);
            for c in 0..self.cols {
                let tx = (c as u32) * self.tile_size;
                let tw = self.tile_size.min(self.width - tx);
                let idx = r * self.cols + c;
                let h = fnv1a_tile(rgba, stride, tx as usize, ty as usize, tw as usize, th as usize);

                if force_key || h != self.prev_hashes[idx] {
                    self.prev_hashes[idx] = h;
                    self.needs_refinement[idx] = true;
                    self.static_ticks[idx] = 0;
                    if let Ok(data) = encode_tile_jpeg(rgba, stride, tx, ty, tw, th, self.jpeg_quality) {
                        tiles.push(EncodedTile {
                            x: tx as u16,
                            y: ty as u16,
                            w: tw as u16,
                            h: th as u16,
                            encoding: TileEncoding::JpegLossy,
                            flags: 0,
                            data,
                        });
                    }
                } else if self.needs_refinement[idx] {
                    self.static_ticks[idx] = self.static_ticks[idx].saturating_add(1);
                    if self.static_ticks[idx] >= self.refine_ticks
                        && refined_this_frame < MAX_REFINEMENTS_PER_FRAME
                    {
                        self.needs_refinement[idx] = false;
                        if let Ok(data) = encode_tile_png(rgba, stride, tx, ty, tw, th) {
                            tiles.push(EncodedTile {
                                x: tx as u16,
                                y: ty as u16,
                                w: tw as u16,
                                h: th as u16,
                                encoding: TileEncoding::PngLossless,
                                flags: 1,
                                data,
                            });
                            refined_this_frame += 1;
                        }
                    }
                }
            }
        }

        if refined_this_frame > 0 {
            flags |= frame_flags::LOSSLESS_REFINEMENT;
        }
        let pkt = VideoTileFramePacket {
            capture_ts_us,
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

/// Zero-allocation (reuses `dst` capacity) 2x2 subpixel box/area downsampler.
/// Eliminates 33 MB/frame heap clones on 4K displays while keeping thin UI/font strokes crisp.
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
    let src_stride = sw * 4;

    for dy in 0..dh {
        let sy0 = (dy * sh) / dh;
        let sy1 = (((dy * 2 + 1) * sh) / (dh * 2)).min(sh - 1);
        let row0 = sy0 * src_stride;
        let row1 = sy1 * src_stride;
        let dst_row = dy * dw * 4;

        for dx in 0..dw {
            let sx0 = ((dx * sw) / dw) * 4;
            let sx1 = ((((dx * 2 + 1) * sw) / (dw * 2)).min(sw - 1)) * 4;

            let p00 = row0 + sx0;
            let p10 = row0 + sx1;
            let p01 = row1 + sx0;
            let p11 = row1 + sx1;
            let out_p = dst_row + dx * 4;

            dst[out_p] = ((src[p00] as u16 + src[p10] as u16 + src[p01] as u16 + src[p11] as u16 + 2) >> 2) as u8;
            dst[out_p + 1] = ((src[p00 + 1] as u16 + src[p10 + 1] as u16 + src[p01 + 1] as u16 + src[p11 + 1] as u16 + 2) >> 2) as u8;
            dst[out_p + 2] = ((src[p00 + 2] as u16 + src[p10 + 2] as u16 + src[p01 + 2] as u16 + src[p11 + 2] as u16 + 2) >> 2) as u8;
            dst[out_p + 3] = 255;
        }
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

fn encode_tile_jpeg(rgba: &[u8], stride: usize, tx: u32, ty: u32, tw: u32, th: u32, q: u8) -> Result<Vec<u8>, String> {
    let rgb = extract_tile_rgb(rgba, stride, tx, ty, tw, th);
    let mut out = Cursor::new(Vec::with_capacity(4096));
    JpegEncoder::new_with_quality(&mut out, q)
        .write_image(&rgb, tw, th, ColorType::Rgb8.into())
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

fn encode_tile_png(rgba: &[u8], stride: usize, tx: u32, ty: u32, tw: u32, th: u32) -> Result<Vec<u8>, String> {
    let rgb = extract_tile_rgb(rgba, stride, tx, ty, tw, th);
    let mut out = Cursor::new(Vec::with_capacity(4096));
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilterType::Adaptive)
        .write_image(&rgb, tw, th, ColorType::Rgb8.into())
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

fn fnv1a_tile(rgba: &[u8], stride: usize, tx: usize, ty: usize, tw: usize, th: usize) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for y in 0..th {
        let row = (ty + y) * stride + tx * 4;
        for x in 0..tw {
            let p = row + x * 4;
            let px = (rgba[p] as u64) | ((rgba[p + 1] as u64) << 8) | ((rgba[p + 2] as u64) << 16);
            h ^= px;
            h = h.wrapping_mul(1099511628211);
        }
    }
    if h == 0 { 1 } else { h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crisp_downsampler_and_paced_png_refinement() {
        let src = vec![120u8; 640 * 480 * 4];
        let mut dst = Vec::new();
        downsample_rgba_crisp(&src, 640, 480, 320, 240, &mut dst);
        assert_eq!(dst.len(), 320 * 240 * 4);
        assert_eq!(dst[0], 120);

        // 8x4 grid = 32 tiles (1024x512); static refinement must cap at MAX_REFINEMENTS_PER_FRAME (16)
        let frame = vec![90u8; 1024 * 512 * 4];
        let mut enc = CompoundTileEncoder::new(1024, 512, 128, 70, 2);
        let (p1, _) = enc.encode_rgba(&frame, 1, true);
        assert_eq!(p1.tiles.len(), 32);
        let (p2, _) = enc.encode_rgba(&frame, 2, false);
        assert_eq!(p2.tiles.len(), 0);
        let (p3, f3) = enc.encode_rgba(&frame, 3, false);
        assert_eq!(p3.tiles.len(), MAX_REFINEMENTS_PER_FRAME);
        assert_ne!(f3 & frame_flags::LOSSLESS_REFINEMENT, 0);
        let (p4, _) = enc.encode_rgba(&frame, 4, false);
        assert_eq!(p4.tiles.len(), 32 - MAX_REFINEMENTS_PER_FRAME);
    }
}
