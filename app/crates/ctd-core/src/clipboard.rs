//! Cross-platform native OS system clipboard integration (v0.4.0 P1):
//! - Windows: Pure Win32 FFI (`OpenClipboard` / `CF_UNICODETEXT` / `GlobalAlloc`), zero extra crates
//! - macOS: Native `/usr/bin/pbpaste` & `/usr/bin/pbcopy` with UTF-8 locale
//! - Linux: Wayland (`wl-paste`/`wl-copy`) -> X11 (`xclip` / `xsel`) -> thread-safe memory fallback

use std::sync::{OnceLock, RwLock};

static FALLBACK_CLIPBOARD: OnceLock<RwLock<String>> = OnceLock::new();

fn mem_clip() -> &'static RwLock<String> {
    FALLBACK_CLIPBOARD.get_or_init(|| RwLock::new(String::new()))
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
    }
}
