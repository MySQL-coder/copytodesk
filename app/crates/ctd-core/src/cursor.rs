//! Cross-platform native cursor position & shape sampler (v0.5.0 P2):
//! - Windows: Win32 `GetCursorInfo` + cached `LoadCursorW` standard system cursor handles
//! - macOS: CoreGraphics `CGEventCreate` / `CGEventGetLocation` + `CGDisplayBounds`
//! - Linux: Runtime `dlopen("libX11.so.6")` `XQueryPointer` (zero compile-time X11 link requirements)

use ctd_protocol::{CursorStatePacket, MonitorInfo};

/// Shape IDs matching frontend CSS cursor mapping:
/// 0=default, 1=text, 2=pointer, 3=wait, 4=crosshair, 5=ew-resize,
/// 6=ns-resize, 7=nwse-resize, 8=nesw-resize, 9=move, 10=not-allowed
pub fn sample_cursor(mon: &MonitorInfo) -> Option<CursorStatePacket> {
    os_sample_cursor(mon)
}

fn norm_in_monitor(px: f64, py: f64, mx: f64, my: f64, mw: f64, mh: f64) -> (u16, u16) {
    let rx = ((px - mx) / mw.max(1.0)).clamp(0.0, 1.0);
    let ry = ((py - my) / mh.max(1.0)).clamp(0.0, 1.0);
    ((rx * 65535.0).round() as u16, (ry * 65535.0).round() as u16)
}

#[cfg(target_os = "windows")]
fn os_sample_cursor(mon: &MonitorInfo) -> Option<CursorStatePacket> {
    use std::sync::OnceLock;

    const CURSOR_SHOWING: u32 = 0x0000_0001;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CursorInfo {
        cb_size: u32,
        flags: u32,
        h_cursor: usize,
        pt_screen_pos: Point,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetCursorInfo(pci: *mut CursorInfo) -> i32;
        fn LoadCursorW(h_instance: usize, lp_cursor_name: usize) -> usize;
    }

    static STD_CURSORS: OnceLock<Vec<(usize, u8)>> = OnceLock::new();
    let table = STD_CURSORS.get_or_init(|| unsafe {
        let pairs: &[(usize, u8)] = &[
            (32512, 0),  // IDC_ARROW
            (32513, 1),  // IDC_IBEAM
            (32649, 2),  // IDC_HAND
            (32514, 3),  // IDC_WAIT
            (32650, 3),  // IDC_APPSTARTING
            (32515, 4),  // IDC_CROSS
            (32644, 5),  // IDC_SIZEWE
            (32645, 6),  // IDC_SIZENS
            (32642, 7),  // IDC_SIZENWSE
            (32643, 8),  // IDC_SIZENESW
            (32646, 9),  // IDC_SIZEALL
            (32648, 10), // IDC_NO
        ];
        pairs
            .iter()
            .filter_map(|&(res_id, code)| {
                let h = LoadCursorW(0, res_id);
                if h != 0 {
                    Some((h, code))
                } else {
                    None
                }
            })
            .collect()
    });

    unsafe {
        let mut ci = CursorInfo {
            cb_size: std::mem::size_of::<CursorInfo>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut ci) == 0 {
            return None;
        }
        let visible = if (ci.flags & CURSOR_SHOWING) != 0 {
            1u8
        } else {
            0u8
        };
        let shape = table
            .iter()
            .find(|&&(h, _)| h == ci.h_cursor)
            .map(|&(_, code)| code)
            .unwrap_or(0);
        let (norm_x, norm_y) = norm_in_monitor(
            ci.pt_screen_pos.x as f64,
            ci.pt_screen_pos.y as f64,
            mon.x as f64,
            mon.y as f64,
            mon.width as f64,
            mon.height as f64,
        );
        Some(CursorStatePacket {
            norm_x,
            norm_y,
            shape,
            visible,
        })
    }
}

#[cfg(target_os = "macos")]
fn os_sample_cursor(mon: &MonitorInfo) -> Option<CursorStatePacket> {
    use std::ffi::c_void;

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

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut u32,
            display_count: *mut u32,
        ) -> i32;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> CGRect;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *mut c_void);
    }

    unsafe {
        let ev = CGEventCreate(std::ptr::null());
        if ev.is_null() {
            return None;
        }
        let pt = CGEventGetLocation(ev);
        CFRelease(ev);

        let mut ids = [0u32; 16];
        let mut count = 0u32;
        let disp_id = if CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) == 0
            && (mon.index as u32) < count
        {
            ids[mon.index as usize]
        } else {
            CGMainDisplayID()
        };
        let b = CGDisplayBounds(disp_id);
        let (norm_x, norm_y) = norm_in_monitor(
            pt.x,
            pt.y,
            b.origin.x,
            b.origin.y,
            b.size.width,
            b.size.height,
        );
        Some(CursorStatePacket {
            norm_x,
            norm_y,
            shape: 0,
            visible: 1,
        })
    }
}

#[cfg(target_os = "linux")]
fn os_sample_cursor(mon: &MonitorInfo) -> Option<CursorStatePacket> {
    use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
    use std::sync::{Mutex, OnceLock};

    extern "C" {
        fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    struct X11CursorQuery {
        dpy: Mutex<usize>,
        root: c_ulong,
        query_ptr: unsafe extern "C" fn(
            *mut c_void,
            c_ulong,
            *mut c_ulong,
            *mut c_ulong,
            *mut c_int,
            *mut c_int,
            *mut c_int,
            *mut c_int,
            *mut c_uint,
        ) -> c_int,
    }

    static X11_CURSOR: OnceLock<Option<X11CursorQuery>> = OnceLock::new();
    let api = X11_CURSOR
        .get_or_init(|| unsafe {
            if std::env::var("DISPLAY").unwrap_or_default().is_empty() {
                return None;
            }
            let lib = dlopen(b"libX11.so.6\0".as_ptr() as *const c_char, 1);
            if lib.is_null() {
                return None;
            }
            let open_sym = dlsym(lib, b"XOpenDisplay\0".as_ptr() as *const c_char);
            let root_sym = dlsym(lib, b"XDefaultRootWindow\0".as_ptr() as *const c_char);
            let query_sym = dlsym(lib, b"XQueryPointer\0".as_ptr() as *const c_char);
            if open_sym.is_null() || root_sym.is_null() || query_sym.is_null() {
                return None;
            }
            let open_fn: unsafe extern "C" fn(*const c_char) -> *mut c_void =
                std::mem::transmute(open_sym);
            let root_fn: unsafe extern "C" fn(*mut c_void) -> c_ulong =
                std::mem::transmute(root_sym);
            let query_ptr = std::mem::transmute(query_sym);
            let dpy = open_fn(std::ptr::null());
            if dpy.is_null() {
                return None;
            }
            let root = root_fn(dpy);
            Some(X11CursorQuery {
                dpy: Mutex::new(dpy as usize),
                root,
                query_ptr,
            })
        })
        .as_ref()?;

    let guard = api.dpy.lock().ok()?;
    unsafe {
        let mut root_ret: c_ulong = 0;
        let mut child_ret: c_ulong = 0;
        let mut rx: c_int = 0;
        let mut ry: c_int = 0;
        let mut wx: c_int = 0;
        let mut wy: c_int = 0;
        let mut mask: c_uint = 0;
        if (api.query_ptr)(
            *guard as *mut c_void,
            api.root,
            &mut root_ret,
            &mut child_ret,
            &mut rx,
            &mut ry,
            &mut wx,
            &mut wy,
            &mut mask,
        ) == 0
        {
            return None;
        }
        let (norm_x, norm_y) = norm_in_monitor(
            rx as f64,
            ry as f64,
            mon.x as f64,
            mon.y as f64,
            mon.width as f64,
            mon.height as f64,
        );
        Some(CursorStatePacket {
            norm_x,
            norm_y,
            shape: 0,
            visible: 1,
        })
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn os_sample_cursor(_mon: &MonitorInfo) -> Option<CursorStatePacket> {
    None
}
