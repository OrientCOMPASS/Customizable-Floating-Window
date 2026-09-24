//! Bridge between host-dispatched plugin threads and the Slint UI runtime.
//!
//! * Windows/Linux: `UiLink::Thread` — the UI runtime runs in-process on a
//!   plugin-owned thread (round-5 rollback to the user-verified first-edition
//!   design); Cmd/Ev travel over in-process channels.
//! * macOS: `UiLink::Proc` — winit/Slint require the process main thread for
//!   the event loop, which the Tauri host owns; the UI therefore runs in the
//!   `floating_helper` subprocess and Cmd/Ev travel over stdin/stdout.

use cfw_protocol::{Cmd, Ev};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;

pub enum UiLink {
    #[cfg(not(target_os = "macos"))]
    Thread {
        tx: Sender<Cmd>,
        rx: Mutex<Receiver<Ev>>,
        handle: Option<JoinHandle<()>>,
    },
    #[cfg(target_os = "macos")]
    Proc(crate::helper::HelperProc),
}

impl UiLink {
    #[cfg(not(target_os = "macos"))]
    pub fn spawn_thread(theme: PathBuf, pos: Option<(i32, i32)>) -> Self {
        let (tx_cmd, rx_cmd) = channel::<Cmd>();
        let (tx_ev, rx_ev) = channel::<Ev>();
        let handle = std::thread::Builder::new()
            .name("cfw-ui".into())
            .spawn(move || {
                // A UI panic must never unwind into the host process.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cfw_uiruntime::run(rx_cmd, tx_ev, theme, pos)
                }));
            })
            .expect("spawn cfw-ui thread");
        UiLink::Thread {
            tx: tx_cmd,
            rx: Mutex::new(rx_ev),
            handle: Some(handle),
        }
    }

    pub fn send(&self, cmd: &Cmd) -> bool {
        match self {
            #[cfg(not(target_os = "macos"))]
            UiLink::Thread { tx, .. } => tx.send(cmd.clone()).is_ok(),
            #[cfg(target_os = "macos")]
            UiLink::Proc(p) => p.send(cmd),
        }
    }

    pub fn drain(&self) -> Vec<Ev> {
        match self {
            #[cfg(not(target_os = "macos"))]
            UiLink::Thread { rx, .. } => {
                let mut out = Vec::new();
                if let Ok(rx) = rx.lock() {
                    while let Ok(ev) = rx.try_recv() {
                        out.push(ev);
                    }
                }
                out
            }
            #[cfg(target_os = "macos")]
            UiLink::Proc(p) => p.drain_events(),
        }
    }

    pub fn is_dead(&mut self) -> bool {
        match self {
            #[cfg(not(target_os = "macos"))]
            UiLink::Thread { handle, .. } => {
                handle.as_ref().map(|h| h.is_finished()).unwrap_or(true)
            }
            #[cfg(target_os = "macos")]
            UiLink::Proc(p) => p.is_dead(),
        }
    }

    pub fn shutdown(self) {
        let _ = self.send(&Cmd::Quit);
        match self {
            #[cfg(not(target_os = "macos"))]
            UiLink::Thread { handle, .. } => {
                if let Some(h) = handle {
                    let _ = h.join();
                }
            }
            #[cfg(target_os = "macos")]
            UiLink::Proc(p) => p.shutdown(),
        }
    }
}
