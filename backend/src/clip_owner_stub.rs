//! Non-Windows stubs for the Windows delayed-rendering clipboard owner.
//!
//! Linux and macOS keep the text clipboard path through `arboard`. File
//! clipboard integration is intentionally Windows-only because CF_HDROP and
//! OLE delayed rendering are Win32 protocols.

use std::path::PathBuf;

pub fn offer_active() -> bool {
    false
}

pub fn mark_background_published(_paths: &[PathBuf]) {}

pub fn render_recent(_window_ms: u128) -> bool {
    false
}
