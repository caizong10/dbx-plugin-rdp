use std::sync::Arc;

use dbx_plugin_rdp::fake::FakeEngine;
use dbx_plugin_rdp::handler::RdpPlugin;
use dbx_plugin_rdp::rdp::IronRdpEngine;
use dbx_plugin_sdk::{PluginMetadata, PluginServer, PluginTransport};

fn main() -> std::io::Result<()> {
    // stderr-only logging: stdout is reserved for framed protocol messages.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init()
        .ok();

    // Engine selection: real IronRDP by default. DBX_RDP_FAKE=1 swaps in a
    // no-network fake session purely so the browser dev host's UI loop can be
    // exercised without a live Windows machine.
    let engine: Arc<dyn dbx_plugin_rdp::handler::ConnectEngine> =
        if std::env::var("DBX_RDP_FAKE").is_ok_and(|value| value == "1") {
            tracing::warn!("DBX_RDP_FAKE=1 — fake RDP engine active, no real network I/O");
            Arc::new(FakeEngine)
        } else {
            Arc::new(IronRdpEngine)
        };

    // The Cargo package version must equal manifest.json "version", otherwise
    // the host reports "Sidecar identity or protocol does not match manifest".
    let metadata = PluginMetadata::new("io.github.caizong10.rdp", env!("CARGO_PKG_VERSION"))
        .with_capability("connections")
        .with_capability("binaryInput");
    PluginServer::new(metadata, RdpPlugin::with_engine(engine))
        .transport(PluginTransport::Framed)
        .serve()
}
