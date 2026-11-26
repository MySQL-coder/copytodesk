//! Cross-platform native input injection with multi-monitor coordinate mapping (v0.5.0 P2):
//! - Windows: Win32 `SendInput` mapped to active monitor within `VIRTUALDESK`
//! - macOS: CoreGraphics `CGEvent` mapped to active `CGDisplayBounds` + `CGEventSetFlags`
//! - Linux: Runtime `dlopen("libX11.so.6" + "libXtst.so.6")` native `XTest` injection (zero linker deps)
//! - Safe / Headless fallback: `DryRunAuditInjector`

use ctd_protocol::{KeyEventPacket, MonitorInfo, MouseEventPacket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait InputInjector: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn total_events(&self) -> u64;
    fn set_target_monitor(&self, mon: &MonitorInfo);
    fn inject_mouse(&self, ev: MouseEventPacket);
    fn inject_key(&self, ev: KeyEventPacket);
}

#[derive(Default)]
pub struct DryRunAuditInjector {
    count: AtomicU64,
}

impl InputInjector for DryRunAuditInjector {
    fn backend_name(&self) -> &'static str {
        "DryRun-Audit"
    }

    fn total_events(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn set_target_monitor(&self, _mon: &MonitorInfo) {}

    fn inject_mouse(&self, _ev: MouseEventPacket) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn inject_key(&self, _ev: KeyEventPacket) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(target_os = "windows")]
mod win_input {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicU32};

    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const MOUSEEVENTF_MOVE: u32 = 0x0001;
    const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    const MOUSEEVENTF_WHEEL: u32 = 0x0800;
    const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
    const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
    const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const KEYEVENTF_UNICODE: u32 = 0x0004;

    const SM_XVIRTUALSCREEN: i32 = 76;
    const SM_YVIRTUALSCREEN: i32 = 77;
    const SM_CXVIRTUALSCREEN: i32 = 78;
    const SM_CYVIRTUALSCREEN: i32 = 79;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseInput {
        dx: i32,
        dy: i32,
        mouse_data: u32,
        dw_flags: u32,
        time: u32,
        dw_extra_info: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeybdInput {
        w_vk: u16,
        w_scan: u16,
        dw_flags: u32,
        time: u32,
        dw_extra_info: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct HardwareInput {
        u_msg: u32,
        w_param_l: u16,
        w_param_h: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union InputUnion {
        mi: MouseInput,
        ki: KeybdInput,
        hi: HardwareInput,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Input {
        input_type: u32,
        u: InputUnion,
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
        fn GetSystemMetrics(n_index: i32) -> i32;
    }

    #[derive(Default)]
    pub struct Win32SendInputInjector {
        count: AtomicU64,
        mon_x: AtomicI32,
        mon_y: AtomicI32,
        mon_w: AtomicU32,
        mon_h: AtomicU32,
    }

    impl Win32SendInputInjector {
        fn map_to_virtual_desk(&self, norm_x: u16, norm_y: u16) -> (i32, i32) {
            let mw = self.mon_w.load(Ordering::Relaxed);
            let mh = self.mon_h.load(Ordering::Relaxed);
            if mw == 0 || mh == 0 {
                return (norm_x as i32, norm_y as i32);
            }
            unsafe {
                let vx = GetSystemMetrics(SM_XVIRTUALSCREEN) as i64;
                let vy = GetSystemMetrics(SM_YVIRTUALSCREEN) as i64;
                let vw = (GetSystemMetrics(SM_CXVIRTUALSCREEN) as i64).max(1);
                let vh = (GetSystemMetrics(SM_CYVIRTUALSCREEN) as i64).max(1);

                let mx = self.mon_x.load(Ordering::Relaxed) as i64;
                let my = self.mon_y.load(Ordering::Relaxed) as i64;
                let px = mx + ((norm_x as i64) * (mw.saturating_sub(1) as i64)) / 65535;
                let py = my + ((norm_y as i64) * (mh.saturating_sub(1) as i64)) / 65535;

                let abs_x = (((px - vx) * 65535) / vw.saturating_sub(1).max(1)).clamp(0, 65535);
                let abs_y = (((py - vy) * 65535) / vh.saturating_sub(1).max(1)).clamp(0, 65535);
                (abs_x as i32, abs_y as i32)
            }
        }
    }

    impl InputInjector for Win32SendInputInjector {
        fn backend_name(&self) -> &'static str {
            "Win32-SendInput"
        }

        fn total_events(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        fn set_target_monitor(&self, mon: &MonitorInfo) {
            self.mon_x.store(mon.x, Ordering::Relaxed);
            self.mon_y.store(mon.y, Ordering::Relaxed);
            self.mon_w.store(mon.width, Ordering::Relaxed);
            self.mon_h.store(mon.height, Ordering::Relaxed);
        }

        fn inject_mouse(&self, ev: MouseEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let mut flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
            let mut mouse_data = 0u32;
            match ev.action {
                0 => flags |= MOUSEEVENTF_MOVE,
                1 => {
                    flags |= MOUSEEVENTF_MOVE;
                    flags |= match ev.button {
                        1 => MOUSEEVENTF_LEFTDOWN,
                        2 => MOUSEEVENTF_RIGHTDOWN,
                        3 => MOUSEEVENTF_MIDDLEDOWN,
                        _ => 0,
                    };
                }
                2 => {
                    flags |= MOUSEEVENTF_MOVE;
                    flags |= match ev.button {
                        1 => MOUSEEVENTF_LEFTUP,
                        2 => MOUSEEVENTF_RIGHTUP,
                        3 => MOUSEEVENTF_MIDDLEUP,
                        _ => 0,
                    };
                }
                3 => {
                    flags |= MOUSEEVENTF_WHEEL;
                    mouse_data = (ev.wheel_delta as i32) as u32;
                }
                _ => {}
            }
            let (dx, dy) = self.map_to_virtual_desk(ev.norm_x, ev.norm_y);
            let inp = Input {
                input_type: INPUT_MOUSE,
                u: InputUnion {
                    mi: MouseInput {
                        dx,
                        dy,
                        mouse_data,
                        dw_flags: flags,
                        time: 0,
                        dw_extra_info: 0,
                    },
                },
            };
            unsafe {
                SendInput(1, &inp, std::mem::size_of::<Input>() as i32);
            }
        }

        fn inject_key(&self, ev: KeyEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            if ev.action == 2 {
                let pair = [
                    Input {
                        input_type: INPUT_KEYBOARD,
                        u: InputUnion {
                            ki: KeybdInput {
                                w_vk: 0,
                                w_scan: ev.unicode_char as u16,
                                dw_flags: KEYEVENTF_UNICODE,
                                time: 0,
                                dw_extra_info: 0,
                            },
                        },
                    },
                    Input {
                        input_type: INPUT_KEYBOARD,
                        u: InputUnion {
                            ki: KeybdInput {
                                w_vk: 0,
                                w_scan: ev.unicode_char as u16,
                                dw_flags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                                time: 0,
                                dw_extra_info: 0,
                            },
                        },
                    },
                ];
                unsafe {
                    SendInput(2, pair.as_ptr(), std::mem::size_of::<Input>() as i32);
                }
                return;
            }
            let mut flags = 0u32;
            if ev.action == 1 {
                flags |= KEYEVENTF_KEYUP;
            }
            if (ev.flags & 1) != 0 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            let inp = Input {
                input_type: INPUT_KEYBOARD,
                u: InputUnion {
                    ki: KeybdInput {
                        w_vk: ev.vk,
                        w_scan: ev.scan_code,
                        dw_flags: flags,
                        time: 0,
                        dw_extra_info: 0,
                    },
                },
            };
            unsafe {
                SendInput(1, &inp, std::mem::size_of::<Input>() as i32);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac_input {
    use super::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
    const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
    const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
    const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
    const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
    const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
    const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
    const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
    const K_CG_HID_EVENT_TAP: u32 = 0;

    const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
    const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
    const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
    const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut u32,
            display_count: *mut u32,
        ) -> i32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            mouse_type: u32,
            mouse_cursor_position: CGPoint,
            mouse_button: u32,
        ) -> *mut c_void;
        fn CGEventCreateKeyboardEvent(
            source: *const c_void,
            virtual_key: u16,
            key_down: bool,
        ) -> *mut c_void;
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventKeyboardSetUnicodeString(
            event: *mut c_void,
            string_length: usize,
            unicode_string: *const u16,
        );
        fn CGEventCreateScrollWheelEvent(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
        ) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *mut c_void);
    }

    #[derive(Default)]
    pub struct MacCoreGraphicsInjector {
        count: AtomicU64,
        left_down: AtomicBool,
        mod_flags: AtomicU64,
        mon_index: AtomicU32,
    }

    impl MacCoreGraphicsInjector {
        pub fn new() -> Self {
            unsafe {
                let _ = CGRequestScreenCaptureAccess();
            }
            Self::default()
        }

        fn to_point(&self, norm_x: u16, norm_y: u16) -> CGPoint {
            unsafe {
                let idx = self.mon_index.load(Ordering::Relaxed);
                let mut ids = [0u32; 16];
                let mut cnt = 0u32;
                let disp = if CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut cnt) == 0
                    && idx < cnt
                {
                    ids[idx as usize]
                } else {
                    CGMainDisplayID()
                };
                let b = CGDisplayBounds(disp);
                CGPoint {
                    x: b.origin.x + (norm_x as f64 / 65535.0) * b.size.width.max(1.0),
                    y: b.origin.y + (norm_y as f64 / 65535.0) * b.size.height.max(1.0),
                }
            }
        }
    }

    fn win_vk_to_mac_keycode(vk: u16) -> Option<u16> {
        match vk {
            0x08 => Some(51),        // Backspace / Delete
            0x09 => Some(48),        // Tab
            0x0D => Some(36),        // Return
            0x14 => Some(57),        // CapsLock
            0x1B => Some(53),        // Escape
            0x20 => Some(49),        // Space
            0x21 => Some(116),       // PageUp
            0x22 => Some(121),       // PageDown
            0x23 => Some(119),       // End
            0x24 => Some(115),       // Home
            0x25 => Some(123),       // Left
            0x26 => Some(126),       // Up
            0x27 => Some(124),       // Right
            0x28 => Some(125),       // Down
            0x2E => Some(117),       // Forward Delete
            0x10 => Some(56),        // Shift
            0x11 => Some(59),        // Control
            0x12 => Some(58),        // Option/Alt
            0x5B | 0x5C => Some(55), // Command
            0x41 => Some(0),         // A
            0x42 => Some(11),        // B
            0x43 => Some(8),         // C
            0x44 => Some(2),         // D
            0x45 => Some(14),        // E
            0x46 => Some(3),         // F
            0x47 => Some(5),         // G
            0x48 => Some(4),         // H
            0x49 => Some(34),        // I
            0x4A => Some(38),        // J
            0x4B => Some(40),        // K
            0x4C => Some(37),        // L
            0x4D => Some(46),        // M
            0x4E => Some(45),        // N
            0x4F => Some(31),        // O
            0x50 => Some(35),        // P
            0x51 => Some(12),        // Q
            0x52 => Some(15),        // R
            0x53 => Some(1),         // S
            0x54 => Some(17),        // T
            0x55 => Some(32),        // U
            0x56 => Some(9),         // V
            0x57 => Some(13),        // W
            0x58 => Some(7),         // X
            0x59 => Some(16),        // Y
            0x5A => Some(6),         // Z
            0x30 => Some(29),        // 0
            0x31 => Some(18),        // 1
            0x32 => Some(19),        // 2
            0x33 => Some(20),        // 3
            0x34 => Some(21),        // 4
            0x35 => Some(23),        // 5
            0x36 => Some(22),        // 6
            0x37 => Some(26),        // 7
            0x38 => Some(28),        // 8
            0x39 => Some(25),        // 9
            0xBA => Some(41),        // ;
            0xBB => Some(24),        // =
            0xBC => Some(43),        // ,
            0xBD => Some(27),        // -
            0xBE => Some(47),        // .
            0xBF => Some(44),        // /
            0xC0 => Some(50),        // `
            0xDB => Some(33),        // [
            0xDC => Some(42),        // \
            0xDD => Some(30),        // ]
            0xDE => Some(39),        // '
            0x70 => Some(122),       // F1
            0x71 => Some(120),       // F2
            0x72 => Some(99),        // F3 (Mission Control)
            0x73 => Some(118),       // F4
            0x74 => Some(96),        // F5
            0x75 => Some(97),        // F6
            0x76 => Some(98),        // F7
            0x77 => Some(100),       // F8
            0x78 => Some(101),       // F9
            0x79 => Some(109),       // F10
            0x7A => Some(103),       // F11 (Show Desktop)
            0x7B => Some(111),       // F12
            _ => None,
        }
    }

    impl InputInjector for MacCoreGraphicsInjector {
        fn backend_name(&self) -> &'static str {
            "macOS-CoreGraphics"
        }

        fn total_events(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        fn set_target_monitor(&self, mon: &MonitorInfo) {
            self.mon_index.store(mon.index as u32, Ordering::Relaxed);
        }

        fn inject_mouse(&self, ev: MouseEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let pt = self.to_point(ev.norm_x, ev.norm_y);
            unsafe {
                if ev.action == 3 {
                    // Smooth pixel-unit scrolling (`units = 0` = kCGScrollEventUnitPixel)
                    let px_delta = ((ev.wheel_delta as i32) / 3).clamp(-240, 240);
                    let delta = if px_delta == 0 {
                        if ev.wheel_delta > 0 { 4 } else { -4 }
                    } else {
                        px_delta
                    };
                    let e = CGEventCreateScrollWheelEvent(std::ptr::null(), 0, 1, delta);
                    if !e.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, e);
                        CFRelease(e);
                    }
                    return;
                }

                let (etype, btn) = match (ev.action, ev.button) {
                    (0, _) => {
                        if self.left_down.load(Ordering::Relaxed) {
                            (K_CG_EVENT_LEFT_MOUSE_DRAGGED, 0)
                        } else {
                            (K_CG_EVENT_MOUSE_MOVED, 0)
                        }
                    }
                    (1, 1) => {
                        self.left_down.store(true, Ordering::Relaxed);
                        (K_CG_EVENT_LEFT_MOUSE_DOWN, 0)
                    }
                    (2, 1) => {
                        self.left_down.store(false, Ordering::Relaxed);
                        (K_CG_EVENT_LEFT_MOUSE_UP, 0)
                    }
                    (1, 2) => (K_CG_EVENT_RIGHT_MOUSE_DOWN, 1),
                    (2, 2) => (K_CG_EVENT_RIGHT_MOUSE_UP, 1),
                    (1, 3) => (K_CG_EVENT_OTHER_MOUSE_DOWN, 2),
                    (2, 3) => (K_CG_EVENT_OTHER_MOUSE_UP, 2),
                    _ => return,
                };
                let e = CGEventCreateMouseEvent(std::ptr::null(), etype, pt, btn);
                if !e.is_null() {
                    CGEventPost(K_CG_HID_EVENT_TAP, e);
                    CFRelease(e);
                }
            }
        }

        fn inject_key(&self, ev: KeyEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            unsafe {
                if ev.action == 2 && ev.unicode_char > 0 {
                    let ch = char::from_u32(ev.unicode_char).unwrap_or(' ');
                    let mut utf16 = [0u16; 2];
                    let encoded = ch.encode_utf16(&mut utf16);
                    for down in [true, false] {
                        let e = CGEventCreateKeyboardEvent(std::ptr::null(), 0, down);
                        if !e.is_null() {
                            CGEventKeyboardSetUnicodeString(e, encoded.len(), encoded.as_ptr());
                            CGEventPost(K_CG_HID_EVENT_TAP, e);
                            CFRelease(e);
                        }
                    }
                    return;
                }
                if let Some(kc) = win_vk_to_mac_keycode(ev.vk) {
                    let down = ev.action == 0;
                    let mod_bit = match ev.vk {
                        0x10 => K_CG_EVENT_FLAG_MASK_SHIFT,
                        0x11 => K_CG_EVENT_FLAG_MASK_CONTROL,
                        0x12 => K_CG_EVENT_FLAG_MASK_ALTERNATE,
                        0x5B | 0x5C => K_CG_EVENT_FLAG_MASK_COMMAND,
                        _ => 0,
                    };
                    let mut cur_flags = if mod_bit != 0 {
                        if down {
                            self.mod_flags.fetch_or(mod_bit, Ordering::Relaxed) | mod_bit
                        } else {
                            self.mod_flags.fetch_and(!mod_bit, Ordering::Relaxed) & !mod_bit
                        }
                    } else {
                        self.mod_flags.load(Ordering::Relaxed)
                    };
                    if (ev.flags & ctd_protocol::key_mod_flags::SHIFT) != 0 {
                        cur_flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
                    }
                    if (ev.flags & ctd_protocol::key_mod_flags::CTRL) != 0 {
                        cur_flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
                    }
                    if (ev.flags & ctd_protocol::key_mod_flags::ALT) != 0 {
                        cur_flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
                    }
                    if (ev.flags & ctd_protocol::key_mod_flags::META) != 0 {
                        cur_flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
                    }

                    let e = CGEventCreateKeyboardEvent(std::ptr::null(), kc, down);
                    if !e.is_null() {
                        CGEventSetFlags(e, cur_flags);
                        CGEventPost(K_CG_HID_EVENT_TAP, e);
                        CFRelease(e);
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux_input {
    use super::*;
    use std::ffi::{c_char, c_int, c_uchar, c_uint, c_ulong, c_void};
    use std::sync::atomic::{AtomicI32, AtomicU32};
    use std::sync::Mutex;

    extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    pub struct LinuxXTestInjector {
        count: AtomicU64,
        dpy: Mutex<usize>,
        mon_x: AtomicI32,
        mon_y: AtomicI32,
        mon_w: AtomicU32,
        mon_h: AtomicU32,
        fake_motion: unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_ulong) -> c_int,
        fake_button: unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_ulong) -> c_int,
        fake_key: unsafe extern "C" fn(*mut c_void, c_uint, c_int, c_ulong) -> c_int,
        keysym_to_kc: unsafe extern "C" fn(*mut c_void, c_ulong) -> c_uchar,
        flush: unsafe extern "C" fn(*mut c_void) -> c_int,
    }

    impl LinuxXTestInjector {
        pub fn try_new() -> Option<Self> {
            unsafe {
                if std::env::var("DISPLAY").unwrap_or_default().is_empty() {
                    return None;
                }
                let x11 = dlopen(b"libX11.so.6\0".as_ptr() as *const c_char, 1);
                let xtst = dlopen(b"libXtst.so.6\0".as_ptr() as *const c_char, 1);
                if x11.is_null() || xtst.is_null() {
                    return None;
                }
                let open_sym = dlsym(x11, b"XOpenDisplay\0".as_ptr() as *const c_char);
                let flush_sym = dlsym(x11, b"XFlush\0".as_ptr() as *const c_char);
                let k2k_sym = dlsym(x11, b"XKeysymToKeycode\0".as_ptr() as *const c_char);
                let motion_sym = dlsym(xtst, b"XTestFakeMotionEvent\0".as_ptr() as *const c_char);
                let btn_sym = dlsym(xtst, b"XTestFakeButtonEvent\0".as_ptr() as *const c_char);
                let key_sym = dlsym(xtst, b"XTestFakeKeyEvent\0".as_ptr() as *const c_char);
                if open_sym.is_null()
                    || flush_sym.is_null()
                    || k2k_sym.is_null()
                    || motion_sym.is_null()
                    || btn_sym.is_null()
                    || key_sym.is_null()
                {
                    return None;
                }
                let open_fn: unsafe extern "C" fn(*const c_char) -> *mut c_void =
                    std::mem::transmute(open_sym);
                let dpy = open_fn(std::ptr::null());
                if dpy.is_null() {
                    return None;
                }
                Some(Self {
                    count: AtomicU64::new(0),
                    dpy: Mutex::new(dpy as usize),
                    mon_x: AtomicI32::new(0),
                    mon_y: AtomicI32::new(0),
                    mon_w: AtomicU32::new(1920),
                    mon_h: AtomicU32::new(1080),
                    fake_motion: std::mem::transmute(motion_sym),
                    fake_button: std::mem::transmute(btn_sym),
                    fake_key: std::mem::transmute(key_sym),
                    keysym_to_kc: std::mem::transmute(k2k_sym),
                    flush: std::mem::transmute(flush_sym),
                })
            }
        }
    }

    fn vk_to_x11_keysym(vk: u16) -> Option<c_ulong> {
        match vk {
            0x08 => Some(0xff08), // BackSpace
            0x09 => Some(0xff09), // Tab
            0x0D => Some(0xff0d), // Return
            0x14 => Some(0xffe5), // Caps_Lock
            0x1B => Some(0xff1b), // Escape
            0x20 => Some(0x0020), // space
            0x21 => Some(0xff55), // Page_Up
            0x22 => Some(0xff56), // Page_Down
            0x23 => Some(0xff57), // End
            0x24 => Some(0xff50), // Home
            0x25 => Some(0xff51), // Left
            0x26 => Some(0xff52), // Up
            0x27 => Some(0xff53), // Right
            0x28 => Some(0xff54), // Down
            0x2D => Some(0xff63), // Insert
            0x2E => Some(0xffff), // Delete
            0x10 => Some(0xffe1), // Shift_L
            0x11 => Some(0xffe3), // Control_L
            0x12 => Some(0xffe9), // Alt_L
            0x5B | 0x5C => Some(0xffeb), // Super_L
            0x30..=0x39 => Some(vk as c_ulong),
            0x41..=0x5A => Some((vk as c_ulong) + 32), // lowercase ASCII keysym
            0x70..=0x7B => Some(0xffbe + ((vk - 0x70) as c_ulong)), // F1..F12
            0xBA => Some(0x003b), // ;
            0xBB => Some(0x003d), // =
            0xBC => Some(0x002c), // ,
            0xBD => Some(0x002d), // -
            0xBE => Some(0x002e), // .
            0xBF => Some(0x002f), // /
            0xC0 => Some(0x0060), // `
            0xDB => Some(0x005b), // [
            0xDC => Some(0x005c), // \
            0xDD => Some(0x005d), // ]
            0xDE => Some(0x0027), // '
            _ => None,
        }
    }

    impl InputInjector for LinuxXTestInjector {
        fn backend_name(&self) -> &'static str {
            "Linux-X11-XTest"
        }

        fn total_events(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }

        fn set_target_monitor(&self, mon: &MonitorInfo) {
            self.mon_x.store(mon.x, Ordering::Relaxed);
            self.mon_y.store(mon.y, Ordering::Relaxed);
            self.mon_w.store(mon.width.max(1), Ordering::Relaxed);
            self.mon_h.store(mon.height.max(1), Ordering::Relaxed);
        }

        fn inject_mouse(&self, ev: MouseEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let Ok(guard) = self.dpy.lock() else { return };
            let dpy = *guard as *mut c_void;
            let mx = self.mon_x.load(Ordering::Relaxed) as i64;
            let my = self.mon_y.load(Ordering::Relaxed) as i64;
            let mw = self.mon_w.load(Ordering::Relaxed).max(1) as i64;
            let mh = self.mon_h.load(Ordering::Relaxed).max(1) as i64;
            let px = (mx + ((ev.norm_x as i64) * (mw - 1)) / 65535) as c_int;
            let py = (my + ((ev.norm_y as i64) * (mh - 1)) / 65535) as c_int;

            unsafe {
                let _ = (self.fake_motion)(dpy, -1, px, py, 0);
                match ev.action {
                    1 | 2 => {
                        let is_press = if ev.action == 1 { 1 } else { 0 };
                        let btn: c_uint = match ev.button {
                            2 => 3, // X11 right button = 3
                            3 => 2, // X11 middle button = 2
                            _ => 1,
                        };
                        let _ = (self.fake_button)(dpy, btn, is_press, 0);
                    }
                    3 => {
                        let btn: c_uint = if ev.wheel_delta >= 0 { 4 } else { 5 };
                        let _ = (self.fake_button)(dpy, btn, 1, 0);
                        let _ = (self.fake_button)(dpy, btn, 0, 0);
                    }
                    _ => {}
                }
                let _ = (self.flush)(dpy);
            }
        }

        fn inject_key(&self, ev: KeyEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let Ok(guard) = self.dpy.lock() else { return };
            let dpy = *guard as *mut c_void;
            unsafe {
                if ev.action == 2 && ev.unicode_char > 0 {
                    let ks: c_ulong = if ev.unicode_char <= 0x7e {
                        ev.unicode_char as c_ulong
                    } else {
                        0x0100_0000 | (ev.unicode_char as c_ulong)
                    };
                    let kc = (self.keysym_to_kc)(dpy, ks) as c_uint;
                    if kc != 0 {
                        let _ = (self.fake_key)(dpy, kc, 1, 0);
                        let _ = (self.fake_key)(dpy, kc, 0, 0);
                        let _ = (self.flush)(dpy);
                    }
                    return;
                }
                if let Some(ks) = vk_to_x11_keysym(ev.vk) {
                    let kc = (self.keysym_to_kc)(dpy, ks) as c_uint;
                    if kc != 0 {
                        let is_press = if ev.action == 0 { 1 } else { 0 };
                        let _ = (self.fake_key)(dpy, kc, is_press, 0);
                        let _ = (self.flush)(dpy);
                    }
                }
            }
        }
    }
}

pub fn create_injector(allow_input: bool) -> Arc<dyn InputInjector> {
    if allow_input {
        #[cfg(target_os = "windows")]
        return Arc::new(win_input::Win32SendInputInjector::default());
        #[cfg(target_os = "macos")]
        return Arc::new(mac_input::MacCoreGraphicsInjector::new());
        #[cfg(target_os = "linux")]
        if let Some(x11) = linux_input::LinuxXTestInjector::try_new() {
            return Arc::new(x11);
        }
    }
    Arc::new(DryRunAuditInjector::default())
}
