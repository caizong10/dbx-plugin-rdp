//! DBX RDP plugin sidecar: protocol engine, frame codec, trust store and
//! lifecycle handler.

pub mod clipboard;
pub mod clip_dump;
pub mod clip_files;
#[cfg(windows)]
pub mod clip_owner;
#[cfg(not(windows))]
#[path = "clip_owner_stub.rs"]
pub mod clip_owner;
pub mod clip_transfer;
#[cfg(windows)]
pub mod clip_virtual;
pub mod clip_watch;
pub mod diag;
pub mod endpoint;
pub mod fake;
pub mod frame;
pub mod handler;
pub mod pins;
pub mod rdp;
