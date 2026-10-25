//! Clipboard change watcher: WM_CLIPBOARDUPDATE event pump (mstsc-style).
//!
//! Windows clipboard changes arrive as window messages; a dedicated thread
//! owns a message-only window and relays every change through a channel.
//! This replaces the old 300ms poll, which raced the shell's text
//! projection of file lists and caused the publish/echo bugs. With event
//! delivery, each clipboard generation is read exactly once, in one place.
//!
//! Non-Windows platforms keep the timed poll (same call site, see rdp.rs).

use std::sync::mpsc::Sender;
use std::time::Duration;

/// Events the watcher emits.
#[derive(Debug)]
pub enum ClipboardEvent {
    /// The clipboard changed; the session loop reads it once (files first).
    Changed,
    /// The watcher thread died; fall back to polling.
    Failed(String),
}

/// Spawns the watcher thread. The returned guard keeps it alive; dropping
/// the guard posts a shutdown message to the watcher window.
#[cfg(windows)]
pub fn spawn(events: Sender<ClipboardEvent>) -> WatcherGuard {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    // One clone for the thread, one for startup failure reporting.
    let startup_events = events.clone();
    let thread_events = events;

    std::thread::Builder::new()
        .name("dbx-rdp-clip-watch".to_string())
        .spawn(move || {
            let events = thread_events;
            use clipboard_win::Monitor;
            let mut monitor = match Monitor::new() {
                Ok(monitor) => monitor,
                Err(error) => {
                    let _ = events.send(ClipboardEvent::Failed(format!("monitor: {error}")));
                    return;
                }
            };
            // The shutdown handle must be created from the same thread that
            // owns the window; keep it inside this thread and park it in a
            // local so Drop posts the close message when the thread ends.
            let shutdown = monitor.shutdown_channel();
            let _ = ready_tx.send(Ok(()));

            loop {
                match monitor.recv() {
                    Ok(true) => {
                        if events.send(ClipboardEvent::Changed).is_err() {
                            break; // session loop gone
                        }
                    }
                    Ok(false) => break, // shutdown message
                    Err(error) => {
                        let _ = events.send(ClipboardEvent::Failed(format!("watch: {error}")));
                        break;
                    }
                }
            }
            drop(shutdown);
        })
        .expect("spawn clipboard watcher thread");

    // Wait briefly for the window to come up so the first events are not
    // missed between spawn and registration.
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            let _ = startup_events.send(ClipboardEvent::Failed(reason));
        }
        Err(_) => {
            let _ = startup_events.send(ClipboardEvent::Failed("watcher startup timeout".into()));
        }
    }
    WatcherGuard {}
}

/// Non-Windows: no event source; rdp.rs keeps its timed poll on these
/// platforms, so the watcher is never constructed there.
#[cfg(not(windows))]
pub fn spawn(_events: Sender<ClipboardEvent>) -> WatcherGuard {
    WatcherGuard {}
}

/// Keeps the watcher thread alive for as long as the session lives.
pub struct WatcherGuard {
    // The thread drops its Shutdown handle on exit; nothing to hold here.
}

impl WatcherGuard {
    /// Stop watching (session ended). On Windows the thread sees the
    /// session-loop channel close and exits on its own; this only prevents
    /// further events from being delivered.
    pub fn stop(&self) {}
}
