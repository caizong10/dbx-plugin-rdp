//! Endpoint resolution: DBX hands the post-tunnel real endpoint in
//! `runtime.host` / `runtime.port`; fall back to the raw form fields only when
//! the host did not provide them (e.g. the browser dev host sends `null`).

use serde_json::Value;

fn in_range_port(value: Option<u64>) -> Option<u16> {
    let value = value?;
    if value == 0 || value > u16::MAX as u64 {
        return None;
    }
    Some(value as u16)
}

/// Returns the (host, port) the sidecar must actually dial.
pub fn resolve(params: &Value) -> Result<(String, u16), String> {
    let connection = params
        .get("connection")
        .ok_or_else(|| "Missing connection payload".to_string())?;
    let runtime = params.get("runtime").filter(|value| !value.is_null());

    let runtime_host = runtime
        .and_then(|runtime| runtime.get("host"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let runtime_port = in_range_port(runtime.and_then(|runtime| runtime.get("port")).and_then(Value::as_u64));

    let (host, port) = if !runtime_host.is_empty() && runtime_port.is_some() {
        (runtime_host.to_string(), runtime_port.expect("checked above"))
    } else {
        let form_host = connection.get("host").and_then(Value::as_str).unwrap_or("").trim();
        let form_port = in_range_port(connection.get("port").and_then(Value::as_u64));
        (form_host.to_string(), form_port.unwrap_or(0))
    };

    if host.is_empty() {
        return Err("Host is required.".to_string());
    }
    if port == 0 {
        return Err("Port must be between 1 and 65535.".to_string());
    }
    Ok((host, port))
}

/// Lowercase, colon-separated SHA-256 style fingerprint display.
pub fn format_fingerprint(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
