//! Cross-platform native OS system clipboard integration (v0.4.0 P1):
//! - Windows: Pure Win32 FFI (`OpenClipboard` / `CF_UNICODETEXT` / `GlobalAlloc`), zero extra crates
//! - macOS: Native `/usr/bin/pbpaste` & `/usr/bin/pbcopy` with UTF-8 locale
//! - Linux: Wayland (`wl-paste`/`wl-copy`) -> X11 (`xclip` / `xsel`) -> thread-safe memory fallback

use image::{
    codecs::png::{CompressionType, FilterType as PngFilterType, PngEncoder},
    ColorType, ImageEncoder,
};
use std::io::Cursor;
use std::sync::{OnceLock, RwLock};

static FALLBACK_CLIPBOARD: OnceLock<RwLock<String>> = OnceLock::new();
static FALLBACK_CLIP_PNG: OnceLock<RwLock<Vec<u8>>> = OnceLock::new();

fn mem_clip() -> &'static RwLock<String> {
    FALLBACK_CLIPBOARD.get_or_init(|| RwLock::new(String::new()))
}

fn mem_clip_png() -> &'static RwLock<Vec<u8>> {
    FALLBACK_CLIP_PNG.get_or_init(|| RwLock::new(Vec::new()))
}

pub fn get_image_png() -> Vec<u8> {
    if let Some(png) = os_get_image_png() {
        if !png.is_empty() {
            if let Ok(mut g) = mem_clip_png().write() {
                *g = png.clone();
            }
            return png;
        }
    }
    mem_clip_png()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub fn set_image_png(png_bytes: &[u8]) -> bool {
    if png_bytes.len() < 8 || &png_bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return false;
    }
    if let Ok(mut g) = mem_clip_png().write() {
        *g = png_bytes.to_vec();
    }
    os_set_image_png(png_bytes)
}

#[allow(dead_code)]
fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || rgba.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut out = Cursor::new(Vec::with_capacity(rgba.len() / 2));
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, PngFilterType::Adaptive)
        .write_image(rgba, width, height, ColorType::Rgba8.into())
        .ok()?;
    Some(out.into_inner())
}

pub fn get_text() -> String {
    if let Some(os_txt) = os_get_text() {
        if let Ok(mut g) = mem_clip().write() {
            *g = os_txt.clone();
        }
        return os_txt;
    }
    mem_clip()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub fn set_text(text: &str) -> bool {
    if let Ok(mut g) = mem_clip().write() {
        *g = text.to_string();
    }
    os_set_text(text)
}

#[cfg(target_os = "windows")]
fn os_get_text() -> Option<String> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_UNICODETEXT: u32 = 13;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(u_format: u32) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalSize(h_mem: *mut c_void) -> usize;
    }

    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Some(String::new());
        }
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !opened {
            return None;
        }
        let mut result = None;
        let h_data = GetClipboardData(CF_UNICODETEXT);
        if !h_data.is_null() {
            let ptr = GlobalLock(h_data) as *const u16;
            if !ptr.is_null() {
                let max_u16 = GlobalSize(h_data) / 2;
                let mut len = 0usize;
                while len < max_u16 {
                    if *ptr.add(len) == 0 {
                        break;
                    }
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                result = Some(String::from_utf16_lossy(slice));
                let _ = GlobalUnlock(h_data);
            }
        }
        let _ = CloseClipboard();
        result
    }
}

#[cfg(target_os = "windows")]
fn os_set_text(text: &str) -> bool {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(u_format: u32, h_mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut c_void;
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let byte_len = wide.len() * 2;

    unsafe {
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !opened {
            return false;
        }
        let _ = EmptyClipboard();
        let h_mem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
        if h_mem.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        let dst = GlobalLock(h_mem) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(h_mem);
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(h_mem);
        let ok = !SetClipboardData(CF_UNICODETEXT, h_mem).is_null();
        if !ok {
            let _ = GlobalFree(h_mem);
        }
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(target_os = "windows")]
fn os_get_image_png() -> Option<Vec<u8>> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_DIB: u32 = 8;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn GetClipboardData(u_format: u32) -> *mut c_void;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalSize(h_mem: *mut c_void) -> usize;
    }

    unsafe {
        if IsClipboardFormatAvailable(CF_DIB) == 0 {
            return None;
        }
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !opened {
            return None;
        }
        let mut out_png = None;
        let h_data = GetClipboardData(CF_DIB);
        if !h_data.is_null() {
            let ptr = GlobalLock(h_data) as *const u8;
            let total_len = GlobalSize(h_data);
            if !ptr.is_null() && total_len > 40 {
                let dib = std::slice::from_raw_parts(ptr, total_len);
                let hdr_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
                let w = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
                let raw_h = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
                let bpp = u16::from_le_bytes([dib[14], dib[15]]);
                let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);

                let top_down = raw_h < 0;
                let h = raw_h.unsigned_abs();
                let px_offset = hdr_size + if compression == 3 && hdr_size == 40 { 12 } else { 0 };

                if w > 0 && w <= 8192 && h > 0 && h <= 8192 && (bpp == 24 || bpp == 32) && px_offset < total_len {
                    let wu = w as usize;
                    let hu = h as usize;
                    let bytes_per_px = (bpp / 8) as usize;
                    let stride = (wu * bytes_per_px + 3) & !3;
                    if px_offset + stride * hu <= total_len {
                        let mut rgba = vec![255u8; wu * hu * 4];
                        let mut any_alpha = false;
                        for dy in 0..hu {
                            let sy = if top_down { dy } else { hu - 1 - dy };
                            let srow = px_offset + sy * stride;
                            let drow = dy * wu * 4;
                            for dx in 0..wu {
                                let sp = srow + dx * bytes_per_px;
                                let dp = drow + dx * 4;
                                rgba[dp] = dib[sp + 2];     // R
                                rgba[dp + 1] = dib[sp + 1]; // G
                                rgba[dp + 2] = dib[sp];     // B
                                if bytes_per_px == 4 {
                                    let a = dib[sp + 3];
                                    if a != 0 {
                                        any_alpha = true;
                                    }
                                    rgba[dp + 3] = a;
                                }
                            }
                        }
                        if bytes_per_px == 4 && !any_alpha {
                            for px in rgba.chunks_exact_mut(4) {
                                px[3] = 255;
                            }
                        }
                        out_png = encode_rgba_to_png(&rgba, w as u32, h);
                    }
                }
                let _ = GlobalUnlock(h_data);
            }
        }
        let _ = CloseClipboard();
        out_png
    }
}

#[cfg(target_os = "windows")]
fn os_set_image_png(png_bytes: &[u8]) -> bool {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::time::Duration;

    const CF_DIB: u32 = 8;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(u_format: u32, h_mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut c_void;
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    let Ok(dyn_img) = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png) else {
        return false;
    };
    let rgba = dyn_img.to_rgba8();
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    if w == 0 || h == 0 {
        return false;
    }

    let px_bytes = w * h * 4;
    let mut dib = Vec::with_capacity(40 + px_bytes);
    dib.extend_from_slice(&40u32.to_le_bytes());          // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes());     // biWidth
    dib.extend_from_slice(&(h as i32).to_le_bytes());     // biHeight (positive = bottom-up)
    dib.extend_from_slice(&1u16.to_le_bytes());           // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes());          // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes());           // biCompression = BI_RGB
    dib.extend_from_slice(&(px_bytes as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes());           // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes());           // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes());           // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes());           // biClrImportant

    let raw = rgba.as_raw();
    for sy in (0..h).rev() {
        let row = sy * w * 4;
        for dx in 0..w {
            let p = row + dx * 4;
            dib.push(raw[p + 2]); // B
            dib.push(raw[p + 1]); // G
            dib.push(raw[p]);     // R
            dib.push(raw[p + 3]); // A
        }
    }

    unsafe {
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !opened {
            return false;
        }
        let _ = EmptyClipboard();
        let h_mem = GlobalAlloc(GMEM_MOVEABLE, dib.len());
        if h_mem.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        let dst = GlobalLock(h_mem) as *mut u8;
        if dst.is_null() {
            let _ = GlobalFree(h_mem);
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(dib.as_ptr(), dst, dib.len());
        let _ = GlobalUnlock(h_mem);
        let ok = !SetClipboardData(CF_DIB, h_mem).is_null();
        if !ok {
            let _ = GlobalFree(h_mem);
        }
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(target_os = "macos")]
fn os_get_text() -> Option<String> {
    use std::process::Command;
    let out = Command::new("/usr/bin/pbpaste")
        .env("LANG", "en_US.UTF-8")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(target_os = "macos")]
fn os_set_text(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("/usr/bin/pbcopy")
        .env("LANG", "en_US.UTF-8")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn os_get_image_png() -> Option<Vec<u8>> {
    use std::process::Command;
    let tmp = std::env::temp_dir().join(format!("ctd_clip_out_{}.png", rand::random::<u32>()));
    let path_str = tmp.to_string_lossy().replace('"', "\\\"");
    let script = format!(
        "try\nset f to open for access POSIX file \"{path_str}\" with write permission\nset eof f to 0\nwrite (the clipboard as «class PNGf») to f\nclose access f\nreturn \"OK\"\non error\ntry\nclose access POSIX file \"{path_str}\"\nend try\nreturn \"ERR\"\nend try"
    );
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    let ok = String::from_utf8_lossy(&out.stdout).trim() == "OK";
    let data = if ok { std::fs::read(&tmp).ok() } else { None };
    let _ = std::fs::remove_file(&tmp);
    data.filter(|b| b.len() > 8 && &b[0..8] == b"\x89PNG\r\n\x1a\n")
}

#[cfg(target_os = "macos")]
fn os_set_image_png(png_bytes: &[u8]) -> bool {
    use std::process::Command;
    let tmp = std::env::temp_dir().join(format!("ctd_clip_in_{}.png", rand::random::<u32>()));
    if std::fs::write(&tmp, png_bytes).is_err() {
        return false;
    }
    let path_str = tmp.to_string_lossy().replace('"', "\\\"");
    let script = format!(
        "set the clipboard to (read (POSIX file \"{path_str}\") as «class PNGf»)"
    );
    let ok = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    ok
}

#[cfg(target_os = "linux")]
fn os_get_text() -> Option<String> {
    use std::process::Command;
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(*args).output() {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn os_set_text(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn os_get_image_png() -> Option<Vec<u8>> {
    use std::process::Command;
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--type", "image/png"]),
        ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]),
    ];
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(*args).output() {
            if out.status.success() && out.stdout.len() > 8 && &out.stdout[0..8] == b"\x89PNG\r\n\x1a\n" {
                return Some(out.stdout);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn os_set_image_png(png_bytes: &[u8]) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &["--type", "image/png"]),
        ("xclip", &["-selection", "clipboard", "-t", "image/png", "-i"]),
    ];
    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(png_bytes);
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn os_get_image_png() -> Option<Vec<u8>> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn os_set_image_png(_png_bytes: &[u8]) -> bool {
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn os_get_text() -> Option<String> {
    None
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn os_set_text(_text: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_set_and_get_roundtrip() {
        let sample = "CopyToDesk-Clipboard-UTF8-测试-123";
        let _ = set_text(sample);
        let got = get_text();
        assert_eq!(got, sample);

        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255,
            0, 0, 255, 255, 255, 255, 0, 255,
        ];
        let png = encode_rgba_to_png(&rgba, 2, 2).unwrap();
        let _ = set_image_png(&png);
        let got_png = get_image_png();
        assert!(got_png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}
