//! Persistent trust store for RDP server certificate fingerprints.
//!
//! Lives under `DBX_PLUGIN_DATA_DIR` (plugin-private, survives upgrades).
//! Only host + SHA-256 fingerprints are stored — never credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct PinStore {
    path: PathBuf,
    pins: BTreeMap<String, Vec<String>>,
}

impl PinStore {
    /// Loads from `path` when it exists; a corrupt file starts empty.
    pub fn at(path: PathBuf) -> Self {
        let pins = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, pins }
    }

    /// Default location inside the sidecar data directory.
    pub fn default_location() -> Self {
        let base = std::env::var("DBX_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(".").join("plugin-data"));
        Self::at(base.join("trusted_cert_pins.json"))
    }

    pub fn is_trusted(&self, host: &str, fingerprint: &str) -> bool {
        self.pins
            .get(host)
            .is_some_and(|list| list.iter().any(|pin| pin == fingerprint))
    }

    pub fn trust(&mut self, host: &str, fingerprint: &str) {
        let list = self.pins.entry(host.to_string()).or_default();
        if !list.iter().any(|pin| pin == fingerprint) {
            list.push(fingerprint.to_string());
        }
        self.save();
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.pins) {
            std::fs::write(&self.path, text).ok();
        }
    }
}
