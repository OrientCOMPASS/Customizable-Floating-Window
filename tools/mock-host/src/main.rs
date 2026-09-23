//! mock-host — emulates the MicYou host side of the native plugin C ABI to
//! E2E-test `opss.customizable-floating-window` without building the full
//! Tauri app.
//!
//! It deliberately re-declares `mpl_host_api_t` / `mpl_result_t` **from the
//! host's perspective** (field order per `micyou_plugin_abi.h`, API v2). If
//! the plugin's struct layout drifts, callbacks land on the wrong pointers
//! and this harness fails loudly — which is exactly the regression it guards.
//!
//! Usage:
//! ```text
//! mock-host --plugin <libcustomizable_floating_window.so> --dir <plugin-dir> \
//!           [--seconds 20] [--state <state.json>]
//! MOCK_SNAPSHOT=/path/out.png  → also triggers ui:snapshot at t+7s
//! ```
//! Simulated timeline: t+0 init → t+1.5s `device_connected` → interval ticks
//! every `updateMs` (synthetic sine audio levels) → t+4s config theme switch
//! + `ui:apply-theme` (hot-swap round trip) → t+6s host-side mute flip →
//! t+N deinit. Every host-API call is printed; `set_muted`/`set_monitoring`
//! from the plugin print a `*** CONTROL-PLANE ***` marker (proof a window
//! click travelled helper → plugin → host).

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── ABI mirror (host perspective, API v2) ─────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum mpl_result_t {
    MPL_OK = 0,
    MPL_ERR_NOT_IMPLEMENTED = 1,
    MPL_ERR_INVALID_ARG = 2,
    MPL_ERR_RUNTIME = 3,
    MPL_ERR_BUFFER_TOO_SMALL = 4,
    MPL_ERR_PERMISSION = 5,
}

#[repr(C)]
pub struct mpl_host_api_t {
    pub log: unsafe extern "C" fn(*mut c_void, i32, *const c_char),
    pub get_config:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub set_config: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub emit_event: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub send_message:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, u32) -> mpl_result_t,
    pub audio_state: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub connected_devices: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub ctx: *mut c_void,
    pub play_sound: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub plugin_dir: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub register_hotkey: unsafe extern "C" fn(*mut c_void, *const c_char, *mut u64) -> mpl_result_t,
    pub open_window: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub fs_read:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_char, *mut u32) -> mpl_result_t,
    pub fs_write: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub set_timeout:
        unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t,
    pub clear_timeout: unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t,
    pub http_request: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *mut u64,
    ) -> mpl_result_t,
    pub set_interval:
        unsafe extern "C" fn(*mut c_void, u64, *const c_char, *mut u64) -> mpl_result_t,
    pub clear_interval: unsafe extern "C" fn(*mut c_void, u64) -> mpl_result_t,
    pub open_url: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub notify: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub locale: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub host_info: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub clipboard_read: unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub clipboard_write: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
    pub set_panel_icon:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> mpl_result_t,
    pub set_muted: unsafe extern "C" fn(*mut c_void, u32) -> mpl_result_t,
    pub get_muted: unsafe extern "C" fn(*mut c_void, *mut u32) -> mpl_result_t,
    pub set_monitoring: unsafe extern "C" fn(*mut c_void, u32) -> mpl_result_t,
    pub get_monitoring: unsafe extern "C" fn(*mut c_void, *mut u32) -> mpl_result_t,
    pub get_dsp_settings:
        unsafe extern "C" fn(*mut c_void, *mut c_char, *mut u32) -> mpl_result_t,
    pub set_dsp_settings: unsafe extern "C" fn(*mut c_void, *const c_char) -> mpl_result_t,
}

#[repr(C)]
pub struct mpl_plugin_info_t {
    pub abi_version: u32,
    pub api_version: u32,
    pub id: *const c_char,
    pub version: *const c_char,
}

type InfoFn = unsafe extern "C" fn() -> *const mpl_plugin_info_t;
type InitFn = unsafe extern "C" fn(*const mpl_host_api_t) -> mpl_result_t;
type DeinitFn = unsafe extern "C" fn();
type EventFn = unsafe extern "C" fn(*const c_char, *const c_char) -> mpl_result_t;
type MessageFn = unsafe extern "C" fn(*const c_char, *const c_char, *const u8, u32) -> mpl_result_t;

// ── mock host state ───────────────────────────────────────────────────

struct MockHost {
    dir: String,
    config: Mutex<serde_json::Map<String, serde_json::Value>>,
    state_path: Option<String>,
    muted: AtomicBool,
    monitoring: AtomicBool,
    streaming: AtomicBool,
    start: Instant,
    interval_id: AtomicU64,
    intervals: Mutex<Vec<u64>>,
}

static HOST: OnceLock<Arc<MockHost>> = OnceLock::new();

fn host() -> Arc<MockHost> {
    HOST.get().expect("host initialized").clone()
}

fn elapsed() -> f64 {
    host().start.elapsed().as_secs_f64()
}

fn trace(what: &str) {
    println!("[{:>8.3}s] HOST {what}", elapsed());
}

unsafe fn ctx_of<'a>(p: *mut c_void) -> &'a MockHost {
    unsafe { &*(p as *const MockHost) }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Write a string into the out/out_size buffer, exactly like the real host
/// (NUL-terminated, *out_size = byte count excluding NUL, BUFFER_TOO_SMALL
/// with the required size when the plugin buffer is too small).
unsafe fn write_out(out: *mut c_char, out_size: *mut u32, s: &str) -> mpl_result_t {
    if out.is_null() || out_size.is_null() {
        return mpl_result_t::MPL_ERR_INVALID_ARG;
    }
    let need = s.len() as u32;
    unsafe {
        if *out_size < need + 1 {
            *out_size = need;
            return mpl_result_t::MPL_ERR_BUFFER_TOO_SMALL;
        }
        std::ptr::copy_nonoverlapping(s.as_ptr(), out as *mut u8, s.len());
        *out.add(s.len()) = 0;
        *out_size = need;
    }
    mpl_result_t::MPL_OK
}

// ── host callback shims ───────────────────────────────────────────────

unsafe extern "C" fn h_log(_ctx: *mut c_void, level: i32, msg: *const c_char) {
    let lvl = match level {
        0 => "ERROR",
        1 => "WARN",
        2 => "INFO",
        3 => "DEBUG",
        _ => "TRACE",
    };
    println!("[{:>8.3}s] PLUGIN {lvl}: {}", elapsed(), unsafe { cstr(msg) });
}

unsafe extern "C" fn h_get_config(
    ctx: *mut c_void,
    key: *const c_char,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let k = unsafe { cstr(key) };
    let v = host.config.lock().ok().and_then(|c| c.get(&k).cloned());
    match v {
        Some(v) => unsafe { write_out(out, out_size, &v.to_string()) },
        None => unsafe {
            if !out_size.is_null() {
                *out_size = 0;
            }
            mpl_result_t::MPL_OK
        },
    }
}

unsafe extern "C" fn h_set_config(
    ctx: *mut c_void,
    key: *const c_char,
    value: *const c_char,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let k = unsafe { cstr(key) };
    let v = unsafe { cstr(value) };
    let parsed: serde_json::Value = match serde_json::from_str(&v) {
        Ok(p) => p,
        Err(_) => return mpl_result_t::MPL_ERR_INVALID_ARG,
    };
    if k != "status" {
        // status is written ~1 Hz; keep the trace readable
        trace(&format!("set_config {k} = {v}"));
    }
    if let Ok(mut c) = host.config.lock() {
        c.insert(k, parsed);
        if let Some(path) = &host.state_path {
            if let Ok(f) = std::fs::File::create(path) {
                let _ = serde_json::to_writer_pretty(f, &*c);
            }
        }
    }
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_emit_event(
    _ctx: *mut c_void,
    topic: *const c_char,
    payload: *const c_char,
) -> mpl_result_t {
    trace(&format!("emit_event {} {}", unsafe { cstr(topic) }, unsafe { cstr(payload) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_send_message(
    _ctx: *mut c_void,
    target: *const c_char,
    _payload: *const u8,
    len: u32,
) -> mpl_result_t {
    trace(&format!("send_message {} ({len} bytes)", unsafe { cstr(target) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_audio_state(
    ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let t = host.start.elapsed().as_secs_f64();
    // Synthetic voice-like level: bursts of a sine, with pauses.
    let env = if (t % 4.0) < 2.5 { 1.0 } else { 0.05 };
    let level = ((t * 6.0).sin().abs() * 0.55 + 0.05) * env;
    let json = serde_json::json!({
        "streaming": host.streaming.load(Ordering::Relaxed),
        "sampleRate": 48000,
        "channels": 1,
        "inputLevel": (level * 1000.0).round() / 1000.0,
        "processedLevel": (level * 0.9 * 1000.0).round() / 1000.0,
        "queuedMs": 12.5,
        "muted": host.muted.load(Ordering::Relaxed),
    });
    unsafe { write_out(out, out_size, &json.to_string()) }
}

unsafe extern "C" fn h_connected_devices(
    ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let devs = if host.streaming.load(Ordering::Relaxed) {
        serde_json::json!([{ "mode": "wifi", "label": "MockYou Mobile", "audioActive": true }])
    } else {
        serde_json::json!([])
    };
    unsafe { write_out(out, out_size, &devs.to_string()) }
}

unsafe extern "C" fn h_play_sound(_ctx: *mut c_void, path: *const c_char) -> mpl_result_t {
    trace(&format!("play_sound {}", unsafe { cstr(path) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_plugin_dir(
    ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    unsafe { write_out(out, out_size, &host.dir) }
}

unsafe extern "C" fn h_register_hotkey(
    _ctx: *mut c_void,
    sc: *const c_char,
    out_id: *mut u64,
) -> mpl_result_t {
    trace(&format!("register_hotkey {}", unsafe { cstr(sc) }));
    if !out_id.is_null() {
        unsafe { *out_id = 1 };
    }
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_open_window(_ctx: *mut c_void, panel: *const c_char) -> mpl_result_t {
    trace(&format!("open_window {}", unsafe { cstr(panel) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_fs_read(
    ctx: *mut c_void,
    path: *const c_char,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let p = std::path::Path::new(&host.dir).join(unsafe { cstr(path) });
    match std::fs::read_to_string(p) {
        Ok(s) => unsafe { write_out(out, out_size, &s) },
        Err(_) => mpl_result_t::MPL_ERR_RUNTIME,
    }
}

unsafe extern "C" fn h_fs_write(
    ctx: *mut c_void,
    path: *const c_char,
    content: *const c_char,
) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    let p = std::path::Path::new(&host.dir).join(unsafe { cstr(path) });
    trace(&format!("fs_write {}", p.display()));
    match std::fs::write(p, unsafe { cstr(content) }) {
        Ok(()) => mpl_result_t::MPL_OK,
        Err(_) => mpl_result_t::MPL_ERR_RUNTIME,
    }
}

unsafe extern "C" fn h_set_timeout(
    _ctx: *mut c_void,
    _ms: u64,
    _payload: *const c_char,
    out_id: *mut u64,
) -> mpl_result_t {
    if !out_id.is_null() {
        unsafe { *out_id = 1 };
    }
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_clear_timeout(_ctx: *mut c_void, _id: u64) -> mpl_result_t {
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_http_request(
    _ctx: *mut c_void,
    _m: *const c_char,
    _u: *const c_char,
    _h: *const c_char,
    _b: *const c_char,
    _out_id: *mut u64,
) -> mpl_result_t {
    mpl_result_t::MPL_ERR_NOT_IMPLEMENTED
}

unsafe extern "C" fn h_set_interval(
    _ctx: *mut c_void,
    ms: u64,
    payload: *const c_char,
    out_id: *mut u64,
) -> mpl_result_t {
    let host = host();
    let id = host.interval_id.fetch_add(1, Ordering::Relaxed) + 1;
    let pay = unsafe { cstr(payload) };
    trace(&format!("set_interval {ms}ms payload={pay:?} → id {id}"));
    if !out_id.is_null() {
        unsafe { *out_id = id };
    }
    if let Ok(mut v) = host.intervals.lock() {
        v.push(id);
    }
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_clear_interval(_ctx: *mut c_void, id: u64) -> mpl_result_t {
    trace(&format!("clear_interval {id}"));
    if let Ok(mut v) = host().intervals.lock() {
        v.retain(|x| *x != id);
    }
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_open_url(_ctx: *mut c_void, url: *const c_char) -> mpl_result_t {
    trace(&format!("open_url {}", unsafe { cstr(url) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_notify(
    _ctx: *mut c_void,
    title: *const c_char,
    body: *const c_char,
) -> mpl_result_t {
    trace(&format!(
        "notify {:?} {:?}",
        unsafe { cstr(title) },
        unsafe { cstr(body) }
    ));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_locale(
    _ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    unsafe { write_out(out, out_size, "zh-CN") }
}

unsafe extern "C" fn h_host_info(
    _ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    let j = serde_json::json!({ "name": "mock-host", "version": "0.0.0", "apiVersion": 2 });
    unsafe { write_out(out, out_size, &j.to_string()) }
}

unsafe extern "C" fn h_clipboard_read(
    _ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    unsafe { write_out(out, out_size, "") }
}

unsafe extern "C" fn h_clipboard_write(_ctx: *mut c_void, text: *const c_char) -> mpl_result_t {
    trace(&format!("clipboard_write {:?}", unsafe { cstr(text) }));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_set_panel_icon(
    _ctx: *mut c_void,
    panel: *const c_char,
    icon: *const c_char,
) -> mpl_result_t {
    trace(&format!(
        "set_panel_icon {} {}",
        unsafe { cstr(panel) },
        unsafe { cstr(icon) }
    ));
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_set_muted(ctx: *mut c_void, muted: u32) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    host.muted.store(muted != 0, Ordering::Relaxed);
    println!(
        "[{:>8.3}s] HOST *** CONTROL-PLANE set_muted({}) ***",
        elapsed(),
        muted != 0
    );
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_get_muted(ctx: *mut c_void, out: *mut u32) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    if out.is_null() {
        return mpl_result_t::MPL_ERR_INVALID_ARG;
    }
    unsafe { *out = host.muted.load(Ordering::Relaxed) as u32 };
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_set_monitoring(ctx: *mut c_void, on: u32) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    host.monitoring.store(on != 0, Ordering::Relaxed);
    println!(
        "[{:>8.3}s] HOST *** CONTROL-PLANE set_monitoring({}) ***",
        elapsed(),
        on != 0
    );
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_get_monitoring(ctx: *mut c_void, out: *mut u32) -> mpl_result_t {
    let host = unsafe { ctx_of(ctx) };
    if out.is_null() {
        return mpl_result_t::MPL_ERR_INVALID_ARG;
    }
    unsafe { *out = host.monitoring.load(Ordering::Relaxed) as u32 };
    mpl_result_t::MPL_OK
}

unsafe extern "C" fn h_get_dsp_settings(
    _ctx: *mut c_void,
    out: *mut c_char,
    out_size: *mut u32,
) -> mpl_result_t {
    unsafe { write_out(out, out_size, "{}") }
}

unsafe extern "C" fn h_set_dsp_settings(_ctx: *mut c_void, _json: *const c_char) -> mpl_result_t {
    mpl_result_t::MPL_OK
}

// ── main ──────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| -> Option<String> {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let plugin_path = get("--plugin").expect("--plugin <path to cdylib>");
    let dir = get("--dir").expect("--dir <plugin install dir>");
    let seconds: u64 = get("--seconds").and_then(|s| s.parse().ok()).unwrap_or(20);
    let state_path = get("--state");

    let mut config = serde_json::Map::new();
    config.insert("theme".into(), serde_json::json!("ring.slint"));
    config.insert("visible".into(), serde_json::json!(true));
    config.insert("updateMs".into(), serde_json::json!(100));
    if let Some(p) = &state_path {
        if let Ok(f) = std::fs::File::open(p) {
            if let Ok(v) =
                serde_json::from_reader::<_, serde_json::Map<String, serde_json::Value>>(f)
            {
                config = v;
            }
        }
    }

    let host_arc = Arc::new(MockHost {
        dir: dir.clone(),
        config: Mutex::new(config),
        state_path,
        muted: AtomicBool::new(false),
        monitoring: AtomicBool::new(false),
        streaming: AtomicBool::new(true),
        start: Instant::now(),
        interval_id: AtomicU64::new(0),
        intervals: Mutex::new(Vec::new()),
    });
    HOST.set(host_arc.clone()).ok();
    let ctx_ptr = Arc::into_raw(host_arc.clone()) as *mut c_void;

    let table = mpl_host_api_t {
        log: h_log,
        get_config: h_get_config,
        set_config: h_set_config,
        emit_event: h_emit_event,
        send_message: h_send_message,
        audio_state: h_audio_state,
        connected_devices: h_connected_devices,
        ctx: ctx_ptr,
        play_sound: h_play_sound,
        plugin_dir: h_plugin_dir,
        register_hotkey: h_register_hotkey,
        open_window: h_open_window,
        fs_read: h_fs_read,
        fs_write: h_fs_write,
        set_timeout: h_set_timeout,
        clear_timeout: h_clear_timeout,
        http_request: h_http_request,
        set_interval: h_set_interval,
        clear_interval: h_clear_interval,
        open_url: h_open_url,
        notify: h_notify,
        locale: h_locale,
        host_info: h_host_info,
        clipboard_read: h_clipboard_read,
        clipboard_write: h_clipboard_write,
        set_panel_icon: h_set_panel_icon,
        set_muted: h_set_muted,
        get_muted: h_get_muted,
        set_monitoring: h_set_monitoring,
        get_monitoring: h_get_monitoring,
        get_dsp_settings: h_get_dsp_settings,
        set_dsp_settings: h_set_dsp_settings,
    };

    // ── load the plugin ──
    let lib = unsafe { libloading::Library::new(&plugin_path) }
        .unwrap_or_else(|e| panic!("load {plugin_path}: {e}"));
    let f_info: libloading::Symbol<InfoFn> =
        unsafe { lib.get(b"micyou_plugin_info\0") }.expect("micyou_plugin_info");
    let f_init: libloading::Symbol<InitFn> =
        unsafe { lib.get(b"micyou_plugin_init\0") }.expect("micyou_plugin_init");
    let f_deinit: libloading::Symbol<DeinitFn> =
        unsafe { lib.get(b"micyou_plugin_deinit\0") }.expect("micyou_plugin_deinit");
    let f_event: EventFn =
        *unsafe { lib.get::<EventFn>(b"micyou_plugin_handle_event\0") }
            .expect("micyou_plugin_handle_event");
    let f_msg: MessageFn =
        *unsafe { lib.get::<MessageFn>(b"micyou_plugin_handle_message\0") }
            .expect("micyou_plugin_handle_message");

    let info = unsafe { f_info() };
    assert!(!info.is_null(), "plugin info NULL");
    let info = unsafe { &*info };
    println!(
        "plugin info: abi={} api={} id={} version={}",
        info.abi_version,
        info.api_version,
        unsafe { cstr(info.id) },
        unsafe { cstr(info.version) }
    );
    assert_eq!(info.abi_version, 1, "abi version");
    assert_eq!(info.api_version, 2, "api version");
    assert_eq!(unsafe { cstr(info.id) }, "opss.customizable-floating-window");

    trace("micyou_plugin_init …");
    let code = unsafe { f_init(&table) };
    assert_eq!(code as i32, 0, "init failed: {code:?}");

    // ── interval pump thread (mirrors the real host timer behavior: the
    //    timer thread calls handle_message directly) ──
    std::thread::spawn(move || {
        let mut last: std::collections::HashMap<u64, Instant> = std::collections::HashMap::new();
        loop {
        std::thread::sleep(Duration::from_millis(10));
        let h = host();
        let ids: Vec<u64> = h.intervals.lock().map(|v| v.clone()).unwrap_or_default();
        for id in ids {
            // interval ms comes from what the plugin registered; the mock
            // simply pumps every 50ms and lets the plugin's own cadence
            // logic tolerate it — closer to reality: read back updateMs.
            let ms = h
                .config
                .lock()
                .ok()
                .and_then(|c| c.get("updateMs").and_then(|v| v.as_u64()))
                .unwrap_or(100)
                .max(20);
            let due = {
                let e = last.entry(id).or_insert_with(Instant::now);
                if e.elapsed() >= Duration::from_millis(ms) {
                    *e = Instant::now();
                    true
                } else {
                    false
                }
            };
            if !due {
                continue;
            }
            let json = serde_json::json!({ "interval": id, "payload": "cfw" }).to_string();
            let topic = CString::new("interval:tick").unwrap();
            let src = CString::new("host").unwrap();
            unsafe {
                f_msg(
                    src.as_ptr(),
                    topic.as_ptr(),
                    json.as_ptr(),
                    json.len() as u32,
                )
            };
        }
        }
    });

    // ── simulated event timeline ──
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        let t = CString::new("device_connected").unwrap();
        let j = CString::new(
            r#"{"type":"device_connected","mode":"wifi","label":"MockYou Mobile"}"#,
        )
        .unwrap();
        println!("[{:>8.3}s] HOST → plugin event device_connected", elapsed());
        unsafe { f_event(t.as_ptr(), j.as_ptr()) };
    });

    std::thread::spawn(move || {
        // t+4s: panel flow — write theme config, then trigger ui:apply-theme
        std::thread::sleep(Duration::from_secs(4));
        if let Ok(mut c) = host().config.lock() {
            c.insert("theme".into(), serde_json::json!("pill.slint"));
        }
        let payload = br#"{"action":"apply-theme"}"#;
        let topic = CString::new("ui:apply-theme").unwrap();
        let src = CString::new("ui").unwrap();
        println!("[{:>8.3}s] HOST → plugin message ui:apply-theme", elapsed());
        unsafe { f_msg(src.as_ptr(), topic.as_ptr(), payload.as_ptr(), payload.len() as u32) };

        // t+7s: optional snapshot (visual verification under xvfb)
                // t+5s: optional WDIS broadcast injection (MOCK_WDIS="text")
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(text) = std::env::var("MOCK_WDIS") {
            let mut payload = b"WDIS".to_vec();
            payload.extend_from_slice(&1_700_000_000_000i64.to_le_bytes());
            payload.extend_from_slice(&1_700_000_005_000i64.to_le_bytes());
            payload.extend_from_slice(text.as_bytes());
            let topic = CString::new("broadcast").unwrap();
            let src = CString::new("opss.whatdidisay").unwrap();
            println!("[{:>8.3}s] HOST -> plugin message WDIS broadcast", elapsed());
            unsafe {
                f_msg(
                    src.as_ptr(),
                    topic.as_ptr(),
                    payload.as_ptr(),
                    payload.len() as u32,
                )
            };
        }

std::thread::sleep(Duration::from_secs(3));
        if let Ok(p) = std::env::var("MOCK_SNAPSHOT") {
            let payload = serde_json::json!({ "action": "snapshot", "path": p }).to_string();
            let topic = CString::new("ui:snapshot").unwrap();
            let src = CString::new("ui").unwrap();
            println!("[{:>8.3}s] HOST → plugin message ui:snapshot {p}", elapsed());
            unsafe {
                f_msg(
                    src.as_ptr(),
                    topic.as_ptr(),
                    payload.as_ptr(),
                    payload.len() as u32,
                )
            };
        }

        // t+9s: host-side mute flip (simulates GUI toggle -> mute_changed)
        std::thread::sleep(Duration::from_secs(4));
        host().muted.store(true, Ordering::Relaxed);
        let t = CString::new("mute_changed").unwrap();
        let j = CString::new(r#"{"type":"mute_changed","muted":true}"#).unwrap();
        println!("[{:>8.3}s] HOST → plugin event mute_changed(true)", elapsed());
        unsafe { f_event(t.as_ptr(), j.as_ptr()) };
    });

    // ── run, then tear down ──
    std::thread::sleep(Duration::from_secs(seconds));
    trace("micyou_plugin_deinit …");
    unsafe { f_deinit() };

    // Stop the interval pump.
    if let Ok(mut v) = host().intervals.lock() {
        v.clear();
    }
    std::thread::sleep(Duration::from_millis(100));

    drop(f_deinit);
    drop(f_init);
    drop(f_info);
    drop(lib);
    let _ = (f_msg, f_event); // raw fn pointers: Copy, kept alive by lib
    unsafe { drop(Arc::from_raw(ctx_ptr as *const MockHost)) };

    println!("[{:>8.3}s] mock-host finished OK", elapsed());
}
