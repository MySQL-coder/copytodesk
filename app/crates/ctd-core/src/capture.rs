//! Cross-platform multi-monitor screen capture with live display switching (v0.5.0 P2)
//! and automatic fallback to zero-GPU synthetic dual-monitor pattern on headless environments.

use ctd_protocol::MonitorInfo;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait ScreenCapturer: Send {
    fn backend_name(&self) -> &'static str;
    fn dimensions(&self) -> (u32, u32);
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn active_monitor(&self) -> u16;
    fn active_monitor_info(&self) -> MonitorInfo;
    fn select_monitor(&mut self, index: u16) -> Option<MonitorInfo>;
    /// Writes RGBA8 pixels (`width * height * 4`) into `buf`. Returns `true` if a new frame was captured.
    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool;
}

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Zero-GPU deterministic dual-monitor test pattern generator for headless VPS / CI / fallback mode.
pub struct SyntheticCapturer {
    monitors: Vec<MonitorInfo>,
    active_idx: u16,
    width: u32,
    height: u32,
    tick: u32,
}

impl SyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        let monitors = vec![
            MonitorInfo {
                index: 0,
                name: "Synthetic Primary".to_string(),
                x: 0,
                y: 0,
                width,
                height,
                is_primary: true,
            },
            MonitorInfo {
                index: 1,
                name: "Synthetic Secondary".to_string(),
                x: width as i32,
                y: 0,
                width: 1024,
                height: 768,
                is_primary: false,
            },
        ];
        Self {
            monitors,
            active_idx: 0,
            width,
            height,
            tick: 0,
        }
    }
}

impl ScreenCapturer for SyntheticCapturer {
    fn backend_name(&self) -> &'static str {
        "Synthetic-ZeroGPU"
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }

    fn active_monitor(&self) -> u16 {
        self.active_idx
    }

    fn active_monitor_info(&self) -> MonitorInfo {
        self.monitors
            .get(self.active_idx as usize)
            .cloned()
            .unwrap_or_else(|| self.monitors[0].clone())
    }

    fn select_monitor(&mut self, index: u16) -> Option<MonitorInfo> {
        let m = self.monitors.get(index as usize)?.clone();
        self.active_idx = index;
        self.width = m.width;
        self.height = m.height;
        self.tick = 0;
        Some(m)
    }

    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let w = self.width as usize;
        let h = self.height as usize;
        buf.resize(w * h * 4, 255);

        let accent_r = if self.active_idx == 0 { 64 } else { 16 };
        let accent_g = if self.active_idx == 0 { 158 } else { 185 };
        let accent_b = if self.active_idx == 0 { 255 } else { 129 };

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
            for y in (40..280.min(h)).step_by(32) {
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
                buf[o] = accent_r;
                buf[o + 1] = accent_g;
                buf[o + 2] = accent_b;
                buf[o + 3] = 255;
            }
        }
        true
    }
}

#[cfg(feature = "native-capture")]
pub struct NativeXcapCapturer {
    monitors: Vec<MonitorInfo>,
    active_idx: u16,
    width: u32,
    height: u32,
}

#[cfg(feature = "native-capture")]
fn enumerate_xcap_monitors() -> Option<Vec<(MonitorInfo, (u32, u32))>> {
    let raw = xcap::Monitor::all().ok()?;
    if raw.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(raw.len());
    for (idx, m) in raw.into_iter().enumerate() {
        let (cap_w, cap_h) = match m.capture_image() {
            Ok(img) if img.width() > 0 && img.height() > 0 => (img.width(), img.height()),
            _ => (m.width(), m.height()),
        };
        if cap_w == 0 || cap_h == 0 {
            continue;
        }
        let raw_name = format!("{}", m.name());
        let label = if raw_name.trim().is_empty() {
            format!("Display {}", idx + 1)
        } else {
            raw_name
        };
        let info = MonitorInfo {
            index: idx as u16,
            name: label,
            x: m.x(),
            y: m.y(),
            width: m.width().max(1),
            height: m.height().max(1),
            is_primary: m.is_primary(),
        };
        out.push((info, (cap_w, cap_h)));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(feature = "native-capture")]
impl NativeXcapCapturer {
    pub fn try_new() -> Option<Self> {
        let probed = enumerate_xcap_monitors()?;
        let primary_pos = probed
            .iter()
            .position(|(info, _)| info.is_primary)
            .unwrap_or(0);
        let (_, (width, height)) = probed[primary_pos];
        let active_idx = probed[primary_pos].0.index;
        let monitors = probed.into_iter().map(|(info, _)| info).collect();
        Some(Self {
            monitors,
            active_idx,
            width,
            height,
        })
    }
}

#[cfg(feature = "native-capture")]
impl ScreenCapturer for NativeXcapCapturer {
    fn backend_name(&self) -> &'static str {
        #[cfg(target_os = "windows")]
        {
            "Win-Native-Capture"
        }
        #[cfg(target_os = "macos")]
        {
            "macOS-Native-Capture"
        }
        #[cfg(target_os = "linux")]
        {
            "Linux-Native-Capture"
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        {
            "Native-Capture"
        }
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }

    fn active_monitor(&self) -> u16 {
        self.active_idx
    }

    fn active_monitor_info(&self) -> MonitorInfo {
        self.monitors
            .iter()
            .find(|m| m.index == self.active_idx)
            .cloned()
            .or_else(|| self.monitors.first().cloned())
            .unwrap_or(MonitorInfo {
                index: 0,
                name: "Primary".into(),
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
                is_primary: true,
            })
    }

    fn select_monitor(&mut self, index: u16) -> Option<MonitorInfo> {
        let probed = enumerate_xcap_monitors()?;
        let found = probed.iter().find(|(info, _)| info.index == index)?.clone();
        self.monitors = probed.into_iter().map(|(info, _)| info).collect();
        self.active_idx = found.0.index;
        self.width = found.1 .0;
        self.height = found.1 .1;
        Some(found.0)
    }

    fn capture_rgba(&mut self, buf: &mut Vec<u8>) -> bool {
        let Ok(monitors) = xcap::Monitor::all() else {
            return false;
        };
        let idx = self.active_idx as usize;
        let Some(monitor) = monitors
            .into_iter()
            .nth(idx)
            .or_else(|| xcap::Monitor::all().ok().and_then(|v| v.into_iter().next()))
        else {
            return false;
        };
        if let Ok(img) = monitor.capture_image() {
            let expected = (self.width * self.height * 4) as usize;
            let raw = if img.width() == self.width && img.height() == self.height {
                img.into_raw()
            } else {
                image::imageops::resize(
                    &img,
                    self.width,
                    self.height,
                    image::imageops::FilterType::Triangle,
                )
                .into_raw()
            };
            if raw.len() == expected {
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
