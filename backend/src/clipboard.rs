//! Text-only clipboard redirection (MS-RDPECLIP) over CLIPRDR.
//!
//! Scope: plain CF_UNICODETEXT both ways. Remote copies are fetched
//! automatically and written to the local clipboard; local changes are polled
//! and advertised by the session loop (which owns the arboard handle). File
//! formats are intentionally not implemented — see README known limitations.
//!
//! All CliprdrBackend callbacks fire on the RDP session thread; the backend is
//! a pure command queue and never touches the OS clipboard itself.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;

/// True when this advertised format carries clipboard text. Matched by the
/// standard ids OR by a name containing "TEXT": servers that negotiate
/// USE_LONG_FORMAT_NAMES register text under varied ids and names
/// (CF_UNICODETEXT / CF_TEXT / CF_OEMTEXT), and a plain-text copy on a real
/// Windows server must be recognized in every case.
pub fn format_is_text(format: &ClipboardFormat) -> bool {
    let id = format.id().0;
    // 13 = CF_UNICODETEXT, 1 = CF_TEXT, 7 = CF_OEMTEXT.
    if id == 13 || id == 1 || id == 7 {
        return true;
    }
    format
        .name()
        .is_some_and(|name| name.value().to_uppercase().contains("TEXT"))
}

/// Commands the backend queues for the session loop (which owns the Cliprdr
/// processor and the socket).
#[derive(Debug)]
pub enum ClipCommand {
    /// Channel ready / server asked for our format list.
    AdvertiseText,
    /// Server wants our clipboard payload for the given format id.
    SendText(u32),
    /// Remote clipboard now holds text: request it.
    FetchRemoteText,
    /// Remote text arrived: write it to the local clipboard.
    RemoteText(String),
}

#[derive(Debug)]
pub struct TextClipboard {
    tx: SyncSender<ClipCommand>,
    temp_dir: String,
    /// Format id the server currently advertises for Unicode text. Servers
    /// negotiating long format names register CF_UNICODETEXT under a dynamic
    /// id, which the fetch step must echo back.
    text_format_id: Arc<AtomicU32>,
}

impl TextClipboard {
    pub fn new(tx: SyncSender<ClipCommand>, text_format_id: Arc<AtomicU32>) -> Self {
        Self {
            tx,
            temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            text_format_id,
        }
    }

    fn command(&self, command: ClipCommand) {
        // Bounded queue: drop (rather than block) when the session loop is
        // behind; a later poll/advertise cycle recovers the missed event.
        let _ = self.tx.try_send(command);
    }
}

impl_as_any!(TextClipboard);

impl CliprdrBackend for TextClipboard {
    fn temporary_directory(&self) -> &str {
        crate::diag::note("backend.temporary_directory");
        &self.temp_dir
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Observed live: this server advertises plain ids with empty names.
        // Do not claim long format names and advertise the same way — the
        // server ignored our named announcement entirely (no response, no
        // format data request).
        crate::diag::note("backend.client_capabilities(empty)");
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_format_list_response(&mut self, ok: bool) {
        crate::diag::note(&format!("backend.on_format_list_response ok={ok} (server answer to our announce)"));
    }

    fn on_ready(&mut self) {
        crate::diag::note("backend.on_ready (channel reached Ready)");
        self.command(ClipCommand::AdvertiseText);
    }

    fn on_request_format_list(&mut self) {
        crate::diag::note("backend.on_request_format_list (server sent MonitorReady)");
        self.command(ClipCommand::AdvertiseText);
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        crate::diag::note(&format!("backend.on_process_negotiated_capabilities flags={:?}", capabilities));
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        crate::diag::note(&format!(
            "backend.on_remote_copy formats=[{}]",
            available_formats
                .iter()
                .map(|format| {
                    let name = format.name().map(|value| value.value()).unwrap_or("");
                    format!("#{}:{}", format.id().0, name)
                })
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        let text = available_formats
            .iter()
            .find(|format| format.id().0 == 13)
            .or_else(|| available_formats.iter().find(|format| format_is_text(format)));
        if let Some(text) = text {
            self.text_format_id.store(text.id().0, Ordering::Relaxed);
            crate::diag::note(&format!(
                "backend.on_remote_copy text format #{} found -> FetchRemoteText",
                text.id().0
            ));
            self.command(ClipCommand::FetchRemoteText);
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        crate::diag::note(&format!(
            "backend.on_format_data_request format #{} (remote paste wants our data)",
            request.format.0
        ));
        self.command(ClipCommand::SendText(request.format.0));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            crate::diag::note("backend.on_format_data_response ERROR");
            return;
        }
        match response.to_unicode_string() {
            Ok(text) => {
                crate::diag::note(&format!("backend.on_format_data_response len={}", text.len()));
                self.command(ClipCommand::RemoteText(text));
            }
            Err(error) => crate::diag::note(&format!("backend.on_format_data_response undecodable {error}")),
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {
        crate::diag::note("backend.on_file_contents_request (file format copy, not supported)");
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {
        crate::diag::note("backend.on_lock");
    }

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

/// The one format this plugin advertises: bare CF_UNICODETEXT, no name —
/// matching how the server itself lists formats (observed: plain ids, empty
/// names). The named variant was silently ignored by the server.
pub fn unicode_text_formats() -> [ClipboardFormat; 1] {
    [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
}
