//! Platform window plumbing that Slint does not model:
//!
//! 1. **Skip taskbar** — Slint has no `skip-taskbar` window property (checked
//!    the whole 1.18 source tree). We post-process the native window through
//!    `Window::window_handle()` (feature `raw-window-handle-06`):
//!      * Windows: add `WS_EX_TOOLWINDOW` to the extended style → no taskbar
//!        button, no Alt-Tab entry.
//!      * X11: append `_NET_WM_STATE_SKIP_TASKBAR` + `_NET_WM_STATE_SKIP_PAGER`
//!        to `_NET_WM_STATE` (EWMH — honored by GNOME Shell, KDE, XFCE, …).
//!      * Wayland: no client-side protocol exists; visibility in any bar is
//!        compositor policy. Nothing we can do (documented limitation).
//!      * macOS: there is no taskbar; the equivalent is the Dock/app switcher.
//!        We set `NSApplicationActivationPolicyAccessory` on the *helper
//!        process* (see [`macos_set_accessory_policy`]) → no Dock icon, no
//!        Cmd-Tab entry, windows still interactive.
//! 2. **Primary screen size** — needed for the default top-right placement
//!    (Slint exposes no monitor API). Same FFI story per platform.
//!
//! All FFI is raw `extern` declarations (no platform crates beyond `x11-dl`
//! on Linux, which dlopens libX11 so there is no link-time dependency and
//! headless builds keep working).

use slint::Window;

/// Description of what was applied (for the helper log).
pub fn apply_skip_taskbar(window: &Window) -> &'static str {
    #[cfg(windows)]
    return windows_skip_taskbar(window);
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    return linux_skip_taskbar(window);
    #[cfg(target_os = "macos")]
    {
        let _ = window; // policy is process-wide, applied in main()/retry chain
        return "macos-accessory-policy";
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = window;
        "unsupported-platform"
    }
}

/// Primary screen size in physical pixels (fallback: None → keep WM placement).
pub fn screen_size() -> Option<(i32, i32)> {
    #[cfg(windows)]
    {
        unsafe {
            let w = GetSystemMetrics(SM_CXSCREEN);
            let h = GetSystemMetrics(SM_CYSCREEN);
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
        // Under Wayland there is no reliable client-side screen geometry
        // (and winit refuses absolute positioning anyway) → let the
        // compositor place the window.
        if is_wayland() {
            return None;
        }
        x11_screen_size()
    }
}

/// Whether absolute window positioning works on this session.
/// False only for Wayland (winit reports dummy (0,0) positions there and
/// refuses `set_position`); true on Windows/X11/macOS.
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

/// macOS: make the helper an "accessory" app (no Dock icon / app menu).
/// Called once at startup *and* again after the window shows, because winit
/// applies its default (Regular) activation policy when the event loop
/// starts — whichever call lands last wins, so we re-assert after show.
#[cfg(target_os = "macos")]
pub fn macos_set_accessory_policy() {
    use std::ffi::CString;
    const NS_APPLICATION_ACTIVATION_POLICY_ACCESSORY: isize = 1;
    unsafe {
        let cls_name = match CString::new("NSApplication") {
            Ok(c) => c,
            Err(_) => return,
        };
        let sel_shared = match CString::new("sharedApplication") {
            Ok(c) => c,
            Err(_) => return,
        };
        let sel_policy = match CString::new("setActivationPolicy:") {
            Ok(c) => c,
            Err(_) => return,
        };
        let cls = objc_getClass(cls_name.as_ptr());
        if cls.is_null() {
            return;
        }
        let sel_shared = sel_registerName(sel_shared.as_ptr());
        let sel_policy = sel_registerName(sel_policy.as_ptr());
        if sel_shared.is_null() || sel_policy.is_null() {
            return;
        }
        // fn-item → ptr cast, then transmute to the typed signature
        // (objc_msgSend is untyped by design; per-signature pointers are the
        // standard raw-ObjC pattern).
        let msg0: MsgSend0 = std::mem::transmute(objc_msgSend as *const c_void);
        let app = msg0(cls, sel_shared);
        if app.is_null() {
            return;
        }
        let msg1: MsgSendI = std::mem::transmute(objc_msgSend as *const c_void);
        msg1(app, sel_policy, NS_APPLICATION_ACTIVATION_POLICY_ACCESSORY);
    }
}

#[cfg(not(target_os = "macos"))]
pub fn macos_set_accessory_policy() {}

// ─────────────────────────── Windows ───────────────────────────

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    pub type HWND = *mut c_void;
    pub const GWL_EXSTYLE: i32 = -20;
    pub const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
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
    }
}

#[cfg(windows)]
use win::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

#[cfg(windows)]
fn windows_skip_taskbar(window: &Window) -> &'static str {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    let wh = window.window_handle();
    let Ok(handle) = wh.window_handle() else {
        return "no-handle";
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return "not-win32";
    };
    unsafe {
        let hwnd = h.hwnd.get() as win::HWND;
        let ex = win::GetWindowLongPtrW(hwnd, win::GWL_EXSTYLE);
        win::SetWindowLongPtrW(hwnd, win::GWL_EXSTYLE, ex | win::WS_EX_TOOLWINDOW);
        win::SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            win::SWP_NOMOVE
                | win::SWP_NOSIZE
                | win::SWP_NOZORDER
                | win::SWP_NOACTIVATE
                | win::SWP_FRAMECHANGED,
        );
    }
    "ws_ex_toolwindow"
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

// ─────────────────────────── Linux/X11 ─────────────────────────

#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn x11_screen_size() -> Option<(i32, i32)> {
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

#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn linux_skip_taskbar(window: &Window) -> &'static str {
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

        // Read the existing _NET_WM_STATE (XA_ATOM = 4) so we append instead
        // of clobbering states the WM already set (e.g. ABOVE for always-on-top).
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
            0, // False
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
        // PropModeReplace = 0
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
            0, // False: create if missing
        )
    }
}
