//! Minimal file-based diagnostics for the clipboard (CLIPRDR) channel.
//!
//! The host captures sidecar stderr nowhere persistent and this machine's
//! copy of the password is stored encrypted, so a developer cannot reproduce
//! the live handshake locally. This module writes a short, structured trace
//! to the plugin data directory that the developer can read after a single
//! real connection. It records channel IDs and protocol milestones only —
//! never credentials, never clipboard contents (text is logged as a length).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Monotonic record counter so ordering survives across threads.
static SEQ: AtomicUsize = AtomicUsize::new(0);
static LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> PathBuf {
    let base = std::env::var("DBX_PLUGIN_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("cliprdr-diag.log")
}

/// Append one line with a microsecond timestamp. Best-effort: a diagnostic
/// failure must never affect the session, so all errors are swallowed.
pub fn note(message: &str) {
    let _guard = LOCK.lock().ok();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(file, "#{n} t={now}us {message}");
    }
}

/// Truncate the log at session start so each connection yields a clean trace.
pub fn reset() {
    let _guard = LOCK.lock().ok();
    SEQ.store(0, Ordering::Relaxed);
    let _ = std::fs::write(log_path(), "");
}
