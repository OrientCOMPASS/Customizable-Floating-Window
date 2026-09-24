//! Platform window plumbing (round-5 design, per user-verified recipe).
//!
//! # Windows — the user-verified recipe (no hide/show, ever)
//! Applied from the UI 33 ms timer once the raw HWND exists:
//!   1. strip caption/frame/sysmenu/boxes from `GWL_STYLE`;
//!   2. `GWL_EXSTYLE`: add `WS_EX_TOOLWINDOW | WS_EX_TOPMOST`, remove
//!      `WS_EX_APPWINDOW` (taskbar/Alt-Tab exclusion);
//!   3. `SetWindowPos(SWP_NOMOVE|SWP_NOSIZE|SWP_FRAMECHANGED|SWP_NOACTIVATE)`;
//!   4. every ~30 ticks re-assert topmost with
//!      `SetWindowPos(SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE)`.
//! Crucially **never** `ShowWindow(SW_HIDE/SW_SHOW)`: that destroys the DWM
//! transparent composition surface and desyncs winit's visibility state
//! (round-4 regression: grey partial repaints, caption & taskbar returning).
//! Transparency relies solely on Slint's native `background: transparent`.
//!
//! # X11 / macOS / Wayland
//! X11: `_NET_WM_STATE_SKIP_TASKBAR/+SKIP_PAGER` (WM-managed, persists) plus
//! `_MOTIF_WM_HINTS` decorations=0. macOS: process-level Accessory activation
//! policy (helper process only). Wayland: compositor policy → no-op.

use slint::Window;

// ─────────────────────────── Windows ───────────────────────────

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    pub type HWND = *mut c_void;
    pub const GWL_STYLE: i32 = -16;
    pub const GWL_EXSTYLE: i32 = -20;
    pub const WS_CAPTION: isize = 0x00C0_0000;
    pub const WS_THICKFRAME: isize = 0x0004_0000;
    pub const WS_SYSMENU: isize = 0x0008_0000;
    pub const WS_MINIMIZEBOX: isize = 0x0002_0000;
    pub const WS_MAXIMIZEBOX: isize = 0x0001_0000;
    pub const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    pub const WS_EX_APPWINDOW: isize = 0x0004_0000;
    pub const WS_EX_TOPMOST: isize = 0x0000_0008;
    pub const SWP_NOSIZE: u32 = 0x0001;
    pub const SWP_NOMOVE: u32 = 0x0002;
    pub const SWP_NOZORDER: u32 = 0x0004;
    pub const SWP_NOACTIVATE: u32 = 0x0010;
    pub const SWP_FRAMECHANGED: u32 = 0x0020;
    pub const SM_CXSCREEN: i32 = 0;
    pub const SM_CYSCREEN: i32 = 1;

    #[link(name = "user32")]
    extern "system" {
        pub fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, new_long: isize) -> isize;
        pub fn SetWindowPos(
            hwnd: HWND,
            insert_after: HWND,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        pub fn GetSystemMetrics(index: i32) -> i32;
        pub fn GetCursorPos(lp: *mut Point) -> i32;
    }
    #[repr(C)]
    pub struct Point {
        pub x: i32,
        pub y: i32,
    }
    pub const HWND_TOPMOST: HWND = -1isize as *mut c_void;
}

/// Per-window mutable state for the Windows recipe (lives in the UI state).
#[derive(Default)]
pub struct WinHardening {
    pub hwnd: Option<usize>,
    pub applied: bool,
}

/// Windows: acquire HWND + apply/refresh the user-verified style recipe.
/// Returns a short description for logging on first application.
#[cfg(windows)]
pub fn windows_harden(window: &Window, st: &mut WinHardening, tick: u32) -> Option<&'static str> {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    if st.hwnd.is_none() {
        let wh = window.window_handle();
        if let Ok(handle) = wh.window_handle() {
            if let RawWindowHandle::Win32(w) = handle.as_raw() {
                st.hwnd = Some(w.hwnd.get() as usize);
            }
        }
    }
    let Some(hwnd) = st.hwnd.map(|h| h as win::HWND) else {
        return None;
    };
    unsafe {
        if !st.applied {
            let style = win::GetWindowLongPtrW(hwnd, win::GWL_STYLE);
            let new_style = style
                & !win::WS_CAPTION
                & !win::WS_SYSMENU
                & !win::WS_MINIMIZEBOX
                & !win::WS_MAXIMIZEBOX
                & !win::WS_THICKFRAME;
            win::SetWindowLongPtrW(hwnd, win::GWL_STYLE, new_style);
            let ex = win::GetWindowLongPtrW(hwnd, win::GWL_EXSTYLE);
            let new_ex =
                (ex | win::WS_EX_TOOLWINDOW | win::WS_EX_TOPMOST) & !win::WS_EX_APPWINDOW;
            win::SetWindowLongPtrW(hwnd, win::GWL_EXSTYLE, new_ex);
            // NO ShowWindow hide/show here — see module docs.
            win::SetWindowPos(
                hwnd,
                win::HWND_TOPMOST,
                0,
                0,
                0,
                0,
                win::SWP_NOMOVE | win::SWP_NOSIZE | win::SWP_FRAMECHANGED | win::SWP_NOACTIVATE,
            );
            st.applied = true;
            return Some("borderless+toolwindow+topmost (native transparency preserved)");
        } else if tick % 30 == 0 {
            win::SetWindowPos(
                hwnd,
                win::HWND_TOPMOST,
                0,
                0,
                0,
                0,
                win::SWP_NOMOVE | win::SWP_NOSIZE | win::SWP_NOACTIVATE,
            );
        }
    }
    None
}

#[cfg(not(windows))]
pub fn windows_harden(_window: &Window, _st: &mut WinHardening, _tick: u32) -> Option<&'static str> {
    None
}

// ─────────────────────────── X11 ───────────────────────────

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn linux_skip_taskbar(window: &Window) -> &'static str {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    if is_wayland() {
        return "wayland-noop";
    }
    let wh = window.window_handle();
    let Ok(handle) = wh.window_handle() else {
        return "no-handle";
    };
    let RawWindowHandle::Xlib(h) = handle.as_raw() else {
        return "not-x11";
    };
    let win_id = h.window as x11_dl::xlib::Window;
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else {
        return "no-xlib";
    };
    unsafe {
        let disp = (xlib.XOpenDisplay)(std::ptr::null());
        if disp.is_null() {
            return "no-display";
        }
        let state_atom = intern(&xlib, disp, b"_NET_WM_STATE\0");
        let skip_taskbar = intern(&xlib, disp, b"_NET_WM_STATE_SKIP_TASKBAR\0");
        let skip_pager = intern(&xlib, disp, b"_NET_WM_STATE_SKIP_PAGER\0");
        if state_atom == 0 || skip_taskbar == 0 || skip_pager == 0 {
            (xlib.XCloseDisplay)(disp);
            return "atom-error";
        }
        const XA_ATOM: x11_dl::xlib::Atom = 4;
        let mut actual_type: x11_dl::xlib::Atom = 0;
        let mut actual_format: std::os::raw::c_int = 0;
        let mut nitems: std::os::raw::c_ulong = 0;
        let mut bytes_after: std::os::raw::c_ulong = 0;
        let mut prop: *mut std::os::raw::c_uchar = std::ptr::null_mut();
        (xlib.XGetWindowProperty)(
            disp,
            win_id,
            state_atom,
            0,
            64,
            0,
            XA_ATOM,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut prop,
        );
        let mut atoms: Vec<x11_dl::xlib::Atom> = Vec::new();
        if !prop.is_null() {
            if actual_type == XA_ATOM && actual_format == 32 && nitems > 0 {
                atoms.extend_from_slice(std::slice::from_raw_parts(
                    prop as *const x11_dl::xlib::Atom,
                    nitems as usize,
                ));
            }
            (xlib.XFree)(prop as *mut std::os::raw::c_void);
        }
        for a in [skip_taskbar, skip_pager] {
            if !atoms.contains(&a) {
                atoms.push(a);
            }
        }
        (xlib.XChangeProperty)(
            disp,
            win_id,
            state_atom,
            XA_ATOM,
            32,
            0,
            atoms.as_ptr() as *const std::os::raw::c_uchar,
            atoms.len() as std::os::raw::c_int,
        );
        let motif = intern(&xlib, disp, b"_MOTIF_WM_HINTS\0");
        if motif != 0 {
            #[repr(C)]
            struct MotifHints {
                flags: u64,
                functions: u64,
                decorations: u64,
                input_mode: i64,
                status: u64,
            }
            let hints = MotifHints {
                flags: 1 << 1,
                functions: 0,
                decorations: 0,
                input_mode: 0,
                status: 0,
            };
            (xlib.XChangeProperty)(
                disp,
                win_id,
                motif,
                motif,
                32,
                0,
                &hints as *const MotifHints as *const std::os::raw::c_uchar,
                5,
            );
        }
        (xlib.XFlush)(disp);
        (xlib.XCloseDisplay)(disp);
    }
    "skip-taskbar-x11"
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
unsafe fn intern(
    xlib: &x11_dl::xlib::Xlib,
    disp: *mut x11_dl::xlib::Display,
    name: &'static [u8],
) -> x11_dl::xlib::Atom {
    unsafe {
        (xlib.XInternAtom)(
            disp,
            name.as_ptr() as *const std::os::raw::c_char,
            0,
        )
    }
}

// ─────────────────────────── macOS ─────────────────────────────

#[cfg(target_os = "macos")]
use std::ffi::{c_char, c_void};

#[cfg(target_os = "macos")]
type MsgSend0 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
#[cfg(target_os = "macos")]
type MsgSendI = unsafe extern "C" fn(*mut c_void, *mut c_void, isize);

#[cfg(target_os = "macos")]
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
}

/// macOS: Dock / Cmd-Tab exclusion via process-level Accessory policy.
#[cfg(target_os = "macos")]
pub fn macos_set_accessory_policy() {
    use std::ffi::CString;
    const ACCESSORY: isize = 1;
    unsafe {
        let Ok(cls_name) = CString::new("NSApplication") else { return };
        let Ok(sel_shared) = CString::new("sharedApplication") else { return };
        let Ok(sel_policy) = CString::new("setActivationPolicy:") else { return };
        let cls = objc_getClass(cls_name.as_ptr());
        if cls.is_null() {
            return;
        }
        let sel_shared = sel_registerName(sel_shared.as_ptr());
        let sel_policy = sel_registerName(sel_policy.as_ptr());
        if sel_shared.is_null() || sel_policy.is_null() {
            return;
        }
        let msg0: MsgSend0 = std::mem::transmute(objc_msgSend as *const c_void);
        let app = msg0(cls, sel_shared);
        if app.is_null() {
            return;
        }
        let msg1: MsgSendI = std::mem::transmute(objc_msgSend as *const c_void);
        msg1(app, sel_policy, ACCESSORY);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn macos_set_accessory_policy() {}

// ─────────────────────────── common ───────────────────────────

/// Primary screen size in physical pixels (None → keep WM placement).
pub fn screen_size() -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        unsafe {
            let w = win::GetSystemMetrics(win::SM_CXSCREEN);
            let h = win::GetSystemMetrics(win::SM_CYSCREEN);
            (w > 0 && h > 0).then_some((w, h))
        }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe {
            let id = CGMainDisplayID();
            let w = CGDisplayPixelsWide(id) as i32;
            let h = CGDisplayPixelsHigh(id) as i32;
            (w > 0 && h > 0).then_some((w, h))
        }
    }
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        if is_wayland() {
            return None;
        }
        let xlib = x11_dl::xlib::Xlib::open().ok()?;
        unsafe {
            let disp = (xlib.XOpenDisplay)(std::ptr::null());
            if disp.is_null() {
                return None;
            }
            let screen = (xlib.XDefaultScreen)(disp);
            let w = (xlib.XDisplayWidth)(disp, screen);
            let h = (xlib.XDisplayHeight)(disp, screen);
            (xlib.XCloseDisplay)(disp);
            (w > 0 && h > 0).then_some((w, h))
        }
    }
}

/// Whether absolute window positioning / global cursor work (false: Wayland).
pub fn position_api_usable() -> bool {
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    {
        !is_wayland()
    }
    #[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
    {
        true
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none()
        || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
}

/// Global cursor position in physical pixels (drag tracking). None: Wayland.
#[cfg(windows)]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    unsafe {
        let mut p = win::Point { x: 0, y: 0 };
        if win::GetCursorPos(&mut p) != 0 {
            Some((p.x as f64, p.y as f64))
        } else {
            None
        }
    }
}

#[cfg(all(target_os = "linux", not(target_os = "android")))]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    use x11_dl::xlib;
    if is_wayland() {
        return None;
    }
    thread_local! {
        static DISP: std::cell::Cell<Option<*mut xlib::Display>> = const { std::cell::Cell::new(None) };
    }
    let Ok(xlib) = xlib::Xlib::open() else { return None };
    let disp = DISP.with(|d| {
        if let Some(p) = d.get() {
            return p;
        }
        let p = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
        d.set(Some(p));
        p
    });
    if disp.is_null() {
        return None;
    }
    unsafe {
        let screen = (xlib.XDefaultScreen)(disp);
        let root = (xlib.XRootWindow)(disp, screen);
        let mut root_ret: xlib::Window = 0;
        let mut child: xlib::Window = 0;
        let mut rx: std::os::raw::c_int = 0;
        let mut ry: std::os::raw::c_int = 0;
        let mut wx: std::os::raw::c_int = 0;
        let mut wy: std::os::raw::c_int = 0;
        let mut mask: std::os::raw::c_uint = 0;
        if (xlib.XQueryPointer)(
            disp, root, &mut root_ret, &mut child, &mut rx, &mut ry, &mut wx, &mut wy, &mut mask,
        ) == 0
        {
            return None;
        }
        Some((rx as f64, ry as f64))
    }
}

#[cfg(target_os = "macos")]
pub fn global_cursor(scale: f32) -> Option<(f64, f64)> {
    use std::ffi::CString;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsPoint {
        x: f64,
        y: f64,
    }
    type MsgSendPoint = unsafe extern "C" fn(*mut c_void, *mut c_void) -> NsPoint;
    unsafe {
        let cls_name = CString::new("NSEvent").ok()?;
        let sel_name = CString::new("mouseLocation").ok()?;
        let cls = objc_getClass(cls_name.as_ptr());
        let sel = sel_registerName(sel_name.as_ptr());
        if cls.is_null() || sel.is_null() {
            return None;
        }
        let msg: MsgSendPoint = std::mem::transmute(objc_msgSend as *const c_void);
        let p = msg(cls, sel);
        let s = f64::from(scale.max(0.5));
        let h_px = CGDisplayPixelsHigh(CGMainDisplayID()) as f64;
        Some((p.x * s, (h_px / s - p.y) * s))
    }
}
