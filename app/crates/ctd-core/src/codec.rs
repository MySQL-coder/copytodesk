//! Zero-GPU pure-CPU 128x128 dirty-tile compound encoder:
//! Pass 1 (motion): fast lossy JPEG -> Pass 2 (static >= 3 ticks): lossless PNG refinement.

use ctd_protocol::{frame_flags, EncodedTile, TileEncoding, VideoTileFramePacket};
use image::{codecs::jpeg::JpegEncoder, codecs::png::PngEncoder, ColorType, ImageEncoder, RgbaImage};
use std::io::Cursor;
use std::time::Instant;

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
        let mut has_refinement = false;
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
                    self.static_ticks[idx] += 1;
                    if self.static_ticks[idx] >= self.refine_ticks {
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
                            has_refinement = true;
                        }
                    }
                }
            }
        }

        if has_refinement {
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

pub fn encode_full_screenshot(
    rgba: &[u8],
    width: u32,
    height: u32,
    max_width: u32,
    quality: u8,
    as_png: bool,
) -> Result<(u16, u16, Vec<u8>), String> {
    let img = RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "invalid rgba dimensions".to_string())?;
    let (out_w, out_h, resized) = if max_width > 0 && width > max_width {
        let nh = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;
        let r = image::imageops::resize(&img, max_width, nh, image::imageops::FilterType::Triangle);
        (max_width, nh, r)
    } else {
        (width, height, img)
    };

    let mut out = Cursor::new(Vec::new());
    if as_png {
        PngEncoder::new(&mut out)
            .write_image(resized.as_raw(), out_w, out_h, ColorType::Rgba8.into())
            .map_err(|e| e.to_string())?;
    } else {
        let rgb = image::DynamicImage::ImageRgba8(resized).to_rgb8();
        JpegEncoder::new_with_quality(&mut out, quality.clamp(10, 95))
            .write_image(rgb.as_raw(), out_w, out_h, ColorType::Rgb8.into())
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
    PngEncoder::new(&mut out)
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
