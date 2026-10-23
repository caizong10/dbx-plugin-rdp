//! Phase 1 lifecycle semantics, now layered on the Phase 2 engine abstraction:
//! validation, idempotent connect/disconnect, and no secret leakage.
use serde_json::{json, Value};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dbx_plugin_rdp::handler::{ConnectEngine, ConnectFailure, RdpPlugin};

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

fn park_session(host: &str, port: u16) -> dbx_plugin_rdp::handler::RdpSession {
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (commands_tx, commands_rx) = std::sync::mpsc::channel::<dbx_plugin_rdp::rdp::SessionCommand>();
    let task = std::thread::spawn(move || {
        let _ = shutdown_rx.recv();
        drop(commands_rx);
    });
    dbx_plugin_rdp::handler::RdpSession {
        info: dbx_plugin_rdp::handler::SessionInfo {
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

struct NullEngine;

impl ConnectEngine for NullEngine {
    fn connect(
        &self,
        params: &Value,
        _emitter: Option<&dbx_plugin_sdk::PluginEmitter>,
    ) -> Result<dbx_plugin_rdp::handler::RdpSession, ConnectFailure> {
        let (host, port) = dbx_plugin_rdp::endpoint::resolve(params).map_err(ConnectFailure)?;
        Ok(park_session(&host, port))
    }
}

fn plugin() -> RdpPlugin {
    RdpPlugin::with_engine(Arc::new(NullEngine))
}

#[test]
fn test_returns_success_for_valid_form() {
    let result = plugin().dispatch("connection/test", params()).unwrap();
    assert_eq!(result["success"], json!(true));
}

#[test]
fn test_rejects_empty_host() {
    let mut p = params();
    p["connection"]["host"] = json!("");
    let result = plugin().dispatch("connection/test", p).unwrap();
    assert_eq!(result["success"], json!(false));
    assert!(result["message"].as_str().unwrap().to_lowercase().contains("host"));
}

#[test]
fn test_rejects_out_of_range_port() {
    let mut p = params();
    p["connection"]["port"] = json!(70000u64);
    let result = plugin().dispatch("connection/test", p).unwrap();
    assert_eq!(result["success"], json!(false));
}

#[test]
fn accepts_dev_host_payload_with_password_in_secrets() {
    // The dev host (and DBX itself) maps binding:"secret" fields into
    // connection.connection_secrets instead of the top-level password field.
    let mut p = params();
    p["connection"]["password"] = Value::Null;
    p["connection"]["connection_secrets"] = json!({ "password": "from-store" });
    let result = plugin().dispatch("connection/test", p.clone()).unwrap();
    assert_eq!(result["success"], json!(true), "test: {result}");
    let connected = plugin().dispatch("connection/connect", p).unwrap();
    assert_eq!(connected["success"], json!(true));
}

#[test]
fn connect_requires_connection_id() {
    let mut p = params();
    p["connection"]["id"] = Value::Null;
    let error = plugin().dispatch("connection/connect", p).unwrap_err();
    assert_eq!(error.code, -32602);
}

#[test]
fn connect_fails_without_any_password_source() {
    let mut p = params();
    p["connection"]["password"] = Value::Null;
    let instance = plugin();
    let result = instance.dispatch("connection/connect", p).unwrap();
    assert_eq!(result["success"], json!(false));
    assert!(result["message"].as_str().unwrap().to_lowercase().contains("password"));
    assert_eq!(instance.session_count(), 0);
}

#[test]
fn connect_records_session_and_is_idempotent() {
    let instance = plugin();
    let first = instance.dispatch("connection/connect", params()).unwrap();
    assert_eq!(first["success"], json!(true));
    let second = instance.dispatch("connection/connect", params()).unwrap();
    assert_eq!(second["success"], json!(true));
    assert_eq!(instance.session_count(), 1);
}

#[test]
fn disconnect_is_idempotent() {
    let instance = plugin();
    instance.dispatch("connection/connect", params()).unwrap();
    instance.dispatch("connection/disconnect", params()).unwrap();
    let again = instance.dispatch("connection/disconnect", params()).unwrap();
    assert_eq!(again["success"], json!(true));
    assert_eq!(instance.session_count(), 0);
}

#[test]
fn action_method_is_accepted_with_action_id() {
    let mut p = params();
    p["action"] = json!({ "id": "io.github.caizong10.rdp.noop" });
    let result = plugin().dispatch("connection/action", p).unwrap();
    assert_eq!(result["success"], json!(true));
}

#[test]
fn unknown_method_reports_method_not_found() {
    let error = plugin().dispatch("objects/list", json!({})).unwrap_err();
    assert_eq!(error.code, -32601);
}

#[test]
fn ping_returns_ok_and_never_leaks_password() {
    let instance = plugin();
    let result = instance
        .dispatch("dbx-plugin-rdp/ping", json!({ "connectionId": "conn-1" }))
        .unwrap();
    assert_eq!(result["ok"], json!(true));
    let mut p = params();
    p["connection"]["host"] = json!("");
    p["connection"]["password"] = json!("hunter2");
    let text = instance.dispatch("connection/test", p).unwrap().to_string();
    assert!(!text.contains("hunter2"), "response leaked password: {text}");
}

#[test]
fn test_never_touches_the_engine() {
    struct CountingEngine {
        calls: AtomicUsize,
    }
    impl ConnectEngine for CountingEngine {
        fn connect(
            &self,
            _params: &Value,
            _emitter: Option<&dbx_plugin_sdk::PluginEmitter>,
        ) -> Result<dbx_plugin_rdp::handler::RdpSession, ConnectFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            unreachable!("test method must not dial")
        }
    }
    let engine = Arc::new(CountingEngine { calls: AtomicUsize::new(0) });
    let instance = RdpPlugin::with_engine(engine.clone());
    instance.dispatch("connection/test", params()).unwrap();
    assert_eq!(engine.calls.load(Ordering::SeqCst), 0);
}
