//! Cross-platform screen capture abstraction with automatic fallback to zero-GPU synthetic pattern.

use std::time::{SystemTime, UNIX_EPOCH};

pub trait ScreenCapturer: Send {
    fn backend_name(&self) -> &'static str;
    fn dimensions(&self) -> (u32, u32);
    /// Writes RGBA8 pixels (`width * height * 4`) into `buf`. Returns `true` if a new frame was captured.
    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool;
}

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Zero-GPU deterministic test pattern generator for headless VPS / CI / fallback mode.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    tick: u32,
}

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height, tick: 0 }
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn backend_name(&self) -> &'static str {
        "Synthetic-ZeroGPU"
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let w = self.width as usize;
        let h = self.height as usize;
        buf.resize(w * h * 4, 255);

        // Dark slate background with moving accent block along bottom strip (top stays static for Pass-2 refinement)
        if self.tick == 1 {
            for y in 0..h {
                for x in 0..w {
                    let o = (y * w + x) * 4;
                    buf[o] = 20;
                    buf[o + 1] = 26;
                    buf[o + 2] = 36;
                    buf[o + 3] = 255;
                }
            }
            // High-contrast static grid lines on upper region
            for y in (40..280).step_by(32) {
                for x in 40..640.min(w) {
                    let o = (y * w + x) * 4;
                    buf[o] = 210;
                    buf[o + 1] = 225;
                    buf[o + 2] = 245;
                }
            }
        }

        let strip_y = h.saturating_sub(140);
        for y in strip_y..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                buf[o] = 30;
                buf[o + 1] = 38;
                buf[o + 2] = 52;
                buf[o + 3] = 255;
            }
        }
        let bx = ((self.tick as usize) * 16) % w.saturating_sub(110).max(1);
        for y in (strip_y + 20)..(strip_y + 110).min(h) {
            for x in bx..(bx + 90).min(w) {
                let o = (y * w + x) * 4;
                buf[o] = 64;
                buf[o + 1] = 158;
                buf[o + 2] = 255;
                buf[o + 3] = 255;
            }
        }
        true
    }
}

#[cfg(feature = "native-capture")]
pub struct NativeXcapCapturer {
    monitor: xcap::Monitor,
    width: u32,
    height: u32,
}

#[cfg(feature = "native-capture")]
impl NativeXcapCapturer {
    pub fn try_new() -> Option<Self> {
        let monitors = xcap::Monitor::all().ok()?;
        let monitor = monitors
            .into_iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .or_else(|| xcap::Monitor::all().ok()?.into_iter().next())?;
        let width = monitor.width().ok()?;
        let height = monitor.height().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some(Self { monitor, width, height })
    }
}

#[cfg(feature = "native-capture")]
impl ScreenCapturer for NativeXcapCapturer {
    fn backend_name(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        { "Win-Native-Capture" }
        #[cfg(target_os = "macos")]
        { "macOS-Native-Capture" }
        #[cfg(target_os = "linux")]
        { "Linux-Native-Capture" }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        { "Native-Capture" }
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool {
        if let Ok(img) = self.monitor.capture_image() {
            let raw = img.into_raw();
            if raw.len() == (self.width * self.height * 4) as usize {
                *buf = raw;
                return true;
            }
        }
        false
    }
}

pub fn create_capturer(force_synthetic: bool) -> Box<dyn ScreenCapturer> {
    if !force_synthetic {
        #[cfg(feature = "native-capture")]
        if let Some(native) = NativeXcapCapturer::try_new() {
            return Box::new(native);
        }
    }
    Box::new(SyntheticCapturer::new(1280, 720))
}
