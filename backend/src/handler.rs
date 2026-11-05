use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dbx_plugin_sdk::{
    host_client, PluginEmitter, PluginError, PluginHandler, RequestContext, UserInputOption,
    UserInputPrompt,
};
use serde_json::{json, Value};

use crate::endpoint;
use crate::frame::{self, InputKind, InputMessage};
use crate::rdp::{IronRdpEngine, SessionCommand};

/// Outcome of a user-facing prompt routed through the host UI.
pub enum Answer {
    Submit(String),
    Cancel,
    Timeout,
}

/// Marker for "the host cannot prompt" (-32001 / -32601 / -32602 or no client).
pub struct HostError;

/// True when the host both reached us and advertises the approval prompt.
/// Used only to make a rejected certificate self-explanatory in the logs.
pub fn host_can_prompt() -> bool {
    match host_client() {
        Some(client) => client.supports(dbx_plugin_sdk::HOST_REQUEST_USER_INPUT_METHOD),
        None => false,
    }
}

/// What the handler asks the host when a self-signed certificate appears.
/// Kept engine-agnostic so it is unit-testable without a sidecar server.
pub fn ask_host_to_trust(host_display: String, fingerprint: String) -> Result<Answer, HostError> {
    let Some(client) = host_client() else {
        return Err(HostError);
    };
    if !client.supports(dbx_plugin_sdk::HOST_REQUEST_USER_INPUT_METHOD) {
        return Err(HostError);
    }
    let prompt = UserInputPrompt::choice(
        format!("Certificate SHA-256: {fingerprint}\nConnect to {host_display} anyway?"),
        vec![
            UserInputOption {
                value: "approve".to_string(),
                label: "Trust and continue".to_string(),
            },
            UserInputOption {
                value: "deny".to_string(),
                label: "Cancel".to_string(),
            },
        ],
    )
    .with_title("Untrusted RDP server certificate")
    .with_timeout_secs(120);
    match client.request_user_input(&prompt) {
        Ok(answer) => {
            if let Some(value) = answer.submitted() {
                Ok(Answer::Submit(value.to_string()))
            } else if answer.is_timeout() {
                Ok(Answer::Timeout)
            } else {
                Ok(Answer::Cancel)
            }
        }
        Err(error) if matches!(error.code, -32001 | -32601 | -32602 | -32002) => Err(HostError),
        Err(_) => Err(HostError),
    }
}

/// Certificate acceptance policy (pure, table-testable):
/// ignore_cert wins; else a trusted pin; else the host prompt must come back
/// with an explicit approve. Every other outcome fails closed.
pub fn should_accept_certificate(
    ignore_cert: bool,
    trusted: bool,
    answer: Option<Result<Answer, HostError>>,
) -> bool {
    if ignore_cert {
        return true;
    }
    if trusted {
        return true;
    }
    matches!(answer, Some(Ok(Answer::Submit(ref value))) if value == "approve")
}

/// Successful negotiation summary kept per session. Never contains secrets.
#[derive(Clone)]
pub struct SessionInfo {
    pub dial_host: String,
    pub dial_port: u16,
    pub width: u16,
    pub height: u16,
    pub color_depth: u8,
    pub compression: String,
}

/// An RDP session: negotiated state plus the live sidecar task driving the
/// protocol loop and the command queue receiving UI input.
pub struct RdpSession {
    pub info: SessionInfo,
    pub shutdown: std::sync::mpsc::Sender<()>,
    pub commands: std::sync::mpsc::Sender<SessionCommand>,
    pub task: std::thread::JoinHandle<()>,
    pub peer_addr: String,
}

pub trait ConnectEngine: Send + Sync {
    /// Perform the real RDP handshake for this lifecycle payload. `emitter`
    /// carries desktop frames and status notifications to the UI.
    fn connect(
        &self,
        params: &Value,
        emitter: Option<&PluginEmitter>,
    ) -> Result<RdpSession, ConnectFailure>;
}

pub struct ConnectFailure(pub String);

/// Outcome the connect RPC reports back to the host.
enum Handshake {
    /// Session established within the synchronous window.
    Ready(RdpSession, Arc<AtomicBool>),
    /// Handshake failed (message never contains credentials).
    Failed(String),
    /// Handshake still running; a publisher thread registers the session and
    /// pushes rdp/status events when it completes.
    Running,
}

/// How long the connect RPC waits synchronously before declaring the
/// handshake "running in background". The host aborts backend RPCs at ~10s;
/// LAN handshakes usually finish in well under 3s, and anything slower is
/// surfaced through status events instead of a dead RPC.
const SYNC_HANDSHAKE_WINDOW: std::time::Duration = std::time::Duration::from_millis(2500);

/// Shared registry so background handshake threads can publish sessions the
/// same way the synchronous path used to.
struct RegisteredSession {
    session: RdpSession,
    id: String,
    dynamic: bool,
    initial_resize_used: bool,
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

fn register_session(session: RdpSession, dynamic: bool) -> RegisteredSession {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    let serial = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    RegisteredSession { session, id: format!("{stamp:016x}{serial:016x}"), dynamic, initial_resize_used: false }
}

fn dynamic_mode(params: &Value) -> bool {
    params.get("connection").and_then(|c| c.get("external_config")).and_then(|c| c.get("resize_mode"))
        .and_then(Value::as_str) != Some("fixed")
}

type Sessions = Arc<std::sync::Mutex<HashMap<String, RegisteredSession>>>;

pub struct RdpPlugin {
    sessions: Sessions,
    /// Connection ids with a handshake in flight; the flag lets a user
    /// disconnect cancel the background attempt.
    handshakes: Arc<std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>>,
    engine: Arc<dyn ConnectEngine>,
}

impl Default for RdpPlugin {
    fn default() -> Self {
        Self::with_engine(Arc::new(IronRdpEngine))
    }
}

impl RdpPlugin {
    pub fn with_engine(engine: Arc<dyn ConnectEngine>) -> Self {
        Self {
            sessions: Arc::new(std::sync::Mutex::new(HashMap::new())),
            handshakes: Arc::new(std::sync::Mutex::new(HashMap::new())),
            engine,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|map| map.len()).unwrap_or(0)
    }

    /// Snapshot of an active session for UI state.
    pub fn session_info(&self, connection_id: &str) -> Option<SessionInfo> {
        self.sessions
            .lock()
            .ok()
            .and_then(|map| map.get(connection_id).map(|s| s.session.info.clone()))
    }

    fn cleanup_handshake(&self, connection_id: &str, token: &Arc<AtomicBool>) {
        if let Ok(mut map) = self.handshakes.lock() {
            if map
                .get(connection_id)
                .is_some_and(|current| Arc::ptr_eq(current, token))
            {
                map.remove(connection_id);
            }
        }
    }

    /// Start the handshake in a worker thread and wait for it only briefly,
    /// so a slow or hung endpoint can never stall the host's RPC window.
    fn begin_handshake(
        &self,
        params: &Value,
        connection_id: &str,
        emitter: Option<&PluginEmitter>,
    ) -> Handshake {
        // One handshake per connection id; a second connect while the first is
        // still in flight is acknowledged rather than dialing twice.
        let cancel = {
            let mut handshakes = match self.handshakes.lock() {
                Ok(map) => map,
                Err(_) => return Handshake::Failed("Handshake registry is poisoned".to_string()),
            };
            if handshakes.contains_key(connection_id) {
                return Handshake::Running;
            }
            let flag = Arc::new(AtomicBool::new(false));
            handshakes.insert(connection_id.to_string(), Arc::clone(&flag));
            flag
        };

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let engine = Arc::clone(&self.engine);
        let params_for_task = params.clone();
        let emitter_for_task = emitter.cloned();
        let id_for_task = connection_id.to_string();
        let cancel_for_task = Arc::clone(&cancel);
        let spawn = std::thread::Builder::new()
            .name(format!("dbx-rdp-handshake-{id_for_task}"))
            .spawn(move || {
                let outcome = match engine.connect(&params_for_task, emitter_for_task.as_ref()) {
                    Ok(session) => Ok(session),
                    Err(ConnectFailure(message)) => Err(message),
                };
                // If the user cancelled while we were dialing, tear the fresh
                // session down instead of publishing it.
                if let Ok(session) = &outcome {
                    if cancel_for_task.load(Ordering::SeqCst) {
                        let _ = session.shutdown.send(());
                        return;
                    }
                }
                let _ = result_tx.send(outcome);
            });
        if spawn.is_err() {
            self.cleanup_handshake(connection_id, &cancel);
            return Handshake::Failed("Failed to start handshake thread".to_string());
        }

        match result_rx.recv_timeout(SYNC_HANDSHAKE_WINDOW) {
            Ok(Ok(session)) => Handshake::Ready(session, Arc::clone(&cancel)),
            Ok(Err(message)) => {
                self.cleanup_handshake(connection_id, &cancel);
                Handshake::Failed(message)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.cleanup_handshake(connection_id, &cancel);
                Handshake::Failed("RDP handshake thread ended without a result".to_string())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Still dialing. Tell the workbench we are connecting, then
                // publish the session (or the failure) when the thread ends.
                let Some(emitter) = emitter.cloned() else {
                    self.cleanup_handshake(connection_id, &cancel);
                    return Handshake::Failed(
                        "Handshake still running but no event channel is available.".to_string(),
                    );
                };
                let _ = emitter.event(
                    frame::STATUS_EVENT,
                    frame::status_payload(
                        connection_id,
                        "connecting",
                        Some(json!({ "async": true })),
                    ),
                );
                let sessions = Arc::clone(&self.sessions);
                let dynamic = dynamic_mode(params);
                let handshakes = Arc::clone(&self.handshakes);
                let cancel_for_publish = Arc::clone(&cancel);
                let id_for_worker = connection_id.to_string();
                let publish = std::thread::Builder::new()
                    .name(format!("dbx-rdp-publish-{id_for_worker}"))
                    .spawn(move || {
                        let outcome = result_rx.recv();
                        // Hold the handshake registry through publication. Disconnect
                        // takes the same lock before the session lock, so a cancelled
                        // or superseded generation cannot publish or remove a newer one.
                        let Ok(mut registry) = handshakes.lock() else {
                            if let Ok(Ok(session)) = outcome {
                                let _ = session.shutdown.send(());
                            }
                            return;
                        };
                        let current = registry
                            .get(&id_for_worker)
                            .is_some_and(|token| Arc::ptr_eq(token, &cancel_for_publish));
                        if !current || cancel_for_publish.load(Ordering::SeqCst) {
                            if let Ok(Ok(session)) = outcome {
                                let _ = session.shutdown.send(());
                            }
                            return;
                        }
                        match outcome {
                            Ok(Ok(session)) => {
                                if let Ok(mut map) = sessions.lock() {
                                    if let Some(old) = map.insert(id_for_worker.clone(), register_session(session, dynamic)) {
                                        let _ = old.session.shutdown.send(());
                                    }
                                    if let Some(registered) = map.get(&id_for_worker) {
                                        let ready = json!({"resizeMode": if registered.dynamic { "dynamic" } else { "fixed" }, "sessionId": registered.id});
                                        let _ = emitter.event(frame::STATUS_EVENT, frame::status_payload(&id_for_worker, "session-ready", Some(ready)));
                                    }
                                } else {
                                    let _ = session.shutdown.send(());
                                }
                            }
                            Ok(Err(message)) => {
                                tracing::warn!("RDP background handshake failed: {message}");
                                let _ = emitter.event(
                                    frame::STATUS_EVENT,
                                    frame::status_payload(
                                        &id_for_worker,
                                        "connect-failed",
                                        Some(json!({ "message": message })),
                                    ),
                                );
                            }
                            Err(_) => {}
                        }
                        registry.remove(&id_for_worker);
                    });
                if publish.is_err() {
                    self.cleanup_handshake(connection_id, &cancel);
                    return Handshake::Failed("Failed to start handshake publisher".to_string());
                }
                Handshake::Running
            }
        }
    }

    /// Pure dispatch entry used by unit tests (no emitter: lifecycle
    /// bookkeeping only; real frames need the protocol handler's emitter).
    pub fn dispatch(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        self.dispatch_with_emitter(method, params, None)
    }

    pub fn dispatch_with_emitter(
        &self,
        method: &str,
        params: Value,
        emitter: Option<&PluginEmitter>,
    ) -> Result<Value, PluginError> {
        match method {
            "connection/test" => {
                let problems = validate_form(&params);
                if problems.is_empty() {
                    let (host, port) = endpoint::resolve(&params)
                        .map_err(|message| PluginError::new(-32602, message))?;
                    Ok(json!({
                        "success": true,
                        "message": format!("RDP parameters are valid for {host}:{port}.")
                    }))
                } else {
                    Ok(json!({ "success": false, "message": problems.join("; ") }))
                }
            }
            "connection/connect" => {
                let problems = validate_form(&params);
                if !problems.is_empty() {
                    return Ok(json!({ "success": false, "message": problems.join("; ") }));
                }
                let connection_id = connection_id(&params)?;
                {
                    let sessions = self
                        .sessions
                        .lock()
                        .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?;
                    if sessions.contains_key(connection_id) {
                        return Ok(json!({ "success": true })); // idempotent
                    }
                }
                let handshake = self.begin_handshake(&params, connection_id, emitter);
                match handshake {
                    Handshake::Ready(session, token) => {
                        if token.load(Ordering::SeqCst) {
                            let _ = session.shutdown.send(());
                            self.cleanup_handshake(connection_id, &token);
                            return Ok(
                                json!({ "success": false, "message": "Connection was cancelled during handshake." }),
                            );
                        }
                        let info = session.info.clone();
                        let dynamic = dynamic_mode(&params);
                        let registered = register_session(session, dynamic);
                        let ready = json!({"resizeMode": if dynamic { "dynamic" } else { "fixed" }, "sessionId": registered.id});
                        self.sessions
                            .lock()
                            .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?
                            .insert(connection_id.to_string(), registered);
                        if let Some(emitter) = emitter {
                            let _ = emitter.event(frame::STATUS_EVENT, frame::status_payload(connection_id, "session-ready", Some(ready)));
                        }
                        self.cleanup_handshake(connection_id, &token);
                        tracing::info!(
                            peer = %info.dial_host,
                            port = info.dial_port,
                            width = info.width,
                            height = info.height,
                            color = info.color_depth,
                            compression = %info.compression,
                            "RDP protocol negotiation succeeded"
                        );
                        Ok(json!({
                            "success": true,
                            "desktop": {
                                "width": info.width,
                                "height": info.height,
                                "colorDepth": info.color_depth,
                                "compression": info.compression
                            }
                        }))
                    }
                    Handshake::Failed(message) => {
                        tracing::warn!("RDP connect failed: {message}");
                        Ok(json!({ "success": false, "message": message }))
                    }
                    Handshake::Running => Ok(json!({
                        "success": true,
                        "async": true,
                        "message": "RDP handshake is running in the background."
                    })),
                }
            }
            "connection/disconnect" => {
                let connection_id = connection_id(&params)?;
                // Cancel an in-flight handshake before removing the session:
                // the handshake thread checks the flag and tears its session
                // down instead of publishing it.
                if let Some(flag) = self
                    .handshakes
                    .lock()
                    .map_err(|_| PluginError::new(-32000, "Handshake registry is poisoned"))?
                    .remove(connection_id)
                {
                    flag.store(true, Ordering::SeqCst);
                }
                let session = self
                    .sessions
                    .lock()
                    .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?
                    .remove(connection_id);
                if let Some(session) = session {
                    let _ = session.session.shutdown.send(());
                    // Do not join: the task exits on its own once the socket is
                    // closed; blocking would stall the worker pool.
                }
                Ok(json!({ "success": true }))
            }
            "connection/action" => {
                let action_id = params
                    .get("action")
                    .and_then(|action| action.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| PluginError::new(-32602, "Missing action id"))?;
                // Phase 3 action: force a size renegotiation with the given canvas size.
                if action_id == "io.github.caizong10.rdp.resize" {
                    let connection_id = connection_id(&params)?;
                    let width = params.get("width").and_then(Value::as_u64).unwrap_or(0) as u16;
                    let height = params.get("height").and_then(Value::as_u64).unwrap_or(0) as u16;
                    if width == 0 || height == 0 {
                        return Ok(
                            json!({ "success": false, "message": "resize requires width and height" }),
                        );
                    }
                    let sent = self
                        .sessions
                        .lock()
                        .map_err(|_| PluginError::new(-32000, "Session registry is poisoned"))?
                        .get(connection_id)
                        .map(|session| {
                            session
                                .session.commands
                                .send(SessionCommand::Resize { width, height })
                                .is_ok()
                        })
                        .unwrap_or(false);
                    return Ok(json!({
                        "success": sent,
                        "message": if sent { format!("resize queued to {width}x{height}") } else { "no active session for resize".to_string() }
                    }));
                }
                Ok(json!({ "success": true, "message": "Action completed." }))
            }
            "dbx-plugin-rdp/ping" => Ok(json!({
                "ok": true,
                "plugin": "io.github.caizong10.rdp",
                "language": "rust",
                "connectionId": params.get("connectionId").cloned().unwrap_or(Value::Null)
            })),
            _ => Err(PluginError::method_not_found(method)),
        }
    }

    /// Binary input frames from the UI (channel "input").
    pub fn deliver_input(&self, message: InputMessage) {
        self.deliver_input_with_emitter(message, None);
    }

    fn deliver_input_with_emitter(&self, message: InputMessage, emitter: Option<&PluginEmitter>) {
        let Ok(mut guard) = self.sessions.lock() else { return; };
        let Some(registered) = guard.get_mut(&message.connection_id) else {
            if matches!(message.event, InputKind::SessionReadyQuery) {
                if let Some(emitter) = emitter {
                    let _ = emitter.event(frame::STATUS_EVENT, frame::status_payload(&message.connection_id, "session-unavailable", None));
                }
            }
            return;
        };
        let mut notification = None;
        let command = match message.event {
            InputKind::SessionReadyQuery => {
                notification = Some(("session-ready", json!({"sessionId": registered.id, "resizeMode": if registered.dynamic { "dynamic" } else { "fixed" }})));
                None
            }
            InputKind::InitialResize { width, height, session_id } => {
                let valid = session_id.as_slice() == registered.id.as_bytes()
                    && registered.dynamic && !registered.initial_resize_used
                    && (256..=8192).contains(&width) && (200..=8192).contains(&height)
                    && width % 4 == 0 && height % 4 == 0
                    && u32::from(width) * u32::from(height) <= 3840 * 2160;
                let accepted = valid && registered.session.commands.send(SessionCommand::ApplySize { width, height }).is_ok();
                if accepted { registered.initial_resize_used = true; }
                notification = Some(("initial-resize-result", json!({"sessionId": String::from_utf8_lossy(&session_id), "accepted": accepted})));
                None
            }
            InputKind::ApplySize { width, height } => {
                registered.initial_resize_used = true;
                Some(SessionCommand::ApplySize { width, height })
            }
            InputKind::Resize { width, height } => Some(SessionCommand::Resize { width, height }),
            other => Some(SessionCommand::Input(other)),
        };
        if let Some(command) = command { let _ = registered.session.commands.send(command); }
        drop(guard);
        if let (Some(emitter), Some((state, detail))) = (emitter, notification) {
            let _ = emitter.event(frame::STATUS_EVENT, frame::status_payload(&message.connection_id, state, Some(detail)));
        }
    }
}

impl PluginHandler for RdpPlugin {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        self.dispatch_with_emitter(method, params, Some(emitter))
    }

    fn handle_binary(
        &self,
        channel: &str,
        data: Vec<u8>,
        emitter: &PluginEmitter,
    ) -> Result<(), PluginError> {
        if channel != frame::INPUT_CHANNEL {
            return Err(PluginError::new(
                -32601,
                format!("Unknown binary channel: {channel}"),
            ));
        }
        let message = frame::parse_input(&data)
            .map_err(|error| PluginError::new(-32600, format!("Malformed input frame: {error}")))?;
        self.deliver_input_with_emitter(message, Some(emitter));
        Ok(())
    }
}

fn connection_id(params: &Value) -> Result<&str, PluginError> {
    params
        .get("connection")
        .and_then(|connection| connection.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::new(-32602, "Missing connection id"))
}

fn has_password(connection: &Value) -> bool {
    let direct = connection.get("password").and_then(Value::as_str);
    let stored = connection
        .get("connection_secrets")
        .and_then(|secrets| secrets.get("password"))
        .and_then(Value::as_str);
    direct.is_some_and(|value| !value.is_empty()) || stored.is_some_and(|value| !value.is_empty())
}

fn validate_form(params: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let connection = params.get("connection").cloned().unwrap_or_default();

    let host = connection
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        problems.push("Host is required.".to_string());
    }

    match connection.get("port").and_then(Value::as_u64) {
        None => problems.push("Port is required.".to_string()),
        Some(port) if port == 0 || port > 65535 => {
            problems.push("Port must be between 1 and 65535.".to_string());
        }
        Some(_) => {}
    }

    let username = connection
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if username.is_empty() {
        problems.push("Username is required.".to_string());
    }

    // Password presence is validated without ever echoing the value back.
    if !has_password(&connection) {
        problems.push("Password is required.".to_string());
    }

    problems
}
