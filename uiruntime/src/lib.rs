//! Slint UI runtime for the customizable floating window.
//!
//! Round-5 architecture (rolls back to the user-verified "first edition"
//! design): the Slint event loop runs on a **plugin-owned thread** inside the
//! plugin process on Windows/Linux (no helper subprocess), communicating with
//! the host-dispatched threads through two unbounded channels:
//!
//! ```text
//! host threads (init/tick/events)  ──Cmd──▶  UI thread (Slint loop)
//!                              ◀──Ev───
//! ```
//!
//! macOS is the exception: winit/Slint require the process main thread for the
//! event loop, which the Tauri host owns — there the same runtime runs as the
//! `floating_helper` subprocess main loop (stdin/stdout carry Cmd/Ev).
//!
//! The 33 ms UI timer drains the Cmd channel (user's reference pattern), so no
//! `invoke_from_event_loop` hop is needed for state updates.

pub mod arc;
pub mod platform;
pub mod theme;

use cfw_protocol::{Cmd, Ev};
use slint::{ComponentHandle as _, PhysicalPosition, Timer, TimerMode};
use slint_interpreter::Value;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use theme::LoadedTheme;

/// Everything the UI thread mutates. Single-threaded (the Slint loop thread).
struct Ui {
    theme: Option<LoadedTheme>,
    timers: Vec<Timer>,
    smooth: f64,
    smooth_target: f64,
    last_applied_smooth: f64,
    phase: f64,
    // platform hardening (cfg-gated readers; fields exist everywhere)
    #[allow(dead_code)]
    win: platform::WinHardening,
    #[allow(dead_code)]
    x11_flags_done: bool,
    // drag (global-cursor incremental tracking)
    dragging: bool,
    drag_last: Option<(f64, f64)>,
    drag_scale: f32,
    drag_events: u32,
    win_tick: u32,
    #[allow(dead_code)] // read only under cfg(target_os = "macos")
    mac_policy_ticks: u32,
    // misc
    last_pos: Option<PhysicalPosition>,
    wdis_timer: Option<Timer>,
}

impl Ui {
    fn new() -> Self {
        Self {
            theme: None,
            timers: Vec::new(),
            smooth: 0.0,
            smooth_target: 0.0,
            last_applied_smooth: f64::NAN,
            phase: 0.0,
            dragging: false,
            drag_last: None,
            drag_scale: 1.0,
            drag_events: 0,
            win: platform::WinHardening::default(),
            win_tick: 0,
            x11_flags_done: false,
            mac_policy_ticks: 0,
            last_pos: None,
            wdis_timer: None,
        }
    }
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::new());
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> R {
    UI.with(|u| f(&mut u.borrow_mut()))
}

fn with_theme<R>(f: impl FnOnce(&LoadedTheme) -> R) -> Option<R> {
    UI.with(|u| u.borrow().theme.as_ref().map(f))
}

fn with_window<R>(f: impl FnOnce(&slint::Window) -> R) -> Option<R> {
    with_theme(|t| f(t.instance.window()))
}

fn emit(tx: &Sender<Ev>, ev: Ev) {
    let _ = tx.send(ev);
}

/// Run the floating-window UI on the **calling thread** until `Cmd::Quit`
/// or the window hides (Slint quits the loop when the last window hides).
pub fn run(rx: Receiver<Cmd>, tx: Sender<Ev>, theme_path: PathBuf, pos: Option<(i32, i32)>) {
    #[cfg(target_os = "macos")]
    platform::macos_set_accessory_policy();

    // initial theme (bundled fallback keeps a window alive on bad themes)
    let (res, err) = theme::load_with_fallback(&theme_path);
    let theme = match res {
        Ok(t) => t,
        Err(e) => {
            emit(&tx, Ev::ThemeError {
                message: format!("fatal: cannot load any theme: {e}"),
            });
            return;
        }
    };
    if let Some(msg) = err {
        emit(&tx, Ev::ThemeError { message: msg });
    }
    activate_theme(theme, &tx, pos, false);

    // ── 33 ms driver: drain Cmd channel + smoothing + platform hardening ──
    let driver = Timer::default();
    let rx_ref = rx;
    let tx_clone = tx.clone();
    driver.start(TimerMode::Repeated, Duration::from_millis(33), move || {
        // 1) drain host commands (user's reference pattern)
        while let Ok(cmd) = rx_ref.try_recv() {
            handle_cmd(cmd, &tx_clone);
        }
        // 2) level smoothing + animated path properties
        with_ui(|u| {
            let streaming = u
                .theme
                .as_ref()
                .map(|t| {
                    t.instance
                        .get_property("streaming")
                        .map(|v| matches!(v, Value::Bool(true)))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let muted = u
                .theme
                .as_ref()
                .map(|t| {
                    t.instance
                        .get_property("muted")
                        .map(|v| matches!(v, Value::Bool(true)))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let target = u.smooth_target;
            u.smooth += (target - u.smooth) * 0.30;
            if (target - u.smooth).abs() < 0.004 {
                u.smooth = target;
            }
            let animating = streaming && !muted && u.smooth > 0.02;
            if animating {
                u.phase = (u.phase + 6.0) % 360.0;
            }
            if !animating && (u.smooth - u.last_applied_smooth).abs() < 0.0015 {
                return;
            }
            u.last_applied_smooth = u.smooth;
            if let Some(t) = u.theme.as_ref() {
                theme::apply_smooth(t, u.smooth, u.phase);
            }
        });
        // 3) platform window hardening (Windows recipe / X11 flags / mac policy)
        with_ui(|u| {
            u.win_tick += 1;
            #[cfg(windows)]
            {
                let tick = u.win_tick;
                if let Some(t) = u.theme.as_ref() {
                    let w = t.instance.window();
                    if let Some(desc) = platform::windows_harden(w, &mut u.win, tick) {
                        emit(&tx_clone, Ev::Log {
                            message: format!("window flags applied: {desc}"),
                        });
                    }
                }
            }
            #[cfg(all(target_os = "linux", not(target_os = "android")))]
            {
                let tick = u.win_tick;
                if !u.x11_flags_done && tick >= 4 {
                    if let Some(t) = u.theme.as_ref() {
                        let w = t.instance.window();
                        let r = platform::linux_skip_taskbar(w);
                        u.x11_flags_done = true;
                        emit(&tx_clone, Ev::Log {
                            message: format!("window flags applied: {r}"),
                        });
                    }
                }
            }
            #[cfg(target_os = "macos")]
            if u.mac_policy_ticks < 10 {
                u.mac_policy_ticks += 1;
                platform::macos_set_accessory_policy();
            }
        });
    });
    with_ui(|u| u.timers.push(driver));

    // ── 1 s position poll (WindowMoveArea / WM moves) ──
    let poll = Timer::default();
    let tx_poll = tx.clone();
    poll.start(TimerMode::Repeated, Duration::from_millis(1000), move || {
        if !platform::position_api_usable() {
            return;
        }
        with_ui(|u| {
            if u.dragging {
                return;
            }
            let Some(t) = u.theme.as_ref() else { return };
            let cur = t.instance.window().position();
            if cur.x == 0 && cur.y == 0 {
                return;
            }
            let changed = match u.last_pos {
                Some(p) => (p.x - cur.x).abs() > 2 || (p.y - cur.y).abs() > 2,
                None => true,
            };
            u.last_pos = Some(cur);
            if changed {
                emit(&tx_poll, Ev::Moved { x: cur.x, y: cur.y });
            }
        });
    });
    with_ui(|u| u.timers.push(poll));

    let _ = slint::run_event_loop();
    emit(&tx, Ev::Bye);
}

/// Hot-swap or first-load a theme instance; (re)wires callbacks & timers.
fn activate_theme(theme: LoadedTheme, tx: &Sender<Ev>, pos: Option<(i32, i32)>, reloaded: bool) {
    let old_pos = with_ui(|u| {
        u.theme
            .as_ref()
            .and_then(|t| {
                platform::position_api_usable().then(|| t.instance.window().position())
            })
    });
    attach_callbacks(&theme, tx);
    if theme.instance.show().is_err() {
        emit(tx, Ev::ThemeError {
            message: "cannot show window".into(),
        });
        return;
    }
    let name = theme.name.clone();
    with_ui(|u| {
        if let Some(old) = u.theme.take() {
            let _ = old.instance.window().hide();
        }
        u.theme = Some(theme);
        u.timers.clear();
        u.smooth = 0.0;
        u.last_applied_smooth = f64::NAN;
        u.phase = 0.0;
        u.dragging = false;
        u.drag_last = None;
        u.drag_events = 0;
        u.last_pos = None;
    });
    place_window(old_pos.or(pos.map(|(x, y)| PhysicalPosition::new(x, y))));
    emit(tx, Ev::Ready { theme: name, reloaded });
}

fn place_window(pos: Option<PhysicalPosition>) {
    let applied = with_window(|win| {
        let target = match pos {
            Some(p) => p,
            None => match (platform::screen_size(), win.size()) {
                (Some((sw, _sh)), size) if sw > 0 => {
                    let scale = win.scale_factor().max(1.0);
                    let margin = (24.0 * scale) as i32;
                    PhysicalPosition::new((sw - size.width as i32 - margin).max(0), margin.max(0))
                }
                _ => return None,
            },
        };
        win.set_position(target);
        Some(target)
    })
    .flatten();
    if let Some(t) = applied {
        with_ui(|u| u.last_pos = Some(t));
    }
}

/// Wire the theme contract callbacks to host events / drag tracking.
fn attach_callbacks(theme: &LoadedTheme, tx: &Sender<Ev>) {
    let inst = &theme.instance;
    let has = |n: &str| theme.callbacks.contains(n);

    if has("mute-toggle") {
        let tx = tx.clone();
        let _ = inst.set_callback("mute-toggle", move |_| {
            emit(&tx, Ev::Mute);
            Value::Void
        });
    }
    if has("monitoring-toggle") {
        let tx = tx.clone();
        let _ = inst.set_callback("monitoring-toggle", move |_| {
            emit(&tx, Ev::Monitoring);
            Value::Void
        });
    }
    if has("menu-action") {
        let tx = tx.clone();
        let _ = inst.set_callback("menu-action", move |args| {
            let action = match args.first() {
                Some(Value::String(s)) => s.to_string(),
                _ => String::new(),
            };
            emit(&tx, Ev::Menu { action });
            Value::Void
        });
    }
    if has("hide-window") {
        let tx = tx.clone();
        let _ = inst.set_callback("hide-window", move |_| {
            emit(&tx, Ev::Hide);
            let _ = with_window(|w| w.hide());
            Value::Void
        });
    }

    // ── drag: global-cursor incremental tracking (event-driven) ──
    // moved events fire while pressed; each one we query the GLOBAL cursor and
    // move the window by its delta — the cursor frame never depends on the
    // window position, so tracking is 1:1 with zero feedback loop, and no WM
    // grab is involved (all button events keep reaching the theme).
    if has("drag-start") {
        let _ = inst.set_callback("drag-start", move |_| {
            let scale = with_window(|w| w.scale_factor()).unwrap_or(1.0);
            let c0 = platform::global_cursor(scale);
            with_ui(|u| {
                u.drag_events = 0;
                u.dragging = true;
                u.drag_scale = scale;
                u.drag_last = c0;
            });
            Value::Void
        });
    }
    if has("drag-move") {
        let _ = inst.set_callback("drag-move", move |_| {
            let (dragging, scale, last) = with_ui(|u| {
                if !u.dragging {
                    return (false, u.drag_scale, u.drag_last);
                }
                u.drag_events += 1;
                (true, u.drag_scale, u.drag_last)
            });
            if !dragging || !platform::position_api_usable() {
                return Value::Void;
            }
            let Some(c) = platform::global_cursor(scale) else {
                return Value::Void;
            };
            if let Some(last) = last {
                let dx = c.0 - last.0;
                let dy = c.1 - last.1;
                if dx != 0.0 || dy != 0.0 {
                    let _ = with_window(|w| {
                        let p = w.position();
                        w.set_position(PhysicalPosition::new(
                            p.x + dx.round() as i32,
                            p.y + dy.round() as i32,
                        ))
                    });
                }
            }
            with_ui(|u| u.drag_last = Some(c));
            Value::Void
        });
    }
    if has("drag-end") {
        let tx = tx.clone();
        let _ = inst.set_callback("drag-end", move |_| {
            let events = with_ui(|u| {
                let n = u.drag_events;
                u.dragging = false;
                u.drag_last = None;
                n
            });
            if events >= 2 && platform::position_api_usable() {
                if let Some(p) = with_window(|w| w.position()) {
                    if !(p.x == 0 && p.y == 0) {
                        with_ui(|u| u.last_pos = Some(p));
                        emit(&tx, Ev::Moved { x: p.x, y: p.y });
                    }
                }
            }
            Value::Void
        });
    }
}

/// Handle one host command on the UI thread.
fn handle_cmd(cmd: Cmd, tx: &Sender<Ev>) {
    match cmd {
        Cmd::State(st) => {
            with_ui(|u| {
                u.smooth_target = st.level.clamp(0.0, 1.0);
                if let Some(t) = u.theme.as_ref() {
                    theme::apply_state(t, &st, u.smooth);
                }
            });
        }
        Cmd::Theme { path } => {
            let p = PathBuf::from(path);
            match std::fs::read_to_string(&p)
                .map_err(|e| format!("cannot read {}: {e}", p.display()))
                .and_then(|src| theme::compile(src, &p).map_err(|e| e.to_string()))
                .and_then(|def| theme::instantiate(def, &p).map_err(|e| e.to_string()))
            {
                Ok(t) => activate_theme(t, tx, None, true),
                Err(e) => emit(tx, Ev::ThemeError { message: e }),
            }
        }
        Cmd::Visible { show } => {
            let _ = with_window(|w| {
                if show {
                    let _ = w.show();
                } else {
                    let _ = w.hide();
                }
            });
        }
        Cmd::Pos { x, y } => {
            let p = PhysicalPosition::new(x, y);
            let _ = with_window(|w| w.set_position(p));
            with_ui(|u| u.last_pos = Some(p));
        }
        Cmd::PosDefault => place_window(None),
        Cmd::Wdis { text, hold_ms } => show_wdis(text, hold_ms),
        Cmd::Snapshot { path } => take_snapshot(&path, tx),
        Cmd::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}

/// Show a WhatdidIsay transcript via the optional theme contract members and
/// schedule the auto-hide.
fn show_wdis(text: String, hold_ms: u64) {
    with_ui(|u| {
        if let Some(t) = u.theme.as_ref() {
            if t.props.contains("wdis-text") {
                let _ = t.instance.set_property("wdis-text", Value::String(text.into()));
            }
            if t.props.contains("wdis-visible") {
                let _ = t.instance.set_property("wdis-visible", Value::Bool(true));
            }
        }
        if let Some(old) = u.wdis_timer.take() {
            old.stop();
        }
        let t = Timer::default();
        t.start(TimerMode::SingleShot, Duration::from_millis(hold_ms), || {
            with_theme(|t| {
                if t.props.contains("wdis-visible") {
                    let _ = t.instance.set_property("wdis-visible", Value::Bool(false));
                }
            });
        });
        u.wdis_timer = Some(t);
    });
}

fn take_snapshot(path: &str, tx: &Sender<Ev>) {
    let res = with_theme(|t| t.instance.window().take_snapshot().ok());
    match res.flatten() {
        Some(buf) => {
            let (w, h) = (buf.width(), buf.height());
            let msg = match std::fs::File::create(path) {
                Ok(file) => {
                    let mut enc = png::Encoder::new(file, w, h);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    match enc
                        .write_header()
                        .and_then(|mut wr| wr.write_image_data(buf.as_bytes()))
                    {
                        Ok(()) => format!("snapshot written: {path} ({w}x{h})"),
                        Err(e) => format!("snapshot encode failed: {e}"),
                    }
                }
                Err(e) => format!("snapshot create failed: {e}"),
            };
            emit(tx, Ev::Log { message: msg });
        }
        None => emit(tx, Ev::Log {
            message: "snapshot unavailable".into(),
        }),
    }
}

/// Headless theme validation used by CI and theme authors.
pub fn compile_check(path: &std::path::Path) -> i32 {
    let Ok(src) = std::fs::read_to_string(path) else {
        eprintln!("cannot read {}", path.display());
        return 1;
    };
    match theme::compile(src, path) {
        Ok(def) => {
            let props: Vec<String> = def.properties().map(|(n, _)| n).collect();
            let cbs: Vec<String> = def.callbacks().collect();
            println!("OK {}", path.display());
            println!("  component : {}", def.name());
            println!("  properties: {}", props.join(", "));
            println!("  callbacks : {}", cbs.join(", "));
            0
        }
        Err(e) => {
            eprintln!("COMPILE ERRORS:\n{e}");
            1
        }
    }
}
