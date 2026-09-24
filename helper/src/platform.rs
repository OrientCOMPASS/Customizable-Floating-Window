//! Platform window plumbing that Slint does not model.
//!
//! # Root-cause lesson (round-4 regression, see docs/TECHNICAL.md §7.8)
//! winit owns `GWL_STYLE`/`GWL_EXSTYLE` on Windows: it recomputes them from its
//! internal state whenever visibility/decorations change (`WindowFlags::apply`).
//! Post-creation mutations of those bits (WS_EX_TOOLWINDOW, caption stripping,
//! `ShowWindow` hide/show cycles, blur-behind re-application) get **wiped** or
//! desync winit's redraw bookkeeping — observed symptoms: grey window that
//! only repaints interacted components (winit believed it was hidden), caption
//! re-appearing, taskbar button returning (winit re-applied WS_EX_APPWINDOW).
//!
//! Therefore this module only touches attributes winit does NOT manage:
//!   * Windows: window **ownership** (`GWLP_HWNDPARENT` → invisible owner).
//!     Owned windows get no taskbar button and no Alt-Tab entry; the shell
//!     groups them under the owner, which is never shown.
//!   * X11: `_NET_WM_STATE_SKIP_TASKBAR/_NET_WM_STATE_SKIP_PAGER` (WM-managed,
//!     persists) + `_MOTIF_WM_HINTS` decorations=0 (CSD shells).
//!   * macOS: process-level `NSApplicationActivationPolicyAccessory` (outside
//!     window management) → no Dock icon / Cmd-Tab entry.
//!   * Wayland: no client-side protocol → no-op + log.
//! Window decoration/transparency themselves are honored by winit **at window
//! creation** from the `.slint` Window item (`no-frame`, `background`) — the
//! X11 depth-32 ARGB measurement proves the creation path; we never fight it.

use slint::Window;

/// Apply the platform's taskbar/alt-tab/dock exclusion. Idempotent; called
/// from a retry chain after show (the raw handle only exists once mapped).
pub fn apply_skip_taskbar(window: &Window) -> &'static str {
    #[cfg(windows)]
    return windows_skip_taskbar(window);
    #[cfg(all(target_os = "linux", not(target_os = "android")))]
    return linux_skip_taskbar(window);
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        macos_set_accessory_policy();
        return "macos-accessory-policy";
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = window;
        "unsupported-platform"
    }
}

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
        // Under Wayland there is no reliable client-side screen geometry
        // (and winit refuses absolute positioning anyway).
        if is_wayland() {
            return None;
        }
        x11_screen_size()
    }
}

/// Whether absolute window positioning works (false on Wayland).
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

// ─────────────────────────── Windows ───────────────────────────

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    pub type HWND = *mut c_void;
    pub const GWL_HWNDPARENT: i32 = -21;
    pub const WS_POPUP: u32 = 0x8000_0000;
    pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
    pub const SM_CXSCREEN: i32 = 0;
    pub const SM_CYSCREEN: i32 = 1;

    #[link(name = "user32")]
    extern "system" {
        pub fn GetSystemMetrics(index: i32) -> i32;
        pub fn SetWindowLongPtrW(hwnd: HWND, index: i32, new_long: isize) -> isize;
        pub fn CreateWindowExW(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
            dw_style: u32,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            hwnd_parent: HWND,
            h_menu: *mut c_void,
            h_instance: *mut c_void,
            lp_param: *mut c_void,
        ) -> HWND;
    }
}

#[cfg(windows)]
fn windows_skip_taskbar(window: &Window) -> &'static str {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use std::sync::atomic::{AtomicIsize, Ordering};
    static OWNER: AtomicIsize = AtomicIsize::new(0);
    let wh = window.window_handle();
    let Ok(handle) = wh.window_handle() else {
        return "no-handle";
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return "not-win32";
    };
    unsafe {
        let mut owner = OWNER.load(Ordering::Relaxed);
        if owner == 0 {
            // Invisible, never-shown tool popup as owner (process lifetime).
            let class: &[u16] = &[
                'S' as u16, 'T' as u16, 'A' as u16, 'T' as u16, 'I' as u16, 'C' as u16, 0,
            ];
            let name: &[u16] = &[0];
            let hwnd = win::CreateWindowExW(
                win::WS_EX_TOOLWINDOW,
                class.as_ptr(),
                name.as_ptr(),
                win::WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if !hwnd.is_null() {
                owner = hwnd as isize;
                OWNER.store(owner, Ordering::Relaxed);
            }
        }
        if owner != 0 {
            win::SetWindowLongPtrW(h.hwnd.get() as win::HWND, win::GWL_HWNDPARENT, owner);
            return "owned-window";
        }
        "owner-create-failed"
    }
}

// ─────────────────────────── Linux / X11 ───────────────────────────

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

        // Read the existing _NET_WM_STATE (XA_ATOM = 4) so we append instead of
        // clobbering states the WM already set (e.g. ABOVE for always-on-top).
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
        (xlib.XChangeProperty)(
            disp,
            win_id,
            state_atom,
            XA_ATOM,
            32,
            0, // PropModeReplace
            atoms.as_ptr() as *const std::os::raw::c_uchar,
            atoms.len() as std::os::raw::c_int,
        );

        // Belt & suspenders for WMs that add frames/CSDs anyway (some CJK
        // desktop shells): MOTIF_WM_HINTS with decorations = 0.
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
                flags: 1 << 1, // MWM_HINTS_DECORATIONS
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
            0, // False: create if missing
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

/// macOS: hide from Dock / Cmd-Tab by making the helper an *accessory* app.
/// Asserted at startup and re-asserted by the post-show retry chain, because
/// winit resets the activation policy to Regular when it initializes
/// NSApplication. Process-level only — never touches window state.
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

// ─────────────────────────── global cursor ───────────────────────────

/// Global cursor position in **physical pixels**, for the helper-side drag
/// tracker (drag protocol v3). Returns None where unavailable (Wayland).
///
/// Using global coordinates removes the window-relative feedback loop
/// entirely: the cursor frame is independent of the window we move, so
/// `P = P0 + (C − C0)` is exact by construction (system-drag quality
/// without a WM grab, and the client keeps every button event).
#[cfg(windows)]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    extern "system" {
        fn GetCursorPos(lp: *mut Point) -> i32;
    }
    unsafe {
        let mut p = Point { x: 0, y: 0 };
        if GetCursorPos(&mut p) != 0 {
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
    let Ok(xlib) = xlib::Xlib::open() else {
        return None;
    };
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
            disp,
            root,
            &mut root_ret,
            &mut child,
            &mut rx,
            &mut ry,
            &mut wx,
            &mut wy,
            &mut mask,
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
        let p = msg(cls, sel); // points, origin bottom-left
        let s = f64::from(scale.max(0.5));
        let h_px = CGDisplayPixelsHigh(CGMainDisplayID()) as f64;
        // flip to top-left origin and convert points → physical pixels
        Some((p.x * s, (h_px / s - p.y) * s))
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn global_cursor(_scale: f32) -> Option<(f64, f64)> {
    None
}
