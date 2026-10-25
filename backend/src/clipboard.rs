//! Clipboard redirection (MS-RDPECLIP) over CLIPRDR: text + files.
//!
//! Text: plain CF_UNICODETEXT both ways (unchanged from previous releases).
//! Files: FileGroupDescriptorW + Preferred DropEffect, streamed via
//! FileContentsRequest/Response in <=1 MiB RANGE chunks so >4 GiB files
//! transfer with constant memory (position is u64 on the wire).
//!
//! File clipboard is Windows-only for now (CF_HDROP via clipboard-win);
//! Linux/macOS keep text-only and ignore file formats gracefully.
//!
//! All CliprdrBackend callbacks fire on the RDP session thread; the backend is
//! a pure command queue and never touches the OS clipboard itself.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_core::IntoOwned;

/// True when this advertised format carries clipboard text. Matched by the
/// standard ids OR by a name containing "TEXT": servers negotiating
/// USE_LONG_FORMAT_NAMES register text under varied ids and names
/// (CF_UNICODETEXT / CF_TEXT / CF_OEMTEXT).
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

/// True when this advertised format is a remote file list
/// (FileGroupDescriptorW, arbitrary id under long format names).
pub fn format_is_file_list(format: &ClipboardFormat) -> bool {
    format
        .name()
        .is_some_and(|name| name.value().eq_ignore_ascii_case("FileGroupDescriptorW"))
}

/// True for the drop-companion formats Explorer pairs with file copies:
/// their presence in a FormatList means "this is a file clipboard" even
/// when the FileGroupDescriptorW entry has not been advertised (yet).
pub fn is_file_hint_format(format: &ClipboardFormat) -> bool {
    format.name().is_some_and(|name| {
        let value = name.value();
        value.eq_ignore_ascii_case("FileNameW")
            || value.eq_ignore_ascii_case("FileName")
            || value.eq_ignore_ascii_case("FileContents")
            || value.eq_ignore_ascii_case("Preferred DropEffect")
    })
}

/// Commands the backend queues for the session loop (which owns the Cliprdr
/// processor, the socket, and the OS clipboard handles).
#[derive(Debug)]
pub enum ClipCommand {
    // ---- text (unchanged) ----
    /// Channel ready / server asked for our format list.
    AdvertiseText,
    /// The CLIPRDR channel just reached Ready: re-read the local clipboard
    /// once so files copied before the session finished connecting still
    /// get advertised (and anything queued during connect is not lost).
    ClipboardReady,
    /// Server wants our clipboard payload for the given format id.
    SendText(u32),
    /// Server asked for a format we do not carry (e.g. FileNameW during a
    /// file paste); answer with CB_RESPONSE_FAIL so it can move on.
    RefuseFormat(u32),
    /// Remote clipboard now holds text: request it.
    FetchRemoteText,
    /// Remote text arrived: write it to the local clipboard.
    RemoteText(String),
    /// Remote clipboard now holds a file list: request it (paste) so
    /// on_remote_file_list fires with the descriptors.
    FetchRemoteFiles(u32),
    /// Deferred render: arm a delayed CF_HDROP without pulling anything;
    /// the data is fetched from the server at local paste time.
    OfferRemoteFiles,
    /// The file-list paste came back FAIL; retry it (Windows renders the
    /// data lazily right after a copy — first request often fails).
    RetryPaste { format_id: u32, attempt: u32 },
    /// Server sent a FormatList: acknowledge with FormatListResponse OK
    /// so it proceeds to send FileGroupDescriptorW / file data.
    SendFormatListResponse,

    // ---- files ----
    /// A remote file list arrived: begin downloading it to the staging dir.
    RemoteFileList { descriptors: Vec<ironrdp_cliprdr::pdu::FileDescriptor>, data_id: Option<u32> },
    /// Local clipboard holds files (CF_HDROP): the 300ms poll passes its
    /// snapshot here so the command pump never re-opens the clipboard (the
    /// old double-read raced the OS clipboard and wedged the 2nd copy).
    AdvertiseLocalFiles(Vec<std::path::PathBuf>),
    /// The remote clipboard changed to non-file content; our advertised
    /// file set is gone server-side, so the upload dedupe must reset.
    LocalFilesSuperseded,
    /// Remote paste requested bytes of our advertised file: served by the
    /// transfer thread via request-file-chunk / submit-file-contents.
    /// (No session-loop work; the transfer thread owns the file handle.)
    ServeFileChunk(FileContentsRequest),
    /// A FileContentsResponse chunk of an in-flight download arrived.
    DownloadChunk(FileContentsResponse<'static>),
}

#[derive(Debug)]
pub struct TextClipboard {
    tx: SyncSender<ClipCommand>,
    temp_dir: String,
    /// Format id the server currently advertises for Unicode text. Servers
    /// negotiating long format names register CF_UNICODETEXT under a dynamic
    /// id, which the fetch step must echo back.
    text_format_id: Arc<AtomicU32>,
    /// Format id the server currently advertises for the file list.
    file_format_id: Arc<AtomicU32>,
    /// Consecutive FAIL count for the current file-list paste; cleared on
    /// success so a later copy starts its retry ladder fresh.
    retry_attempt: Arc<AtomicU32>,
    /// Native file clipboard integration exists only on Windows.
    file_clipboard_enabled: bool,
}

impl TextClipboard {
    pub fn new(
        tx: SyncSender<ClipCommand>,
        text_format_id: Arc<AtomicU32>,
        file_format_id: Arc<AtomicU32>,
        retry_attempt: Arc<AtomicU32>,
    ) -> Self {
        Self {
            tx,
            temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            text_format_id,
            file_format_id,
            retry_attempt,
            file_clipboard_enabled: cfg!(windows),
        }
    }

    fn command(&self, command: ClipCommand) {
        // Bounded queue: drop (rather than block) when the session loop is
        // behind; a later poll/advertise cycle recovers the missed event.
        let _ = self.tx.try_send(command);
    }

    /// Whether the local clipboard currently holds a CF_HDROP file set.
    /// Read-only probe used to decide text-vs-refuse; the session loop owns
    /// all clipboard mutations. While our own delayed offer is active the
    /// clipboard belongs to the render pipeline — report "holds files" so
    /// remote text probes get refused instead of deadlocking on a read.
    fn clipboard_holds_files(&self) -> bool {
        if crate::clip_owner::offer_active() {
            return true;
        }
        crate::clip_files::read_local_file_list().map(|list| list.is_some()).unwrap_or(false)
    }
}

impl_as_any!(TextClipboard);

impl CliprdrBackend for TextClipboard {
    fn temporary_directory(&self) -> &str {
        crate::diag::note("backend.temporary_directory");
        &self.temp_dir
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        if !self.file_clipboard_enabled {
            return ClipboardGeneralCapabilityFlags::empty();
        }
        // We implement file streams, not RDPDR drive/path redirection.
        // Require path-free stream descriptors rather than remote CF_HDROP
        // source paths that cannot be opened by the local machine.
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
            | ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {
        crate::diag::note("backend.on_ready (channel reached Ready)");
        self.command(ClipCommand::AdvertiseText);
        self.command(ClipCommand::ClipboardReady);
    }

    fn on_request_format_list(&mut self) {
        crate::diag::note("backend.on_request_format_list (server sent MonitorReady)");
        self.command(ClipCommand::AdvertiseText);
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        crate::diag::note(&format!("backend.on_process_negotiated_capabilities flags={:?}", capabilities));
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.file_format_id.store(0, Ordering::Relaxed);
        self.retry_attempt.store(0, Ordering::Relaxed);
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
        // Only FileGroupDescriptorW encodes CLIPRDR_FILELIST. FileNameW
        // encodes a path, CF_HDROP encodes DROPFILES, and FileContents uses
        // the separate FileContentsRequest/Response exchange. None is a
        // substitute for a descriptor list. IronRDP acknowledges and locks
        // genuine stream offers automatically before we request metadata.
        let file_format = available_formats.iter().find(|f| format_is_file_list(f));
        if let Some(file) = file_format.filter(|_| self.file_clipboard_enabled) {
            self.file_format_id.store(file.id().0, Ordering::Relaxed);
            crate::diag::note(&format!(
                "backend.on_remote_copy file clipboard #{} -> arm deferred offer",
                file.id().0
            ));
            self.command(ClipCommand::OfferRemoteFiles);
            return;
        }
        if self.file_clipboard_enabled && available_formats.iter().any(|f| f.id() == ClipboardFormatId::CF_HDROP || is_file_hint_format(f)) {
            crate::diag::note("remote file paths without FileGroupDescriptorW; not requesting incompatible hint formats");
            return;
        }
        // A non-file remote copy supersedes our advertised file set on the
        // wire (our next FormatList would too, but we will not send one
        // unless the local clipboard changes). Clear the upload dedupe so
        // re-copying the SAME local files still re-advertises — without
        // this, "copy file A, copy text, copy file A again" wedged.
        self.command(ClipCommand::LocalFilesSuperseded);
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
        // Formats that reach the backend are exactly those the Cliprdr
        // processor did NOT answer inline: our file-list (0xC0FE) and
        // drop-effect (0xC0FD) requests are served by the processor itself,
        // so anything here is a non-text format Explorer probes for during a
        // file paste (FileNameW, CF_LOCALE, …) or a plain text request.
        // Answering a non-text id with text bytes corrupts the remote paste
        // (that is why non-txt files "did not copy"); refuse them cleanly.
        //
        // The remote assigns CF_UNICODETEXT a fresh dynamic id per clipboard
        // generation (observed #49405) which never appears in any
        // FormatList, so a recognized-text check by id alone under-refuses.
        // Tiebreak: with no file set on the clipboard, an unknown format is
        // served as text (worst case the remote discards it); with files
        // present, unknown formats fail fast so Explorer skips them.
        let known_text = request.format.0 == 13
            || request.format.0 == 1
            || request.format.0 == 7
            || request.format.0 == self.text_format_id.load(Ordering::Relaxed);
        let holds_files = self.clipboard_holds_files();
        let is_text = known_text || (!holds_files && request.format.0 != 16);
        crate::diag::note(&format!(
            "backend.on_format_data_request format #{} text={} files={} ",
            request.format.0, is_text, holds_files
        ));
        if request.format.0 == 16 {
            // CF_LOCALE must always be answered or strict servers abort
            // the whole paste.
            self.command(ClipCommand::SendText(16));
        } else if is_text {
            self.command(ClipCommand::SendText(request.format.0));
        } else {
            self.command(ClipCommand::RefuseFormat(request.format.0));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            crate::diag::note("backend.on_format_data_response ERROR");
            // A failed response for a file-list format we just requested is
            // almost always Windows lazy rendering (data not ready yet);
            // schedule a retry instead of giving up. The pending format id
            // is whichever file format we last initiated a paste for.
            let file_format = self.file_format_id.load(Ordering::Relaxed);
            if file_format != 0 {
                let next = self.retry_attempt.fetch_add(1, Ordering::Relaxed) + 1;
                crate::diag::note(&format!(
                    "backend.on_format_data_response: file paste FAIL (attempt {next}) -> retry"
                ));
                self.command(ClipCommand::RetryPaste { format_id: file_format, attempt: next });
            }
            return;
        }
        // Successful descriptor responses are handled by IronRDP and routed
        // to on_remote_file_list. A response reaching us while a descriptor
        // request is pending is malformed, not clipboard text.
        let file_format = self.file_format_id.load(Ordering::Relaxed);
        if file_format != 0 {
            crate::diag::note("invalid file descriptor response; refusing to publish binary data as text");
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

    fn on_remote_file_list(&mut self, files: &[ironrdp_cliprdr::pdu::FileDescriptor], clip_data_id: Option<u32>) {
        if !self.file_clipboard_enabled {
            return;
        }
        crate::diag::note(&format!(
            "backend.on_remote_file_list count={} -> download",
            files.len()
        ));
        // The paste sequence finally succeeded: reset the retry ladder.
        self.retry_attempt.store(0, Ordering::Relaxed);
        self.command(ClipCommand::RemoteFileList { descriptors: files.to_vec(), data_id: clip_data_id });
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        crate::diag::note(&format!(
            "backend.on_file_contents_request stream={} index={} flags={:?} pos={} len={}",
            request.stream_id, request.index, request.flags, request.position, request.requested_size
        ));
        self.command(ClipCommand::ServeFileChunk(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        crate::diag::note(&format!(
            "backend.on_file_contents_response stream={} error={} len={}",
            response.stream_id(),
            response.is_error(),
            response.data().len()
        ));
        self.command(ClipCommand::DownloadChunk(response.into_owned()));
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        crate::diag::note(&format!("backend.on_lock clipDataId={}", data_id.0));
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        crate::diag::note(&format!("backend.on_unlock clipDataId={}", data_id.0));
    }
}

/// The formats this plugin advertises for plain text: bare CF_UNICODETEXT,
/// no name — matching how the server itself lists formats (observed: plain
/// ids, empty names). The named variant was silently ignored by the server.
pub fn unicode_text_formats() -> [ClipboardFormat; 1] {
    [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
}

/// Initial format names. Keep the IDs for descriptors and drop effect aligned
/// with IronRDP::initiate_file_copy; FileContents needs its own distinct ID.
#[cfg(windows)]
pub fn supported_clipboard_formats() -> [ClipboardFormat; 4] {
    [
        ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ClipboardFormat::new(ClipboardFormatId::new(0xC0FE))
            .with_name(ClipboardFormatName::new_static("FileGroupDescriptorW")),
        ClipboardFormat::new(ClipboardFormatId::new(0xC0FD))
            .with_name(ClipboardFormatName::new_static("Preferred DropEffect")),
        ClipboardFormat::new(ClipboardFormatId::new(0xC0FC))
            .with_name(ClipboardFormatName::new_static("FileContents")),
    ]
}

#[cfg(not(windows))]
pub fn supported_clipboard_formats() -> [ClipboardFormat; 1] {
    unicode_text_formats()
}

#[cfg(test)]
mod platform_tests {
    use super::*;
    use std::sync::mpsc;

    // Exercise Linux/macOS policy on every runner without opening the OS
    // clipboard. Enabling stream capabilities or preferring file offers on
    // a text-only platform must fail these tests.
    #[test]
    fn text_only_client_does_not_negotiate_file_streams() {
        let (tx, _) = mpsc::sync_channel(32);
        let mut backend = TextClipboard::new(tx, Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        backend.file_clipboard_enabled = false;
        assert!(!backend.client_capabilities().contains(
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED));
    }

    #[test]
    fn text_only_client_selects_text_from_mixed_remote_offer() {
        let (tx, rx) = mpsc::sync_channel(32);
        let file_id = Arc::new(AtomicU32::new(0));
        let mut backend = TextClipboard::new(tx, Arc::new(AtomicU32::new(0)),
            file_id.clone(), Arc::new(AtomicU32::new(0)));
        backend.file_clipboard_enabled = false;
        backend.on_remote_copy(&[
            ClipboardFormat::new(ClipboardFormatId::new(49352))
                .with_name(ClipboardFormatName::new_static("FileGroupDescriptorW")),
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ]);
        let commands: Vec<_> = rx.try_iter().collect();
        assert_eq!(file_id.load(Ordering::Relaxed), 0);
        assert!(commands.iter().any(|c| matches!(c, ClipCommand::FetchRemoteText)));
        assert!(!commands.iter().any(|c| matches!(c, ClipCommand::OfferRemoteFiles)));
    }

    #[test]
    fn text_only_client_ignores_unsolicited_file_list() {
        let (tx, rx) = mpsc::sync_channel(32);
        let mut backend = TextClipboard::new(tx, Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        backend.file_clipboard_enabled = false;
        backend.on_remote_file_list(&[ironrdp_cliprdr::pdu::FileDescriptor::new("file")], None);
        assert!(rx.try_recv().is_err());
    }
}
