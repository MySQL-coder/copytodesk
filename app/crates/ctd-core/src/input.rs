//! Cross-platform input injection abstraction.
//! - Windows: native Win32 `SendInput` FFI (zero extra crates, supports absolute mouse + Unicode/VK keys)
//! - macOS / Linux & safe default: `DryRunAuditInjector`

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

pub fn create_injector(allow_input: bool) -> Arc<dyn InputInjector> {
    if allow_input {
        #[cfg(target_os = "windows")]
        return Arc::new(win_input::Win32SendInputInjector::default());
    }
    Arc::new(DryRunAuditInjector::default())
}
