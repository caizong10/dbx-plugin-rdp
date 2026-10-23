//! Development-only fake engine (never used unless DBX_RDP_FAKE=1).
//!
//! It performs no network I/O: the host UI closed loop in the browser dev host
//! (open workbench -> render -> input -> disconnect) must be verifiable without
//! a live Windows machine, while the default binary always uses IronRDP.
//! Frames are not produced by the fake engine (nothing to render); the UI
//! shows a placeholder pattern instead.

use std::time::Duration;

use serde_json::Value;

use crate::endpoint;
use crate::handler::{ConnectEngine, ConnectFailure, RdpSession, SessionInfo};
use crate::rdp::SessionCommand;

pub struct FakeEngine;

impl ConnectEngine for FakeEngine {
    fn connect(
        &self,
        params: &Value,
        _emitter: Option<&dbx_plugin_sdk::PluginEmitter>,
    ) -> Result<RdpSession, ConnectFailure> {
        let (dial_host, dial_port) = endpoint::resolve(params).map_err(ConnectFailure)?;
        let connection = params.get("connection");
        let width = connection
            .and_then(|c| c.get("external_config"))
            .and_then(|c| c.get("width"))
            .and_then(Value::as_u64)
            .unwrap_or(1280) as u16;
        let height = connection
            .and_then(|c| c.get("external_config"))
            .and_then(|c| c.get("height"))
            .and_then(Value::as_u64)
            .unwrap_or(800) as u16;
        let color_depth = connection
            .and_then(|c| c.get("external_config"))
            .and_then(|c| c.get("color_depth"))
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u8>().ok())
            .unwrap_or(16);

        tracing::warn!(
            "FAKE RDP session (DBX_RDP_FAKE=1): no network I/O for {dial_host}:{dial_port}"
        );

        let info = SessionInfo {
            dial_host: dial_host.clone(),
            dial_port,
            width,
            height,
            color_depth,
            compression: "fake".to_string(),
        };

        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let (commands_tx, commands_rx) = std::sync::mpsc::channel::<SessionCommand>();
        let task = std::thread::Builder::new()
            .name("dbx-rdp-fake-session".to_string())
            .spawn(move || loop {
                match shutdown_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                // Drain and drop commands so senders never block.
                while commands_rx.try_recv().is_ok() {}
            })
            .map_err(|e| ConnectFailure(format!("Failed to start fake session: {e}")))?;

        Ok(RdpSession {
            info,
            shutdown: shutdown_tx,
            commands: commands_tx,
            task,
            peer_addr: format!("{dial_host}:{dial_port}"),
        })
    }
}
