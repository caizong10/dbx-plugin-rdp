//! Phase 3 checks: runtime endpoint precedence, certificate trust policy,
//! and engine-level session management (idempotency, failure, no secret leak)
//! against the Phase 3 session shape (shutdown + command queues).

use serde_json::{json, Value};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dbx_plugin_rdp::handler::{
    Answer, ConnectEngine, ConnectFailure, HostError, RdpPlugin, RdpSession, SessionInfo,
    should_accept_certificate,
};
use dbx_plugin_rdp::rdp::SessionCommand;
use dbx_plugin_rdp::{endpoint, pins};

fn params() -> Value {
    json!({
        "provider": { "id": "io.github.caizong10.rdp.connection" },
        "connection": {
            "id": "conn-1",
            "host": "192.0.2.10",
            "port": 3389u64,
            "username": "alice",
            "password": "s3cret",
            "external_config": {
                "domain": "WORKGROUP",
                "color_depth": "16",
                "resize_mode": "dynamic",
                "ignore_cert": false
            }
        },
        "runtime": { "host": "127.0.0.1", "port": 53890u64 }
    })
}

// ---- endpoint resolution: DBX-provided runtime endpoint is authoritative ----

#[test]
fn prefers_runtime_endpoint_over_form() {
    let (host, port) = endpoint::resolve(&params()).unwrap();
    assert_eq!(host, "127.0.0.1");
    assert_eq!(port, 53890);
}

#[test]
fn falls_back_to_form_when_runtime_missing() {
    let mut p = params();
    p["runtime"] = Value::Null;
    let (host, port) = endpoint::resolve(&p).unwrap();
    assert_eq!(host, "192.0.2.10");
    assert_eq!(port, 3389);
}

#[test]
fn empty_runtime_fields_fall_back_to_form() {
    let mut p = params();
    p["runtime"] = json!({ "host": "", "port": 0u64 });
    let (host, port) = endpoint::resolve(&p).unwrap();
    assert_eq!(host, "192.0.2.10");
    assert_eq!(port, 3389);
}

#[test]
fn out_of_range_runtime_port_is_rejected_or_falls_back() {
    let mut p = params();
    p["runtime"] = json!({ "host": "127.0.0.1", "port": 70000u64 });
    let (host, port) = endpoint::resolve(&p).unwrap();
    assert_eq!((host.as_str(), port), ("192.0.2.10", 3389));
}

#[test]
fn fingerprints_are_formatted_lowercased_spaced() {
    let bytes = [0x01u8, 0x0F, 0xA0, 0xFF];
    assert_eq!(endpoint::format_fingerprint(&bytes), "01:0f:a0:ff");
}

// ---- pin store persistence ----

#[test]
fn pin_store_remembers_and_persists() {
    let dir = std::env::temp_dir().join(format!("dbx-rdp-pins-test-{}", std::process::id()));
    let file = dir.join("trusted_cert_pins.json");
    let _ = std::fs::remove_file(&file);
    let mut store = pins::PinStore::at(file.clone());
    assert!(!store.is_trusted("192.0.2.10", "AA..aa"));
    store.trust("192.0.2.10", "AA..aa");
    assert!(store.is_trusted("192.0.2.10", "AA..aa"));
    let reloaded = pins::PinStore::at(file);
    assert!(reloaded.is_trusted("192.0.2.10", "AA..aa"));
    assert!(!reloaded.is_trusted("192.0.2.11", "AA..aa"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- certificate policy (pure table) ----

#[test]
fn ignore_cert_skips_trust_check() {
    assert!(should_accept_certificate(true, false, None));
}

#[test]
fn trusted_pin_accepts_without_prompt() {
    assert!(should_accept_certificate(false, true, None));
}

#[test]
fn untrusted_needs_explicit_approval_and_fails_closed() {
    let approve = Some(Ok(Answer::Submit("approve".to_string())));
    let deny = Some(Ok(Answer::Cancel));
    let timeout = Some(Ok(Answer::Timeout));
    let wrong_choice = Some(Ok(Answer::Submit("deny".to_string())));
    let unsupported = Some(Err(HostError));
    assert!(should_accept_certificate(false, false, approve));
    assert!(!should_accept_certificate(false, false, deny));
    assert!(!should_accept_certificate(false, false, timeout));
    assert!(!should_accept_certificate(false, false, wrong_choice));
    assert!(!should_accept_certificate(false, false, unsupported));
    assert!(!should_accept_certificate(false, false, None));
}

// ---- engine dispatch ----

fn park_session(host: &str, port: u16) -> RdpSession {
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (commands_tx, commands_rx) = std::sync::mpsc::channel::<SessionCommand>();
    let task = std::thread::spawn(move || {
        let _ = shutdown_rx.recv();
        drop(commands_rx);
    });
    RdpSession {
        info: SessionInfo {
            dial_host: host.to_string(),
            dial_port: port,
            width: 1024,
            height: 768,
            color_depth: 16,
            compression: "rdp61".to_string(),
        },
        shutdown: shutdown_tx,
        commands: commands_tx,
        task,
        peer_addr: format!("{host}:{port}"),
    }
}

struct FakeEngine {
    calls: AtomicUsize,
}

impl ConnectEngine for FakeEngine {
    fn connect(
        &self,
        params: &Value,
        _emitter: Option<&dbx_plugin_sdk::PluginEmitter>,
    ) -> Result<RdpSession, ConnectFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (host, port) = endpoint::resolve(params).map_err(ConnectFailure)?;
        Ok(park_session(&host, port))
    }
}

struct FailingEngine;

impl ConnectEngine for FailingEngine {
    fn connect(
        &self,
        _params: &Value,
        _emitter: Option<&dbx_plugin_sdk::PluginEmitter>,
    ) -> Result<RdpSession, ConnectFailure> {
        Err(ConnectFailure("authentication refused (NLA)".to_string()))
    }
}

#[test]
fn connect_invokes_engine_once_and_tracks_session() {
    let engine = Arc::new(FakeEngine { calls: AtomicUsize::new(0) });
    let plugin = RdpPlugin::with_engine(engine.clone());
    plugin.dispatch("connection/connect", params()).unwrap();
    plugin.dispatch("connection/connect", params()).unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1, "idempotent per connection id");
    assert_eq!(plugin.session_count(), 1);
    // the runtime endpoint (not the form endpoint) was dialed
    assert_eq!(plugin.session_info("conn-1").unwrap().dial_host, "127.0.0.1");
    assert_eq!(plugin.session_info("conn-1").unwrap().dial_port, 53890);
    plugin.dispatch("connection/disconnect", params()).unwrap();
    assert_eq!(plugin.session_count(), 0);
    plugin.dispatch("connection/connect", params()).unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2);
    plugin.dispatch("connection/disconnect", params()).unwrap();
}

#[test]
fn engine_failure_returns_success_false_without_secret_leak() {
    let plugin = RdpPlugin::with_engine(Arc::new(FailingEngine));
    let result = plugin.dispatch("connection/connect", params()).unwrap();
    assert_eq!(result["success"], json!(false));
    assert!(result["message"].as_str().unwrap().contains("NLA"));
    assert!(!result.to_string().contains("s3cret"));
    assert_eq!(plugin.session_count(), 0);
}

#[test]
fn test_method_never_opens_a_real_session() {
    let engine = Arc::new(FakeEngine { calls: AtomicUsize::new(0) });
    let plugin = RdpPlugin::with_engine(engine.clone());
    plugin.dispatch("connection/test", params()).unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn resize_action_queues_command_on_live_session() {
    let engine = Arc::new(FakeEngine { calls: AtomicUsize::new(0) });
    let plugin = RdpPlugin::with_engine(engine);
    plugin.dispatch("connection/connect", params()).unwrap();
    let result = plugin
        .dispatch(
            "connection/action",
            json!({
                "connection": { "id": "conn-1" },
                "action": { "id": "io.github.caizong10.rdp.resize" },
                "width": 1024u64,
                "height": 768u64
            }),
        )
        .unwrap();
    assert_eq!(result["success"], json!(true));
    // unknown id without session fails gracefully
    let missing = plugin
        .dispatch(
            "connection/action",
            json!({
                "connection": { "id": "nope" },
                "action": { "id": "io.github.caizong10.rdp.resize" },
                "width": 800u64,
                "height": 600u64
            }),
        )
        .unwrap();
    assert_eq!(missing["success"], json!(false));
    plugin.dispatch("connection/disconnect", params()).unwrap();
}
