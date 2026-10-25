//! Real IronRDP connection engine.
//!
//! Blocking IronRDP stack on a dedicated supervisor thread per session:
//! TCP -> TLS (rustls+ring) -> cert trust (host/requestUserInput) ->
//! CredSSP/NLA -> active session loop. Phase 3 adds: desktop frames pushed to
//! the UI ("frame" binary channel), input events consumed from the UI
//! ("input"), dynamic resize through DisplayControl with a
//! reconnect-with-new-size fallback, and exponential-backoff auto-reconnect.
//!
//! Logs go to stderr only; stdout belongs to the framed sidecar protocol.
//! Credentials live only in this module's session params and are never logged.

use std::io::Write as _;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbx_plugin_sdk::PluginEmitter;
use serde_json::Value;

use crate::endpoint;
use crate::frame::{self, InputKind};
use crate::handler::{ConnectEngine, ConnectFailure, RdpSession, SessionInfo};
use crate::pins::PinStore;

pub struct IronRdpEngine;

pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Commands the UI can queue into a live session.
pub enum SessionCommand {
    Input(InputKind),
    Resize { width: u16, height: u16 },
    /// User pressed Apply: reconnect immediately at this desktop size.
    ApplySize { width: u16, height: u16 },
}

/// Everything needed to (re)open a session. Owned by the supervisor thread.
struct SessionParams {
    connection_id: String,
    dial_host: String,
    dial_port: u16,
    username: String,
    password: String,
    domain: Option<String>,
    color_depth: u8,
    ignore_cert: bool,
    /// 1 = dynamic resize allowed, 0 = pinned. Cleared after the first
    /// reconnect-with-new-size fallback so a server without DisplayControl
    /// cannot drive an endless reconnect loop.
    dynamic_resize: AtomicU8,
    width: AtomicU16,
    height: AtomicU16,
    /// 0 = none, 1 = a dynamic resize is pending server acknowledgment.
    resize_pending: AtomicU8,
    /// Clipboard backend -> session loop command queue (survives reconnects;
    /// every open_session builds a fresh TextClipboard on this sender).
    clip_tx: mpsc::SyncSender<crate::clipboard::ClipCommand>,
    /// Format id the server advertises for Unicode text (0 = not seen yet).
    /// Long-format-name servers register CF_UNICODETEXT under a dynamic id.
    remote_text_format: Arc<AtomicU32>,
    /// Format id the server advertises for the file list (0 = not seen yet).
    remote_file_format: Arc<AtomicU32>,
    /// Consecutive FAIL count for the current file-list paste.
    file_retry_attempt: Arc<AtomicU32>,
}

type UpgradedFramed = ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

/// (info, framed, connection_result) after a successful handshake.
type Opened = (SessionInfo, UpgradedFramed, ironrdp_connector::ConnectionResult);

impl ConnectEngine for IronRdpEngine {
    fn connect(
        &self,
        params: &Value,
        emitter: Option<&PluginEmitter>,
    ) -> Result<RdpSession, ConnectFailure> {
        install_crypto_provider();
        let (dial_host, dial_port) = endpoint::resolve(params).map_err(ConnectFailure)?;
        let connection = params
            .get("connection")
            .ok_or_else(|| ConnectFailure("Missing connection payload".to_string()))?;
        let connection_id = connection
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ConnectFailure("Missing connection id".to_string()))?
            .to_string();

        let username = field_str(connection, "username").ok_or_else(|| ConnectFailure("Username is required.".to_string()))?;
        let password = connection
            .get("connection_secrets")
            .and_then(|secrets| secrets.get("password"))
            .and_then(Value::as_str)
            .or_else(|| connection.get("password").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ConnectFailure("Password is required.".to_string()))?;

        let (clip_tx, clip_rx) = mpsc::sync_channel::<crate::clipboard::ClipCommand>(16);
        let params = Arc::new(SessionParams {
            connection_id: connection_id.clone(),
            dial_host: dial_host.clone(),
            dial_port,
            username,
            password: password.to_string(),
            domain: config_str(connection, "domain").filter(|value| !value.is_empty()),
            color_depth: config_str(connection, "color_depth")
                .and_then(|value| value.parse::<u8>().ok())
                .unwrap_or(16),
            ignore_cert: config_bool(connection, "ignore_cert"),
            dynamic_resize: AtomicU8::new(
                u8::from(config_str(connection, "resize_mode").as_deref() != Some("fixed")),
            ),
            width: AtomicU16::new(config_u64(connection, "width").unwrap_or(1280) as u16),
            height: AtomicU16::new(config_u64(connection, "height").unwrap_or(800) as u16),
            resize_pending: AtomicU8::new(0),
            clip_tx,
            remote_text_format: Arc::new(AtomicU32::new(0)),
            remote_file_format: Arc::new(AtomicU32::new(0)),
            file_retry_attempt: Arc::new(AtomicU32::new(0)),
        });

        let emitter = emitter.cloned();
        // First handshake is synchronous so failures surface to the host.
        let (info, framed, result) = open_session(&params, emitter.as_ref())?;
        let peer_addr = info.dial_host.clone() + ":" + &info.dial_port.to_string();

        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let (commands_tx, commands_rx) = mpsc::channel::<SessionCommand>();

        let task = std::thread::Builder::new()
            .name(format!("dbx-rdp-{connection_id}"))
            .spawn(move || {
                session_supervisor(params, framed, result, emitter, commands_rx, clip_rx, shutdown_rx)
            })
            .map_err(|e| ConnectFailure(format!("Failed to start session task: {e}")))?;

        Ok(RdpSession {
            info,
            shutdown: shutdown_tx,
            commands: commands_tx,
            task,
            peer_addr,
        })
    }
}

fn open_session(
    params: &Arc<SessionParams>,
    emitter: Option<&PluginEmitter>,
) -> Result<Opened, ConnectFailure> {
    use ironrdp_blocking::{connect_begin, connect_finalize, mark_as_upgraded, Framed as BlockingFramed};

    let width = params.width.load(Ordering::Relaxed);
    let height = params.height.load(Ordering::Relaxed);

    let addr = (params.dial_host.as_str(), params.dial_port)
        .to_socket_addrs()
        .map_err(|e| ConnectFailure(format!("DNS lookup for endpoint failed: {e}")))?
        .next()
        .ok_or_else(|| ConnectFailure("Endpoint did not resolve to an address".to_string()))?;
    let peer_addr = addr.to_string();

    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .map_err(|e| ConnectFailure(format!("TCP connect to {peer_addr} failed: {e}")))?;
    // Interactive input must never wait for Nagle to coalesce small packets.
    let _ = tcp.set_nodelay(true);
    // Bound every handshake read (X.224, TLS, CredSSP) so a half-open peer
    // cannot pin the background handshake thread forever. run_active_session
    // replaces this with its own short poll timeout after the upgrade.
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(15)));
    let client_addr = tcp
        .local_addr()
        .map_err(|e| ConnectFailure(format!("Local address unavailable: {e}")))?;

    let mut framed = BlockingFramed::new(tcp.try_clone().map_err(|e| ConnectFailure(e.to_string()))?);
    let config = build_config(
        params.username.clone(),
        params.password.clone(),
        params.domain.clone(),
        width,
        height,
    );
    let mut connector = ironrdp_connector::ClientConnector::new(config, client_addr);
    connector = connector.with_static_channel(ironrdp_dvc::DrdynvcClient::new().with_dynamic_channel(
        ironrdp_displaycontrol::client::DisplayControlClient::new(|_| Ok(Vec::new())),
    ));
    connector = connector.with_static_channel(ironrdp_cliprdr::Cliprdr::new(Box::new(
        crate::clipboard::TextClipboard::new(
            params.clip_tx.clone(),
            Arc::clone(&params.remote_text_format),
            // Share ONE atomic with the session loop: the backend stores the
            // chosen file format id on remote copy, and the deferred render
            // path reads it for initiate_paste. Two separate atomics left
            // the session side forever 0, pasting the wrong format id.
            Arc::clone(&params.remote_file_format),
            Arc::clone(&params.file_retry_attempt),
        ),
    )));
    let should_upgrade = connect_begin(&mut framed, &mut connector)
        .map_err(|e| ConnectFailure(format!("RDP connect sequence failed: {e}")))?;

    let initial_stream = framed.into_inner_no_leftover();
    let (tls_stream, server_public_key, fingerprint) = tls_upgrade(initial_stream, &params.dial_host)?;
    let fingerprint_display = endpoint::format_fingerprint(&fingerprint);

    let mut pins = PinStore::default_location();
    let trusted = pins.is_trusted(&params.dial_host, &fingerprint_display);
    let mut accepted = params.ignore_cert || trusted;
    let mut trust_via = if params.ignore_cert {
        "ignore_cert"
    } else if trusted {
        "pinned"
    } else {
        ""
    };
    if !accepted {
        if crate::handler::host_can_prompt() {
            let answer = crate::handler::ask_host_to_trust(
                params.dial_host.clone(),
                fingerprint_display.clone(),
            );
            accepted = crate::handler::should_accept_certificate(false, false, Some(answer));
            trust_via = if accepted { "prompt-approved" } else { "prompt-denied" };
        } else {
            // Trust on first use: this host build does not advertise the
            // approval prompt (host/requestUserInput). Accept once and record
            // the fingerprint so every later connect is validated against it.
            accepted = true;
            trust_via = "trust-on-first-use";
        }
    }
    if !accepted {
        tracing::warn!(
            "RDP certificate rejected for {}: SHA-256 {} - the approval prompt was shown but not accepted",
            params.dial_host,
            fingerprint_display
        );
        return Err(ConnectFailure(
            "Server certificate is not trusted; connection aborted (approval was not granted)."
                .to_string(),
        ));
    }
    if !trusted {
        tracing::info!(
            "RDP certificate trusted for {}: SHA-256 {} via {}",
            params.dial_host,
            fingerprint_display,
            trust_via
        );
        if !params.ignore_cert {
            pins.trust(&params.dial_host, &fingerprint_display);
        }
    }

    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);
    let mut upgraded_framed = BlockingFramed::new(tls_stream);
    let mut network_client = OfflineKdcNetworkClient;
    let server_name = ironrdp_connector::ServerName::new(params.dial_host.clone());
    let connection_result = connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        server_name,
        server_public_key,
        None,
    )
    .map_err(|e| ConnectFailure(describe_negotiation_failure(&e)))?;

    let negotiated_width = connection_result.desktop_size.width;
    let negotiated_height = connection_result.desktop_size.height;
    params.width.store(negotiated_width, Ordering::Relaxed);
    params.height.store(negotiated_height, Ordering::Relaxed);
    params.resize_pending.store(0, Ordering::Relaxed);
    let compression = format!("{:?}", connection_result.compression_type).to_lowercase();
    tracing::info!(
        "RDP handshake complete: security=NLA/TLS peer={peer_addr} desktop={negotiated_width}x{negotiated_height} compression={compression} cert_fingerprint={fingerprint_display}"
    );

    let info = SessionInfo {
        dial_host: params.dial_host.clone(),
        dial_port: params.dial_port,
        width: negotiated_width,
        height: negotiated_height,
        color_depth: params.color_depth,
        compression,
    };

    if let Some(emitter) = emitter {
        let _ = emitter.event(
            frame::STATUS_EVENT,
            frame::status_payload(&params.connection_id, "connected", None),
        );
        let _ = emitter.event(
            frame::DESKTOP_EVENT,
            serde_json::json!({
                "connectionId": params.connection_id,
                "width": negotiated_width,
                "height": negotiated_height,
            }),
        );
    }
    Ok((info, upgraded_framed, connection_result))
}

/// Translate a CredSSP/NLA + session negotiation failure into a message the
/// user can act on. An authentication failure here almost always means bad
/// credentials (or a locked/expired account), which the raw IronRDP error
/// hides behind a generic "CredSSP" tag.
fn describe_negotiation_failure<E: std::fmt::Display>(error: &E) -> String {
    let raw = error.to_string();
    let lowered = raw.to_ascii_lowercase();
    let looks_like_auth = lowered.contains("credssp")
        || lowered.contains("ntlm")
        || lowered.contains("sspi")
        || lowered.contains("logon")
        || lowered.contains("authentication")
        || lowered.contains("access denied")
        || lowered.contains("nla");
    if looks_like_auth {
        "登录认证失败：用户名或密码不正确（或该账户被锁定/已过期）。请检查连接配置中的凭据后重试。".to_string()
    } else {
        format!("CredSSP/NLA 或会话协商失败：{raw}")
    }
}

enum SessionEnd {
    UserRequested,
    Lost(String),
    /// Reconnect immediately with the resized desktop (fixed servers).
    ResizeFallback(String),
    /// User clicked Apply: reconnect at the new size with no backoff.
    ApplySize,
}

/// Run one CLIPRDR operation on the active stage's clipboard processor and
/// send the resulting SVC messages on the channel. Borrow-scoped so the stage
/// stays usable after the processor borrow ends.
macro_rules! clip_op {
    ($stage:expr, $framed:expr, $label:expr, $body:expr) => {{
        let stage: &mut ironrdp_session::ActiveStage = $stage;
        let outcome = stage
            .get_svc_processor_mut::<ironrdp_cliprdr::CliprdrClient>()
            .map(|cliprdr| ($body)(cliprdr));
        match outcome {
            Some(Ok(messages)) => {
                crate::diag::note(&format!("{}: processor ok", $label));
                let messages: Vec<ironrdp_svc::SvcMessage> = messages.into();
                for message in &messages {
                    if let Ok(pdu) = message.encode_unframed_pdu() {
                        crate::clip_dump::dump("TX-PDU", &pdu);
                    }
                }
                let messages = ironrdp_svc::SvcProcessorMessages::<ironrdp_cliprdr::CliprdrClient>::new(messages);
                match stage.process_svc_processor_messages(messages) {
                    Ok(frame) => {
                        crate::diag::note(&format!("{}: wrote {} byte(s) to channel", $label, frame.len()));
                        let _ = $framed.write_all(&frame);
                    }
                    Err(error) => {
                        crate::diag::note(&format!("{}: encode failed: {error}", $label));
                        tracing::warn!("{} encode failed: {error}", $label);
                    }
                }
            }
            Some(Err(error)) => {
                crate::diag::note(&format!("{}: processor returned error: {error}", $label));
                tracing::warn!("{} failed: {error}", $label);
            }
            None => {
                crate::diag::note(&format!("{}: DROPPED, CliprdrClient processor absent from stage", $label));
                tracing::debug!("{} dropped: CLIPRDR channel unavailable", $label);
            }
        }
    }};
}

fn session_supervisor(
    params: Arc<SessionParams>,
    framed: UpgradedFramed,
    result: ironrdp_connector::ConnectionResult,
    emitter: Option<PluginEmitter>,
    commands: mpsc::Receiver<SessionCommand>,
    clip_rx: mpsc::Receiver<crate::clipboard::ClipCommand>,
    shutdown: mpsc::Receiver<()>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut attempt = 0u32;
    // True right after the user pressed Apply: the first reconnect attempt is
    // reported as "connecting" instead of a scary "reconnecting (attempt 1)".
    let mut user_initiated = false;
    let mut next: Option<(UpgradedFramed, ironrdp_connector::ConnectionResult)> = Some((framed, result));

    loop {
        let (mut framed, result) = match next.take() {
            Some(pair) => pair,
            None => {
                // Transport died or a resize needs a fresh desktop: reconnect
                // with exponential backoff, bounded so dead endpoints cannot
                // loop forever without user intent.
                loop {
                    if shutdown.try_recv().is_ok() {
                        emit_status(&emitter, &params, "disconnected", None);
                        return;
                    }
                    attempt += 1;
                    let state = if attempt == 1 && user_initiated { "connecting" } else { "reconnecting" };
                    emit_status(&emitter, &params, state, Some(attempt));
                    user_initiated = false;
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    match open_session(&params, emitter.as_ref()) {
                        Ok((_, new_framed, new_result)) => {
                            backoff = Duration::from_millis(500);
                            attempt = 0;
                            next = Some((new_framed, new_result));
                            break;
                        }
                        Err(error) => {
                            tracing::warn!("reconnect attempt {attempt} failed: {}", error.0);
                            emit_status(&emitter, &params, "retry-failed", Some(attempt));
                            if attempt >= 10 {
                                emit_status(&emitter, &params, "disconnected", None);
                                return;
                            }
                        }
                    }
                }
                continue;
            }
        };

        let reason = run_active_session(
            &params,
            &mut framed,
            result,
            emitter.as_ref(),
            &commands,
            &clip_rx,
            &shutdown,
        );
        if shutdown.try_recv().is_ok() {
            emit_status(&emitter, &params, "disconnected", None);
            return;
        }
        match reason {
            SessionEnd::UserRequested => {
                tracing::info!("RDP session task stopping on request");
                emit_status(&emitter, &params, "disconnected", None);
                return;
            }
            SessionEnd::ResizeFallback(cause) => {
                tracing::info!("resize fallback: {cause}; reopening with new size");
            }
            SessionEnd::ApplySize => {
                // User intent: skip the backoff ladder entirely.
                backoff = Duration::from_millis(0);
                attempt = 0;
                user_initiated = true;
            }
            SessionEnd::Lost(cause) => {
                tracing::warn!("RDP session lost: {cause}; will reconnect");
                emit_status(&emitter, &params, "connection-lost", None);
            }
        }
        next = None;
    }
}

/// Decoupled frame delivery: the session thread encodes tiles and pushes the
/// bytes onto a byte-bounded queue; a pump thread drains it into the host's
/// binary channel. Without this, a burst of graphics updates (fast scrolling)
/// filled the host pipe, the session thread blocked inside emitter.binary(),
/// and neither socket reads nor queued input were processed until the pipe
/// drained — experienced as "mouse disconnects while scrolling". When the
/// queue exceeds its byte budget the oldest frames are dropped so the newest
/// pixels always go out: a stale band is a cosmetic artifact, a stalled
/// session is not.
const FRAME_QUEUE_BYTES: usize = 24 * 1024 * 1024;

pub struct FramePump {
    state: Arc<PumpState>,
}

struct PumpState {
    queue: std::sync::Mutex<(std::collections::VecDeque<Vec<u8>>, usize)>,
    cvar: std::sync::Condvar,
    stop: std::sync::atomic::AtomicBool,
}

impl FramePump {
    fn spawn(emitter: PluginEmitter) -> Self {
        let state = Arc::new(PumpState {
            queue: std::sync::Mutex::new((std::collections::VecDeque::new(), 0usize)),
            cvar: std::sync::Condvar::new(),
            stop: std::sync::atomic::AtomicBool::new(false),
        });
        let pump_state = Arc::clone(&state);
        let _ = std::thread::Builder::new()
            .name("dbx-rdp-frame-pump".to_string())
            .spawn(move || loop {
                let frame = {
                    let mut guard = pump_state
                        .queue
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    while guard.0.is_empty() && !pump_state.stop.load(Ordering::Relaxed) {
                        guard = pump_state.cvar.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    if pump_state.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let frame = guard.0.pop_front().expect("non-empty");
                    guard.1 -= frame.len();
                    frame
                };
                if emitter.binary(frame::FRAME_CHANNEL, &frame).is_err() {
                    return; // host pipe gone; session teardown will stop us
                }
            });
        Self { state }
    }

    fn push(&self, bytes: Vec<u8>) {
        if self.state.stop.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self
            .state
            .queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Drop whole oldest frames until the new one fits within budget.
        while guard.1 + bytes.len() > FRAME_QUEUE_BYTES {
            match guard.0.pop_front() {
                Some(old) => guard.1 -= old.len(),
                None => break,
            }
        }
        guard.1 += bytes.len();
        guard.0.push_back(bytes);
        drop(guard);
        self.state.cvar.notify_one();
    }
}

impl Drop for FramePump {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        self.state.cvar.notify_all();
        // Not joined: if the pump is parked writing a full host pipe, waiting
        // would hang session teardown. The thread exits on the next iteration.
    }
}

fn emit_status(emitter: &Option<PluginEmitter>, params: &SessionParams, state: &str, attempt: Option<u32>) {
    if let Some(emitter) = emitter {
        let detail = attempt.map(|n| serde_json::json!({ "attempt": n }));
        let _ = emitter.event(
            frame::STATUS_EVENT,
            frame::status_payload(&params.connection_id, state, detail),
        );
    }
}

fn run_active_session(
    params: &Arc<SessionParams>,
    framed: &mut UpgradedFramed,
    connection_result: ironrdp_connector::ConnectionResult,
    emitter: Option<&PluginEmitter>,
    commands: &mpsc::Receiver<SessionCommand>,
    clip_rx: &mpsc::Receiver<crate::clipboard::ClipCommand>,
    shutdown: &mpsc::Receiver<()>,
) -> SessionEnd {
    use ironrdp_graphics::image_processing::PixelFormat;
    use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};

    crate::diag::reset();
    crate::diag::note(&format!(
        "session start: {}x{} channels={}",
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
        connection_result.static_channels.channel_ids().count()
    ));

    let (stream, _leftover) = framed.get_inner_mut();
    // Short poll: UI input is drained at the top of each loop iteration, so
    // the socket read timeout bounds how long a newly queued mouse move can
    // wait while we are blocked reading. 5ms keeps that imperceptible at
    // negligible idle CPU cost.
    let _ = stream.sock.set_read_timeout(Some(Duration::from_millis(5)));

    let mut image = ironrdp_session::image::DecodedImage::new(
        PixelFormat::RgbA32,
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );
    // CLIPRDR channel id (RX dump filter): resolved BEFORE the channel set
    // is moved into the stage.
    let cliprdr_channel_id = connection_result
        .static_channels
        .get_channel_id_by_type::<ironrdp_cliprdr::CliprdrClient>();
    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    // CLIPRDR init: ironrdp's client start() sends nothing and passively waits
    // for the server's MonitorReady, but many Windows servers in turn wait for
    // the client to send Capabilities first — neither moves, the channel never
    // reaches Ready, and every copy/paste is silently dropped by require_ready.
    // Kick it: in Initialization state initiate_copy() bundles Capabilities +
    // TemporaryDirectory + FormatList, which is the client's opening move.
    clip_op!(
        &mut active_stage,
        framed,
        "cliprdr client init",
        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
            .initiate_copy(&crate::clipboard::supported_clipboard_formats())
    );
    // The opening handshake above is done; file advertise/paste operations
    // queued before this point must wait — see the clip_ready gate in the
    // command pump (a copy detected between process start and channel Ready
    // used to fail with "clipboard channel is not in Ready state").
    let mut clip_ready = false;

    // Decouple frame delivery from this thread so a burst of graphics updates
    // (fast scrolling) can never block socket reads or input handling.
    let pump = emitter.map(|emitter| FramePump::spawn(emitter.clone()));

    let activation_factory = connection_result.activation_factory.clone();
    let mut seq_counter: u32 = 0;
    // Lazily (re)built: a transient arboard failure must not disable clipboard
    // sync for the whole session lifetime.
    let mut clipboard: Option<arboard::Clipboard> = None;
    let mut last_clip_poll = Instant::now() - Duration::from_secs(1);
    let mut last_sent_text: Option<String> = None;
    let mut last_poll_error: Option<String> = None;

    // ---- file clipboard state (Windows-native path; text path untouched) ----
    // Outbound: files we advertised to the remote, keyed by list index.
    let mut outbound = crate::clip_transfer::OutboundTable::new();
    let mut stream_index = crate::clip_transfer::StreamIndexMap::new();
    // What our last local CF_HDROP snapshot looked like (sorted joined
    // paths), so the 300ms poll only advertises on a real change.
    let mut last_local_files: Option<String> = None;
    // Echo guard: the joined snapshot written by our own Publish event. When
    // the poll reads it back, it must not re-advertise to the remote (the
    // remote just pasted those files; echoing them re-enters its clipboard).
    let mut suppress_publish_echo: Option<String> = None;
    // Event-driven clipboard changes (Windows: WM_CLIPBOARDUPDATE watcher;
    // other platforms: no events, the timed poll below keeps working).
    let (clip_watch_tx, clip_watch_rx) = std::sync::mpsc::channel::<crate::clip_watch::ClipboardEvent>();
    let _watcher = crate::clip_watch::spawn(clip_watch_tx);
    // True once the watcher reported a failure: fall back to poll-only.
    let mut watcher_failed = false;
    // Clipboard access is paused until this instant after each publish
    // (see the quiet-window comment at the poll site).
    let mut publish_quiet_until: Option<Instant> = None;
    // ---- deferred-offer plumbing ----
    // Legacy render-pull channel: nothing sends on it anymore (the render
    // never blocks); the drain below keeps any stray sender harmless.
    let (_render_pull_tx, render_pull_rx) =
        std::sync::mpsc::channel::<()>();
    // True between arming the delayed offer and the background pull's
    // publish; while set, a completed download replaces the delayed entry
    // with the real HDROP (mark_background_published).
    let mut awaiting_render = false;
    // stream ids for downloads; odd numbers so they can never collide with
    // anything the server picks (server-chosen ids are echoed verbatim).
    let mut next_stream_id: u32 = 1;
    let (file_events_tx, file_events_rx) = std::sync::mpsc::sync_channel::<crate::clip_transfer::TransferEvent>(32);
    let (file_req_tx, file_req_rx) = std::sync::mpsc::sync_channel::<ironrdp_cliprdr::pdu::FileContentsRequest>(64);
    let staging_dir = std::env::temp_dir().join(format!("dbx-rdp-clip-{}", params.connection_id));
    let _ = std::fs::create_dir_all(&staging_dir);
    let transfer = crate::clip_transfer::spawn_download_thread(
        staging_dir,
        file_events_tx,
        file_req_tx,
    );
    let (virtual_reader, mut virtual_bridge) = crate::clip_transfer::VirtualTransferBridge::new(Duration::from_secs(30));
    let mut virtual_generation = 0u64;
    let mut fallback_descriptors = Vec::new();
    let (virtual_failure_tx, virtual_failure_rx) = std::sync::mpsc::sync_channel::<u64>(1);
    struct ClipboardSessionGuard;
    impl Drop for ClipboardSessionGuard {
        fn drop(&mut self) {
            #[cfg(windows)]
            crate::clip_owner::cancel_virtual(0);
        }
    }
    let _clipboard_session = ClipboardSessionGuard;

    loop {
        if shutdown.try_recv().is_ok() {
            return SessionEnd::UserRequested;
        }

        // Drain every queued command at once and coalesce pointer moves:
        // only the latest move position matters, so a burst of stale moves
        // never delays the current one behind per-event processing.
        let mut batch: Vec<SessionCommand> = Vec::with_capacity(32);
        while let Ok(command) = commands.try_recv() {
            batch.push(command);
        }
        let mut index = 0usize;
        while index < batch.len() {
            let is_move = matches!(batch[index], SessionCommand::Input(InputKind::Mouse { .. }));
            if is_move {
                // Collapse the run of consecutive moves down to the last.
                while index + 1 < batch.len()
                    && matches!(batch[index + 1], SessionCommand::Input(InputKind::Mouse { .. }))
                {
                    index += 1;
                }
            }
            match &batch[index] {
                SessionCommand::Input(kind) => {
                    if let crate::frame::InputKind::PullFiles = kind {
                        // Workbench "pull remote files" button: re-run the
                        // deferred arm + background pull. A folder copy whose
                        // server-side render arrived late becomes fetchable
                        // with one explicit click instead of re-copying.
                        crate::diag::note("workbench: pull-remote-files button");
                        let _ = params.clip_tx.try_send(
                            crate::clipboard::ClipCommand::OfferRemoteFiles,
                        );
                        // A fresh retry ladder: the previous one may have
                        // exhausted itself while the server was still busy.
                        params.file_retry_attempt.store(0, Ordering::Relaxed);
                        let dynamic = params.remote_file_format.load(Ordering::Relaxed);
                        let format = ironrdp_cliprdr::pdu::ClipboardFormatId::new(if dynamic == 0 {
                            15
                        } else {
                            dynamic
                        });
                        clip_op!(
                            &mut active_stage,
                            framed,
                            "cliprdr initiate_paste(pull button)",
                            |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(format)
                        );
                    }
                    if let Some(events) = input_to_fastpath(kind) {
                        match active_stage.process_fastpath_input(&mut image, &events) {
                            Ok(outputs) => {
                                if let Err(error) = drive_response_frames(framed, outputs) {
                                    return SessionEnd::Lost(error);
                                }
                            }
                            Err(error) => tracing::warn!("input processing failed: {error}"),
                        }
                    }
                }
                SessionCommand::Resize { width, height } => {
                    let (width, height) = (*width, *height);
                    let current = (image.width(), image.height());
                    if (width, height) == current {
                        index += 1;
                        continue;
                    }
                    if params.dynamic_resize.load(Ordering::Relaxed) == 0 {
                        tracing::debug!("resize ignored: session is pinned to a fixed size");
                        index += 1;
                        continue;
                    }
                    params.width.store(width, Ordering::Relaxed);
                    params.height.store(height, Ordering::Relaxed);
                    match active_stage.encode_resize(u32::from(width), u32::from(height), None, None) {
                        Some(Ok(frames)) => {
                            params.resize_pending.store(1, Ordering::Relaxed);
                            if let Err(e) = framed.write_all(&frames) {
                                return SessionEnd::Lost(format!("resize write: {e}"));
                            }
                            tracing::info!("DisplayControl resize requested: {width}x{height}");
                        }
                        Some(Err(error)) => {
                            // One reconnect at the new size, then pin: a server
                            // that refuses DisplayControl must not be re-dialed
                            // forever (each retry re-triggers a UI resize).
                            params.dynamic_resize.store(0, Ordering::Relaxed);
                            tracing::warn!("DisplayControl resize failed: {error}; reconnecting once with new size");
                            return SessionEnd::ResizeFallback(format!("{error}"));
                        }
                        None => {
                            params.dynamic_resize.store(0, Ordering::Relaxed);
                            tracing::info!("DisplayControl unavailable; reconnecting once with size {width}x{height}");
                            return SessionEnd::ResizeFallback("display control unavailable".to_string());
                        }
                    }
                }
                SessionCommand::ApplySize { width, height } => {
                    // Explicit user action (Apply button): pin the session to
                    // this desktop size and reconnect right away, no backoff.
                    params.width.store(*width, Ordering::Relaxed);
                    params.height.store(*height, Ordering::Relaxed);
                    params.dynamic_resize.store(0, Ordering::Relaxed);
                    tracing::info!("apply size requested by user: {width}x{height}; reconnecting now");
                    return SessionEnd::ApplySize;
                }
            }
            index += 1;
        }

        // Clipboard pump: commands queued by the CLIPRDR backend.
        while let Ok(command) = clip_rx.try_recv() {
            use crate::clipboard::ClipCommand;
            // File/remote-paste traffic before the channel reached Ready
            // would only error out ("not in Ready state"); drop it here and
            // re-run the local clipboard once ClipboardReady arrives.
            if !clip_ready {
                match command {
                    ClipCommand::AdvertiseText | ClipCommand::ClipboardReady => {}
                    _ => continue,
                }
            }
            match command {
                ClipCommand::ClipboardReady => {
                    clip_ready = true;
                    crate::diag::note("session loop: clipboard channel Ready; file ops unblocked");
                    // Re-read the local clipboard now: files copied during
                    // the handshake were skipped above.
                    read_local_clipboard(
                        &mut clipboard,
                        &mut last_sent_text,
                        &mut last_poll_error,
                        &mut last_local_files,
                        &mut suppress_publish_echo,
                        &params.clip_tx,
                        &mut active_stage,
                        framed,
                    );
                }
                ClipCommand::SendFormatListResponse => {
                    // FormatListResponse is sent automatically by the crate when it
                    // receives a FormatList from the server. No manual action needed.
                    crate::diag::note("session loop: FormatListResponse (auto-handled by crate)");
                }
                ClipCommand::AdvertiseText => {
                    if let Some(text) = clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                        if last_sent_text.as_deref() != Some(text.as_str()) {
                            last_sent_text = Some(text);
                        }
                    }
                    clip_op!(
                        &mut active_stage,
                        framed,
                        "cliprdr initiate_copy",
                        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                            .initiate_copy(&crate::clipboard::supported_clipboard_formats())
                    );
                }
                ClipCommand::SendText(format_id) => {
                    // 16 = CF_LOCALE: a 4-byte little-endian LCID. Windows
                    // asks for it before converting non-Unicode text; refusing
                    // it breaks the paste, so answer with the system locale.
                    let response = if format_id == 16 {
                        // zh-CN (this deployment is a Chinese Windows host).
                        // We only advertise CF_UNICODETEXT, so this request is
                        // rare; answering keeps a strict server from failing
                        // the whole paste instead of ignoring one format.
                        let lcid = 0x0804u32;
                        ironrdp_cliprdr::pdu::FormatDataResponse::new_data(lcid.to_le_bytes().to_vec())
                    } else {
                        clipboard
                            .as_mut()
                            .and_then(|c| c.get_text().ok())
                            .map(|text| ironrdp_cliprdr::pdu::FormatDataResponse::new_unicode_string(&text))
                            .unwrap_or_else(ironrdp_cliprdr::pdu::FormatDataResponse::new_error)
                    };
                    clip_op!(
                        &mut active_stage,
                        framed,
                        "cliprdr submit_format_data",
                        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                            .submit_format_data(ironrdp_core::IntoOwned::into_owned(response))
                    );
                }
                ClipCommand::LocalFilesSuperseded => {
                    // Remote clipboard moved on: drop the dedupe key and the
                    // outbound handles so a later re-copy re-advertises.
                    last_local_files = None;
                    outbound.replace(Vec::new());
                    virtual_bridge.begin_generation(0, None);
                    #[cfg(windows)]
                    crate::clip_owner::cancel_virtual(virtual_generation);
                }
                ClipCommand::RefuseFormat(format_id) => {
                    // A format we do not carry (e.g. FileNameW probed by
                    // Explorer during a file paste): clean CB_RESPONSE_FAIL
                    // so the remote skips it instead of stalling the paste.
                    crate::diag::note(&format!("refusing format #{format_id}"));
                    clip_op!(
                        &mut active_stage,
                        framed,
                        "cliprdr submit_format_data(refuse)",
                        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                            .submit_format_data(ironrdp_cliprdr::pdu::OwnedFormatDataResponse::new_error())
                    );
                }
                ClipCommand::OfferRemoteFiles => {
                    // Echo guard: right after a local paste rendered our
                    // deferred data, the server announces the clipboard
                    // change back at us (the paste itself changed the remote
                    // session's clipboard). Re-arming would EmptyClipboard
                    // and destroy the data the user just pasted with — the
                    // observed "wrote 3 files, clipboard ends up empty".
                    // Skip re-arm while our render output is live; the next
                    // genuine remote copy clears it via a new offer.
                    if crate::clip_owner::render_recent(3000) {
                        crate::diag::note("session loop: skipping re-arm (echo after our own render)");
                        continue;
                    }
                    // Delayed offer + background pull (zero-block render).
                    // The delayed CF_HDROP keeps Paste enabled; the
                    // background pull (retry ladder for wave-1 FAILs,
                    // re-armed on the wave-2 announcement) stages the real
                    // files and publishes the real HDROP the moment the
                    // server cooperates. WM_RENDERFORMAT never blocks: it
                    // serves the staged data or an empty drop instantly.
                    crate::diag::note("session loop: arming deferred file offer");
                    // Do not publish an empty CF_HDROP placeholder. Publish
                    // metadata once it arrives; first paste then reads bytes.
                    {
                        virtual_bridge.begin_generation(0, None);
                        #[cfg(windows)]
                        crate::clip_owner::cancel_virtual(virtual_generation);
                        awaiting_render = true;
                        let dynamic = params.remote_file_format.load(Ordering::Relaxed);
                        let format = ironrdp_cliprdr::pdu::ClipboardFormatId::new(if dynamic == 0 {
                            15
                        } else {
                            dynamic
                        });
                        crate::diag::note(&format!(
                            "background pull: initiate_paste on format #{}",
                            dynamic
                        ));
                        clip_op!(
                            &mut active_stage,
                            framed,
                            "cliprdr initiate_paste(background)",
                            |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(format)
                        );
                    }
                }
                ClipCommand::FetchRemoteText => {
                    // Request the exact text format id the server advertised
                    // (dynamic under long format names); fall back to
                    // CF_UNICODETEXT only when none was recorded yet.
                    let dynamic = params.remote_text_format.load(Ordering::Relaxed);
                    let format = ironrdp_cliprdr::pdu::ClipboardFormatId::new(if dynamic == 0 {
                        13
                    } else {
                        dynamic
                    });
                    clip_op!(
                        &mut active_stage,
                        framed,
                        "cliprdr initiate_paste",
                        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(format)
                    );
                }
                ClipCommand::RetryPaste { format_id, attempt } => {
                    // Windows servers render file-list data lazily: the
                    // first FormatDataRequest right after a copy frequently
                    // comes back CB_RESPONSE_FAIL. Folder copies in
                    // particular announce in two waves: the immediate
                    // prelude (no FileGroupDescriptorW — every request
                    // FAILs) and the full descriptor announcement tens of
                    // seconds later, once the server-side Explorer finished
                    // enumerating the directory. So: keep retrying through
                    // the whole 30s render window rather than giving up
                    // after a fixed count; the second wave makes it succeed
                    // (verified live: folder pull succeeded 80s late).
                    if attempt >= 30 {
                        crate::diag::note(&format!(
                            "file paste format #{format_id} failed after {attempt} attempts; giving up"
                        ));
                    } else {
                        // 800ms growing to ~2s, capped; the loop exits early
                        // the moment a wave-2 announcement succeeds.
                        let delay = (800u64 + u64::from(attempt) * 50).min(2000);
                        std::thread::sleep(Duration::from_millis(delay));
                        crate::diag::note(&format!(
                            "file paste format #{format_id} attempt {} -> retry",
                            attempt + 1
                        ));
                        clip_op!(
                            &mut active_stage,
                            framed,
                            "cliprdr initiate_paste(retry)",
                            |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(
                                ironrdp_cliprdr::pdu::ClipboardFormatId::new(format_id)
                            )
                        );
                    }
                }
                ClipCommand::RemoteText(text) => {
                    if clipboard.is_none() {
                        clipboard = arboard::Clipboard::new().ok();
                    }
                    let length = text.len();
                    match clipboard.as_mut() {
                        Some(handle) => match handle.set_text(text.as_str()) {
                            Ok(()) => {
                                // Treat it as already advertised so the local
                                // poll does not echo the remote's own copy
                                // straight back to the server.
                                last_sent_text = Some(text);
                                crate::diag::note(&format!("remote text set locally len={length}"));
                            }
                            Err(error) => crate::diag::note(&format!("remote text set FAILED: {error}")),
                        },
                        None => crate::diag::note(&format!("remote text len={length} DROPPED: no arboard handle")),
                    }
                }
                ClipCommand::FetchRemoteFiles(format_id) => {
                    // Ask the server for its FileGroupDescriptorW; the
                    // response is intercepted by Cliprdr and lands in
                    // on_remote_file_list -> RemoteFileList below.
                    let format = ironrdp_cliprdr::pdu::ClipboardFormatId::new(format_id);
                    clip_op!(
                        &mut active_stage,
                        framed,
                        "cliprdr initiate_paste(files)",
                        |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(format)
                    );
                }
                ClipCommand::RemoteFileList { descriptors, data_id } => {
                    virtual_generation = virtual_generation.saturating_add(1);
                    let generation = virtual_generation;
                    virtual_bridge.begin_generation(generation, data_id);
                    fallback_descriptors = descriptors.clone();
                    #[cfg(windows)]
                    {
                        let entries = descriptors.iter().map(|d| crate::clip_virtual::VirtualFileEntry {
                            name: d.name.clone(), relative_path: d.relative_path.clone(), size: d.file_size.unwrap_or(0),
                            is_directory: d.attributes.is_some_and(|a| a.contains(ironrdp_cliprdr::pdu::ClipboardFileAttributes::DIRECTORY)),
                        }).collect();
                        let reader = virtual_reader.clone();
                        let failures = virtual_failure_tx.clone();
                        // Unknown lengths use the compatible staging path;
                        // never label an unknown file as a successful empty file.
                        if descriptors.iter().all(|d| d.file_size.is_some() || d.attributes.is_some_and(|a| a.contains(ironrdp_cliprdr::pdu::ClipboardFileAttributes::DIRECTORY))) {
                            if let Ok(source) = crate::clip_virtual::VirtualClipboard::new(entries, move |index, position, length| {
                                let result = reader.read(generation, index, position, length);
                                if result.is_err() { let _ = failures.try_send(generation); }
                                result
                            }) {
                                if crate::clip_owner::offer_virtual(generation, source) {
                                    awaiting_render = false;
                                    continue;
                                }
                            }
                        }
                    }
                    // Remote copied files: allocate (stream_id, lindex) pairs,
                    // then hand the download thread everything it needs to
                    // drive the chunk ladder.
                    let mut entries = Vec::with_capacity(descriptors.len());
                    for (i, descriptor) in descriptors.iter().enumerate() {
                        let stream_id = next_stream_id;
                        next_stream_id = next_stream_id.wrapping_add(2).max(1);
                        let index = i32::try_from(i).unwrap_or(0);
                        stream_index.assign(stream_id, index);
                        entries.push((stream_id, index, descriptor.clone()));
                    }
                    let count = entries.len();
                    let _ = transfer.send(crate::clip_transfer::TransferCommand::Download(entries));
                    crate::diag::note(&format!("file download started: {count} file(s)"));
                }
                ClipCommand::AdvertiseLocalFiles(roots) => {
                    // Snapshot arrived from the poll: dedupe, expand, announce.
                    let joined = roots
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if last_local_files.as_deref() == Some(joined.as_str()) {
                        // Same file set already advertised; skip.
                    } else {
                        match crate::clip_files::build_descriptors(&roots) {
                            Ok(files) if files.is_empty() => {}
                            Ok(files) => {
                                let count = files.len();
                                let descriptors: Vec<ironrdp_cliprdr::pdu::FileDescriptor> =
                                    files.iter().map(|f| f.descriptor.clone()).collect();
                                outbound.replace(files);
                                last_local_files = Some(joined);
                                clip_op!(
                                    &mut active_stage,
                                    framed,
                                    "cliprdr initiate_file_copy",
                                    |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                                        .initiate_file_copy(descriptors.clone())
                                );
                                crate::diag::note(&format!("advertised {count} local file(s) to remote"));
                            }
                            Err(error) => {
                                crate::diag::note(&format!("build_descriptors failed: {error}"));
                            }
                        }
                    }
                }
                ClipCommand::ServeFileChunk(request) => {
                    // Remote paste is pulling bytes of our advertised files.
                    // The Cliprdr processor validated the index against the
                    // advertised list already; serve from disk and submit.
                    let result = outbound.serve(&request);
                    match result {
                        Ok(Some(response)) => {
                            clip_op!(
                                &mut active_stage,
                                framed,
                                "cliprdr submit_file_contents",
                                |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                                    .submit_file_contents(ironrdp_core::IntoOwned::into_owned(response))
                            );
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            crate::diag::note(&format!("serve file chunk failed: {reason}"));
                            let error = ironrdp_cliprdr::pdu::FileContentsResponse::new_error(request.stream_id);
                            clip_op!(
                                &mut active_stage,
                                framed,
                                "cliprdr submit_file_contents(error)",
                                |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                                    .submit_file_contents(error)
                            );
                        }
                    }
                }
                ClipCommand::DownloadChunk(response) => {
                    // A chunk of an in-flight download arrived; forward to the
                    // download thread, which owns the staging state.
                    let stream_id = response.stream_id();
                    if virtual_bridge.respond(response.clone()) { continue; }
                    // Virtual IDs occupy the upper half; late responses must
                    // not leak into the fallback download state machine.
                    if stream_id >= 0x80000000 { continue; }
                    let _ = transfer.send(crate::clip_transfer::TransferCommand::Chunk {
                        stream_id,
                        response,
                    });
                }
            }
        }

        while let Ok(failed_generation) = virtual_failure_rx.try_recv() {
            if failed_generation != virtual_generation { continue; }
            crate::diag::note(&format!("virtual read failed generation={failed_generation}; staging fallback"));
            virtual_bridge.begin_generation(0, None);
            #[cfg(windows)]
            crate::clip_owner::cancel_virtual(failed_generation);
            let mut entries = Vec::new();
            for (index, descriptor) in fallback_descriptors.iter().enumerate() {
                let id = next_stream_id;
                next_stream_id = next_stream_id.wrapping_add(2).max(1);
                entries.push((id, index as i32, descriptor.clone()));
            }
            let _ = transfer.send(crate::clip_transfer::TransferCommand::Download(entries));
        }
        while let Some(request) = virtual_bridge.next_request() {
            crate::diag::note(&format!("virtual read generation={} stream={} index={} offset={} length={}", virtual_generation, request.stream_id, request.index, request.position, request.requested_size));
            clip_op!(&mut active_stage, framed, "cliprdr virtual file read", |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.request_file_contents(request.clone()));
        }

        // Download-thread -> server pump: FileContentsRequests the thread
        // generated (chunk ladder), sent through the Cliprdr processor so
        // stream ids are tracked and capabilities re-checked.
        while let Ok(request) = file_req_rx.try_recv() {
            clip_op!(
                &mut active_stage,
                framed,
                "cliprdr request_file_contents",
                |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                    .request_file_contents(request.clone())
            );
        }

        // Deferred-render pull pump: the owner-thread callback (inside
        // WM_RENDERFORMAT, i.e. the user just pasted) asks for the server
        // pull here. We issue the paste request now; the resulting download
        // routes its paths back through render_done when it completes.
        while render_pull_rx.try_recv().is_ok() {
            crate::diag::note("session loop: render pull requested -> initiate_paste");
            awaiting_render = true;
            let dynamic = params.remote_file_format.load(Ordering::Relaxed);
            let format = ironrdp_cliprdr::pdu::ClipboardFormatId::new(if dynamic == 0 {
                15
            } else {
                dynamic
            });
            clip_op!(
                &mut active_stage,
                framed,
                "cliprdr initiate_paste(deferred)",
                |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr.initiate_paste(format)
            );
        }

        // Transfer-thread events -> UI status pump (rdp/clipboard), plus the
        // clipboard publish, which runs HERE so every OS clipboard access
        // stays on the session thread (the old cross-thread write raced the
        // 300ms poll for the global clipboard lock and surfaced as
        // "permission denied" on Windows).
        while let Ok(event) = file_events_rx.try_recv() {
            use crate::clip_transfer::TransferEvent;
            if let TransferEvent::Publish { paths } = &event {
                // The background pull completed: replace the delayed entry
                // with the REAL HDROP. The render cache is primed
                // (mark_background_published) so any later WM_RENDERFORMAT
                // serves these files instead of an empty drop.
                if awaiting_render {
                    awaiting_render = false;
                }
                let joined = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("\n");
                match crate::clip_files::set_local_file_list(paths) {
                    Ok(()) => {
                        suppress_publish_echo = Some(joined);
                        publish_quiet_until = Some(Instant::now() + Duration::from_millis(1000));
                        crate::clip_owner::mark_background_published(paths);
                        crate::diag::note(&format!(
                            "published {} staged file(s) to local clipboard",
                            paths.len()
                        ));
                    }
                    Err(error) => {
                        crate::diag::note(&format!("publish CF_HDROP failed: {error}"));
                    }
                }
            }
            if let Some(emitter) = emitter {
                let (state, detail) = match &event {
                    TransferEvent::Started { files, bytes } => {
                        ("started", serde_json::json!({ "files": files, "bytes": bytes }))
                    }
                    TransferEvent::Progress { bytes } => {
                        ("progress", serde_json::json!({ "bytes": bytes }))
                    }
                    TransferEvent::Finished { files, bytes } => {
                        ("finished", serde_json::json!({ "files": files, "bytes": bytes }))
                    }
                    TransferEvent::Failed { reason } => {
                        ("failed", serde_json::json!({ "reason": reason }))
                    }
                    TransferEvent::Publish { paths } => {
                        ("finished", serde_json::json!({ "files": paths.len(), "bytes": 0 }))
                    }
                };
                let _ = emitter.event(
                    "rdp/clipboard",
                    serde_json::json!({
                        "connectionId": params.connection_id,
                        "direction": "download",
                        "state": state,
                        "detail": detail,
                    }),
                );
            }
        }

        // Local clipboard: event-driven on Windows (WM_CLIPBOARDUPDATE, the
        // same mechanism mstsc uses), timed poll elsewhere. Either way the
        // clipboard is read exactly once per change, files first.
        let clipboard_tick = clip_watch_rx
            .try_recv()
            .map(|event| match event {
                crate::clip_watch::ClipboardEvent::Changed => true,
                crate::clip_watch::ClipboardEvent::Failed(reason) => {
                    crate::diag::note(&format!("clipboard watcher: {reason}; falling back to poll"));
                    watcher_failed = true;
                    true
                }
            })
            .unwrap_or(false);
        // Post-publish quiet window: right after we write CF_HDROP, hold off
        // every clipboard access (watcher event or poll) for a moment.
        // Explorer builds its right-click Paste menu by synchronously
        // querying the clipboard owner; our watcher's OpenClipboard in that
        // window made the menu item read as unavailable — Ctrl+V worked but
        // right-click Paste did not. Yield the clipboard for ~1s after each
        // publish so the user's paste gesture lands on a free clipboard.
        let in_quiet_window = publish_quiet_until.map(|t| Instant::now() < t).unwrap_or(false);
        if in_quiet_window {
            // Drain watcher events without touching the clipboard so they
            // do not pile up behind the window.
            while clip_watch_rx.try_recv().is_ok() {}
        }
        if (clipboard_tick || watcher_failed || last_clip_poll.elapsed() >= Duration::from_millis(300))
            && !in_quiet_window
        {
            read_local_clipboard(
                &mut clipboard,
                &mut last_sent_text,
                &mut last_poll_error,
                &mut last_local_files,
                &mut suppress_publish_echo,
                &params.clip_tx,
                &mut active_stage,
                framed,
            );
            last_clip_poll = Instant::now();
        }

        match framed.read_pdu() {
            Ok((action, payload)) => {
                // RX PDU dump: on slow-path frames, peek the MCS channel id;
                // when it is the CLIPRDR channel, dump the raw user_data
                // (post-MCS, pre-PDU-decode) for byte-level diffing.
                if let (Some(want), ironrdp_pdu::Action::X224) = (cliprdr_channel_id, action) {
                    if let Ok(ctx) = ironrdp_pdu::mcs::decode_send_data_indication(&payload) {
                        if ctx.channel_id == want {
                            crate::clip_dump::dump_svc_chunk(ctx.user_data);
                        }
                    }
                }
                match active_stage.process(&mut image, action, &payload) {
                    Ok(outputs) => {
                        for output in outputs {
                            match output {
                                ActiveStageOutput::GraphicsUpdate(rect) => {
                                    if let Some(pump) = &pump {
                                        seq_counter = seq_counter.wrapping_add(1);
                                        emit_dirty_rect(pump, params, &image, &rect, seq_counter);
                                    }
                                }
                                ActiveStageOutput::ResponseFrame(frame) => {
                                    if let Err(e) = framed.write_all(&frame) {
                                        return SessionEnd::Lost(format!("write failed: {e}"));
                                    }
                                }
                                ActiveStageOutput::Terminate(reason) => {
                                    tracing::info!("RDP session terminated: {reason:?}");
                                    return SessionEnd::Lost(format!("terminated: {reason:?}"));
                                }
                                ActiveStageOutput::DeactivateAll => {
                                    tracing::info!("Deactivation-Reactivation sequence started");
                                    match run_activation_sequence(framed, &activation_factory) {
                                        Ok(desktop) => {
                                            image = ironrdp_session::image::DecodedImage::new(
                                                PixelFormat::RgbA32,
                                                desktop.0,
                                                desktop.1,
                                            );
                                            params.width.store(desktop.0, Ordering::Relaxed);
                                            params.height.store(desktop.1, Ordering::Relaxed);
                                            let had_resize = params.resize_pending.swap(0, Ordering::Relaxed) == 1;
                                            if let Some(emitter) = emitter {
                                                let _ = emitter.event(
                                                    frame::DESKTOP_EVENT,
                                                    serde_json::json!({
                                                        "connectionId": params.connection_id,
                                                        "width": desktop.0,
                                                        "height": desktop.1,
                                                        "resizeAck": had_resize,
                                                    }),
                                                );
                                            }
                                            if let Some(pump) = &pump {
                                                seq_counter += 1;
                                                emit_full_frame(pump, params, &image, seq_counter);
                                            }
                                        }
                                        Err(error) => return SessionEnd::Lost(error),
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(error) => return SessionEnd::Lost(format!("session processing: {error}")),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                if shutdown.try_recv().is_ok() {
                    return SessionEnd::UserRequested;
                }
                return SessionEnd::Lost(format!("read failed: {e}"));
            }
        }
    }
}

/// Read the local clipboard once and advertise what we find: files first
/// (a CF_HDROP clipboard also exposes a text projection that must never be
/// advertised — it would overwrite the file announcement), then text.
/// Called on every clipboard-change event, with the timed poll as fallback.
#[allow(clippy::too_many_arguments)]
fn read_local_clipboard(
    clipboard: &mut Option<arboard::Clipboard>,
    last_sent_text: &mut Option<String>,
    last_poll_error: &mut Option<String>,
    last_local_files: &mut Option<String>,
    suppress_publish_echo: &mut Option<String>,
    clip_tx: &std::sync::mpsc::SyncSender<crate::clipboard::ClipCommand>,
    active_stage: &mut ironrdp_session::ActiveStage,
    framed: &mut UpgradedFramed,
) {
    #[cfg(windows)]
    if crate::clip_owner::offer_active() { return; }
    if clipboard.is_none() {
        *clipboard = arboard::Clipboard::new().ok();
        if clipboard.is_none() {
            crate::diag::note("local poll: arboard handle unavailable");
        }
    }
    // File side FIRST (see doc comment above).
    let holds_files = match crate::clip_files::read_local_file_list() {
        Ok(Some(list)) => {
            let joined = list
                .roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            if suppress_publish_echo.as_deref() == Some(joined.as_str()) {
                // Our own publish; treat as already advertised.
                *last_local_files = Some(joined);
            } else {
                let _ = clip_tx.try_send(crate::clipboard::ClipCommand::AdvertiseLocalFiles(list.roots));
            }
            true
        }
        Ok(None) => {
            // Clipboard moved on: the echo guard is now meaningless.
            *suppress_publish_echo = None;
            false
        }
        Err(_) => false,
    };

    if holds_files {
        return; // never advertise the shell's text projection of file paths
    }

    // Text side: get_text errors are expected while the clipboard holds
    // files/bitmap; log once per distinct error.
    match clipboard.as_mut().map(|handle| handle.get_text()) {
        Some(Ok(text)) => {
            *last_poll_error = None;
            if last_sent_text.as_deref() != Some(text.as_str()) {
                crate::diag::note(&format!("local poll: text changed len={} -> advertise", text.len()));
                *last_sent_text = Some(text);
                clip_op!(
                    active_stage,
                    framed,
                    "cliprdr advertise",
                    |cliprdr: &mut ironrdp_cliprdr::CliprdrClient| cliprdr
                        .initiate_copy(&crate::clipboard::supported_clipboard_formats())
                );
            }
        }
        Some(Err(error)) => {
            // Clipboard holds non-text (files/bitmap): forget what we
            // advertised so re-copying the SAME text afterwards still
            // re-advertises.
            *last_sent_text = None;
            let message = error.to_string();
            if last_poll_error.as_deref() != Some(message.as_str()) {
                *last_poll_error = Some(message.clone());
                crate::diag::note(&format!("local poll: get_text error: {message}"));
            }
        }
        None => {}
    }
}

fn drive_response_frames(
    framed: &mut UpgradedFramed,
    outputs: Vec<ironrdp_session::ActiveStageOutput>,
) -> Result<(), String> {
    use ironrdp_session::ActiveStageOutput;
    for output in outputs {
        if let ActiveStageOutput::ResponseFrame(frame) = output {
            framed.write_all(&frame).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Drive a Deactivation-Reactivation Sequence (MS-RDPBCGR 1.4.3) to completion
/// and return the confirmed desktop size.
fn run_activation_sequence(
    framed: &mut UpgradedFramed,
    factory: &ironrdp_connector::connection_activation::ConnectionActivationFactory,
) -> Result<(u16, u16), String> {
    use ironrdp_connector::connection_activation::ConnectionActivationState;
    use ironrdp_connector::Sequence as _;
    let mut activation = factory.create();
    let mut buf = ironrdp_core::WriteBuf::new();
    loop {
        buf.clear();
        let written = if let Some(hint) = activation.next_pdu_hint() {
            let pdu = framed
                .read_by_hint(hint)
                .map_err(|e| format!("reactivation read: {e}"))?;
            activation
                .step(&pdu, &mut buf)
                .map_err(|e| format!("reactivation step: {e}"))?
        } else {
            activation
                .step_no_input(&mut buf)
                .map_err(|e| format!("reactivation step: {e}"))?
        };
        if let Some(size) = written.size() {
            framed
                .write_all(&buf[..size])
                .map_err(|e| format!("reactivation write: {e}"))?;
        }
        if let ConnectionActivationState::Finalized { desktop_size, .. } = activation.connection_activation_state() {
            return Ok((desktop_size.width, desktop_size.height));
        }
    }
}

fn emit_dirty_rect(
    pump: &FramePump,
    params: &SessionParams,
    image: &ironrdp_session::image::DecodedImage,
    rect: &ironrdp_pdu::geometry::InclusiveRectangle,
    seq: u32,
) {
    let x = rect.left;
    let y = rect.top;
    let width = rect.right - rect.left + 1;
    let height = rect.bottom - rect.top + 1;
    let (dw, dh) = (image.width(), image.height());
    for (tx, ty, tw, th) in frame::tile_rect(dw, dh, (x, y, width, height)) {
        emit_tile(pump, params, image, (tx, ty, tw, th), dw, dh, seq);
    }
}

fn emit_full_frame(
    pump: &FramePump,
    params: &SessionParams,
    image: &ironrdp_session::image::DecodedImage,
    seq: u32,
) {
    let (dw, dh) = (image.width(), image.height());
    for (tx, ty, tw, th) in frame::tile_rect(dw, dh, (0, 0, dw, dh)) {
        emit_tile(pump, params, image, (tx, ty, tw, th), dw, dh, seq);
    }
}

fn emit_tile(
    pump: &FramePump,
    params: &SessionParams,
    image: &ironrdp_session::image::DecodedImage,
    rect: (u16, u16, u16, u16),
    dw: u16,
    dh: u16,
    seq: u32,
) {
    let (x, y, w, h) = rect;
    if w == 0 || h == 0 || dw == 0 || dh == 0 {
        return;
    }
    // Copy the tile out of the framebuffer row by row: `data_for_rect` exposes
    // a strided view, but the frame contract requires a tight w*h*4 payload.
    let stride = image.stride();
    let source = image.data();
    let right = (x + w).min(dw);
    let bottom = (y + h).min(dh);
    let (w, h) = (right - x, bottom - y);
    let columns = w as usize * 4;
    let mut pixels = Vec::with_capacity(columns * h as usize);
    for row in y..bottom {
        let start = row as usize * stride + x as usize * 4;
        let end = start + columns;
        if end > source.len() {
            return;
        }
        pixels.extend_from_slice(&source[start..end]);
    }
    pump.push(frame::encode_frame(
        &params.connection_id,
        dw,
        dh,
        frame::FORMAT_RGBA32,
        (x, y, w, h),
        seq,
        &pixels,
    ));
}

/// Convert a wire input event into fast-path events. Move throttling lives in
/// the UI (and stale moves are coalesced by the session loop), so every event
/// that reaches here is sent.
fn input_to_fastpath(
    event: &InputKind,
) -> Option<Vec<ironrdp_pdu::input::fast_path::FastPathInputEvent>> {
    use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags as FastPathKeyboardFlags};
    use ironrdp_pdu::input::mouse::PointerFlags;
    use ironrdp_pdu::input::MousePdu;

    match event {
        InputKind::Mouse { flags, x, y } => {
            Some(vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::from_bits_retain(*flags),
                number_of_wheel_rotation_units: 0,
                x_position: *x,
                y_position: *y,
            })])
        }
        InputKind::Wheel { is_horizontal, rotation, x, y } => {
            let flags = if *is_horizontal {
                PointerFlags::HORIZONTAL_WHEEL
            } else {
                PointerFlags::VERTICAL_WHEEL
            };
            Some(vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags,
                number_of_wheel_rotation_units: *rotation,
                x_position: *x,
                y_position: *y,
            })])
        }
        InputKind::Key { flags, code } => {
            // The wire uses slow-path flag bits (EXTENDED 0x0100, RELEASE
            // 0x8000); fast path packs them into its own u8 flags, where a
            // key press is simply the absence of RELEASE.
            let mut kb = FastPathKeyboardFlags::empty();
            if flags & 0x8000 != 0 {
                kb |= FastPathKeyboardFlags::RELEASE;
            }
            let mut code = *code;
            if flags & 0x0100 != 0 || code > 0xFF {
                kb |= FastPathKeyboardFlags::EXTENDED;
                code &= 0x00FF;
            }
            Some(vec![FastPathInputEvent::KeyboardEvent(kb, code as u8)])
        }
        InputKind::Unicode { flags, code } => {
            // IME-committed text arrives as UTF-16 code units and is sent as
            // a Unicode keyboard event (MS-RDPBCGR 2.2.8.1.2.4.2).
            let mut kb = FastPathKeyboardFlags::empty();
            if flags & 0x8000 != 0 {
                kb |= FastPathKeyboardFlags::RELEASE;
            }
            Some(vec![FastPathInputEvent::UnicodeKeyboardEvent(kb, *code)])
        }
        InputKind::Resize { .. } | InputKind::ApplySize { .. } | InputKind::PullFiles => None,
    }
}

// ---------------------------------------------------------------- plumbing

fn build_config(
    username: String,
    password: String,
    domain: Option<String>,
    width: u16,
    height: u16,
) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        credentials: ironrdp_connector::Credentials::UsernamePassword { username, password },
        domain,
        desktop_size: ironrdp_connector::DesktopSize { width, height },
        desktop_scale_factor: 0,
        enable_tls: false,
        enable_credssp: true,
        client_build: 0,
        client_name: "dbx-rdp-plugin".to_owned(),
        keyboard_type: ironrdp_pdu::gcc::KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: ironrdp_pdu::rdp::capability_sets::MajorPlatformType::WINDOWS,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        performance_flags: ironrdp_pdu::rdp::client_info::PerformanceFlags::default(),
        license_cache: None,
        timezone_info: ironrdp_pdu::rdp::client_info::TimezoneInfo::default(),
        compression_type: Some(ironrdp_pdu::rdp::client_info::CompressionType::Rdp61),
        // Client-side pointer only: baking the remote cursor into the bitmap
        // would double it up with the browser cursor on the canvas.
        enable_server_pointer: false,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

fn tls_upgrade(
    stream: TcpStream,
    server_name: &str,
) -> Result<(rustls::StreamOwned<rustls::ClientConnection, TcpStream>, Vec<u8>, [u8; 32]), ConnectFailure> {
    use sha2::Digest as _;

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyVerifier))
        .with_no_client_auth();
    // CredSSP does not support TLS session resumption.
    config.resumption = rustls::client::Resumption::disabled();
    let config = Arc::new(config);

    let name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|e| ConnectFailure(format!("Endpoint name is not a valid TLS server name: {e}")))?;
    let client = rustls::ClientConnection::new(config, name)
        .map_err(|e| ConnectFailure(format!("TLS client setup failed: {e}")))?;
    let mut tls = rustls::StreamOwned::new(client, stream);
    // Flush drives the handshake forward so the server flight is delivered.
    let _ = tls.flush();

    let cert_der = tls
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first().map(|cert| cert.as_ref().to_vec()))
        .ok_or_else(|| ConnectFailure("TLS handshake returned no server certificate".to_string()))?;

    let fingerprint: [u8; 32] = sha2::Sha256::digest(&cert_der).into();
    let server_public_key = extract_public_key(&cert_der)?;
    Ok((tls, server_public_key, fingerprint))
}

fn extract_public_key(cert_der: &[u8]) -> Result<Vec<u8>, ConnectFailure> {
    use x509_cert::der::Decode as _;
    let cert = x509_cert::Certificate::from_der(cert_der)
        .map_err(|e| ConnectFailure(format!("Server certificate is not parseable: {e}")))?;
    let bit_string = &cert.tbs_certificate.subject_public_key_info.subject_public_key;
    Ok(bit_string.raw_bytes().to_vec())
}

/// rustls verifier that defers trust to our fingerprint store +
/// host/requestUserInput flow (RDP servers almost always ship self-signed
/// certs; rustls' built-in root store would reject them).
#[derive(Debug)]
struct AcceptAnyVerifier;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            ED25519,
        ]
    }
}

/// CredSSP may consult a KDC over HTTP for Kerberos. This plugin authenticates
/// with local credentials only (NTLM) and must never dial out on the user's
/// behalf, so the client refuses.
struct OfflineKdcNetworkClient;

impl ironrdp_connector::sspi::network_client::NetworkClient for OfflineKdcNetworkClient {
    fn send(
        &self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::sspi::Result<Vec<u8>> {
        Err(ironrdp_connector::sspi::Error::new(
            ironrdp_connector::sspi::ErrorKind::InternalError,
            "Kerberos KDC access is disabled in this plugin; NTLM only",
        ))
    }
}

fn field_str(connection: &Value, key: &str) -> Option<String> {
    connection
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn config_str(connection: &Value, key: &str) -> Option<String> {
    connection
        .get("external_config")
        .and_then(|config| config.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn config_u64(connection: &Value, key: &str) -> Option<u64> {
    connection
        .get("external_config")
        .and_then(|config| config.get(key))
        .and_then(Value::as_u64)
}

fn config_bool(connection: &Value, key: &str) -> bool {
    connection
        .get("external_config")
        .and_then(|config| config.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
