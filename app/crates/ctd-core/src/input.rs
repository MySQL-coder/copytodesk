//! Cross-platform native input injection:
//! - Windows: Win32 `SendInput` (absolute mouse, wheel, VK + Unicode keys)
//! - macOS: CoreGraphics `CGEvent` (absolute mouse move/click/drag/scroll + Unicode/keyCode keys)
//!   Note: macOS requires granting Accessibility (辅助功能) permission to CopyToDesk.
//! - Linux / Safe mode: `DryRunAuditInjector`

use ctd_protocol::{KeyEventPacket, MouseEventPacket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait InputInjector: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn total_events(&self) -> u64;
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
    }

    #[derive(Default)]
    pub struct Win32SendInputInjector {
        count: AtomicU64,
    }

    impl InputInjector for Win32SendInputInjector {
        fn backend_name(&self) -> &'static str {
            "Win32-SendInput"
        }

        fn total_events(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
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
            let inp = Input {
                input_type: INPUT_MOUSE,
                u: InputUnion {
                    mi: MouseInput {
                        dx: ev.norm_x as i32,
                        dy: ev.norm_y as i32,
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
    use std::sync::atomic::AtomicBool;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
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

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayPixelsWide(display: u32) -> usize;
        fn CGDisplayPixelsHigh(display: u32) -> usize;
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
    }

    impl MacCoreGraphicsInjector {
        pub fn new() -> Self {
            // Proactively trigger macOS Screen Recording permission prompt if not granted yet
            unsafe {
                let _ = CGRequestScreenCaptureAccess();
            }
            Self::default()
        }

        fn to_point(norm_x: u16, norm_y: u16) -> CGPoint {
            unsafe {
                let disp = CGMainDisplayID();
                let w = CGDisplayPixelsWide(disp).max(1) as f64;
                let h = CGDisplayPixelsHigh(disp).max(1) as f64;
                CGPoint {
                    x: (norm_x as f64 / 65535.0) * w,
                    y: (norm_y as f64 / 65535.0) * h,
                }
            }
        }
    }

    fn win_vk_to_mac_keycode(vk: u16) -> Option<u16> {
        match vk {
            0x08 => Some(51),  // Backspace / Delete
            0x09 => Some(48),  // Tab
            0x0D => Some(36),  // Return
            0x1B => Some(53),  // Escape
            0x20 => Some(49),  // Space
            0x25 => Some(123), // Left
            0x26 => Some(126), // Up
            0x27 => Some(124), // Right
            0x28 => Some(125), // Down
            0x2E => Some(117), // Forward Delete
            0x10 => Some(56),  // Shift
            0x11 => Some(59),  // Control
            0x12 => Some(58),  // Option/Alt
            0x5B | 0x5C => Some(55), // Command
            0x41 => Some(0),  // A
            0x42 => Some(11), // B
            0x43 => Some(8),  // C
            0x44 => Some(2),  // D
            0x45 => Some(14), // E
            0x46 => Some(3),  // F
            0x47 => Some(5),  // G
            0x48 => Some(4),  // H
            0x49 => Some(34), // I
            0x4A => Some(38), // J
            0x4B => Some(40), // K
            0x4C => Some(37), // L
            0x4D => Some(46), // M
            0x4E => Some(45), // N
            0x4F => Some(31), // O
            0x50 => Some(35), // P
            0x51 => Some(12), // Q
            0x52 => Some(15), // R
            0x53 => Some(1),  // S
            0x54 => Some(17), // T
            0x55 => Some(32), // U
            0x56 => Some(9),  // V
            0x57 => Some(13), // W
            0x58 => Some(7),  // X
            0x59 => Some(16), // Y
            0x5A => Some(6),  // Z
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

        fn inject_mouse(&self, ev: MouseEventPacket) {
            self.count.fetch_add(1, Ordering::Relaxed);
            let pt = Self::to_point(ev.norm_x, ev.norm_y);
            unsafe {
                if ev.action == 3 {
                    // Scroll wheel (1 = line units)
                    let lines = (ev.wheel_delta as i32) / 40;
                    let delta = if lines == 0 { if ev.wheel_delta > 0 { 1 } else { -1 } } else { lines };
                    let e = CGEventCreateScrollWheelEvent(std::ptr::null(), 1, 1, delta);
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
                    let e = CGEventCreateKeyboardEvent(std::ptr::null(), kc, down);
                    if !e.is_null() {
                        CGEventPost(K_CG_HID_EVENT_TAP, e);
                        CFRelease(e);
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
    }
    Arc::new(DryRunAuditInjector::default())
}
