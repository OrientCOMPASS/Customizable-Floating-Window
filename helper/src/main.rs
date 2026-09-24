//! floating_helper — Slint floating-window process.
//!
//! Since round 5 the UI runtime lives inside the plugin process on
//! Windows/Linux (plugin-owned thread). This binary remains for:
//!   * **macOS**, where winit/Slint require the process main thread for the
//!     event loop (the Tauri host owns it) — the plugin spawns this helper and
//!     bridges Cmd/Ev over stdin/stdout;
//!   * **tooling** on all platforms: `--compile-check` (CI theme validation)
//!     and `--selftest` (headless snapshots for visual regression).

use cfw_protocol::{Cmd, Ev, StatePayload};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| -> Option<String> {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    if args.iter().any(|a| a == "--version") {
        println!("floating_helper {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Some(path) = get("--compile-check") {
        std::process::exit(cfw_uiruntime::compile_check(&PathBuf::from(path)));
    }

    let theme = get("--theme").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("ring.slint"));
    let pos = match (get("--x"), get("--y")) {
        (Some(x), Some(y)) => match (x.parse::<i32>().ok(), y.parse::<i32>().ok()) {
            (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x, y)),
            _ => None,
        },
        _ => None,
    };

    let (tx_cmd, rx_cmd) = mpsc::channel::<Cmd>();
    let (tx_ev, rx_ev) = mpsc::channel::<Ev>();

    // stdout bridge: Ev → JSON lines (protocol channel to the plugin)
    let out = std::io::stdout();
    std::thread::spawn(move || {
        let mut out = std::io::BufWriter::new(out);
        while let Ok(ev) = rx_ev.recv() {
            let line = serde_json::to_string(&ev).unwrap_or_default();
            if writeln!(out, "{line}").is_err() || out.flush().is_err() {
                break;
            }
        }
    });

    // selftest feeder keeps a clone before the stdin bridge moves the sender
    if args.iter().any(|a| a == "--selftest") {
        let prefix = get("--selftest").unwrap_or_else(|| "cfw".into());
        let tx = tx_cmd.clone();
        std::thread::spawn(move || selftest_feeder(tx, prefix));
    }

    // stdin bridge: JSON lines → Cmd. In normal (macOS plugin-driven) mode,
    // stdin EOF means the parent is gone → quit (orphan safety). In selftest
    // mode stdin is closed from the start → must NOT quit on EOF.
    let quit_on_eof = !args.iter().any(|a| a == "--selftest");
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Some(cmd) = cfw_protocol::decode_cmd(&line) {
                if tx_cmd.send(cmd).is_err() {
                    break;
                }
            }
        }
        if quit_on_eof {
            let _ = tx_cmd.send(Cmd::Quit);
        }
    });

    cfw_uiruntime::run(rx_cmd, tx_ev, theme, pos);
}

/// Synthetic state driver for headless visual regression snapshots.
fn selftest_feeder(tx: mpsc::Sender<Cmd>, prefix: String) {
    let streaming = StatePayload {
        level: 0.62,
        processed: 0.55,
        muted: false,
        streaming: true,
        monitoring: false,
        sample_rate: 48000,
        channels: 1,
        queued_ms: 12.5,
        session_seconds: 754,
        session_text: "12:34".into(),
        device_label: "MicYou Mobile".into(),
        device_mode: "wifi".into(),
    };
    let _ = tx.send(Cmd::State(streaming.clone()));
    if std::env::var_os("CFW_SELFTEST_WDIS").is_some() {
        std::thread::sleep(Duration::from_millis(300));
        let _ = tx.send(Cmd::Wdis {
            text: "Hello everyone, let's start the meeting.".into(),
            hold_ms: 5000,
        });
    }
    std::thread::sleep(Duration::from_millis(600));
    let _ = tx.send(Cmd::Snapshot {
        path: format!("{prefix}-stream.png"),
    });
    std::thread::sleep(Duration::from_millis(600));
    let _ = tx.send(Cmd::State(StatePayload {
        muted: true,
        level: 0.3,
        ..streaming.clone()
    }));
    std::thread::sleep(Duration::from_millis(400));
    let _ = tx.send(Cmd::Snapshot {
        path: format!("{prefix}-muted.png"),
    });
    std::thread::sleep(Duration::from_millis(600));
    let _ = tx.send(Cmd::State(StatePayload {
        level: 0.0,
        processed: 0.0,
        muted: false,
        streaming: false,
        session_seconds: 0,
        session_text: "00:00".into(),
        device_label: String::new(),
        device_mode: String::new(),
        ..streaming.clone()
    }));
    std::thread::sleep(Duration::from_millis(400));
    let _ = tx.send(Cmd::Snapshot {
        path: format!("{prefix}-idle.png"),
    });
    std::thread::sleep(Duration::from_millis(300));
    let _ = tx.send(Cmd::Quit);
}
