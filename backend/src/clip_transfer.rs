//! File transfer engine for clipboard redirection.
//!
//! Two directions, both streamed in <=1 MiB chunks:
//!
//! - Outbound (local copy -> remote paste): the session loop keeps a table of
//!   `OutboundFile`s with open handles; each `FileContentsRequest` from the
//!   server is answered inline from disk at the u64 position. >4 GiB files
//!   work because we never hold more than one chunk in memory.
//! - Inbound (remote copy -> local paste): a dedicated thread drives the
//!   chunk ladder per file: SIZE (or descriptor size) then repeated RANGE
//!   requests written to a staging file, finishing with a CF_HDROP publish.
//!
//! The engine talks to the RDP channel exclusively through the same
//! `clip_op!` macro the session loop uses, so reconnects and channel errors
//! behave identically to the text path.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::pdu::{FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor};

use crate::clip_files::{self, OutboundFile, MAX_CHUNK};

/// Events the transfer machinery pushes to the session loop, which relays
/// them to the UI as `rdp/clipboard` status.
#[derive(Debug, Clone)]
pub enum TransferEvent {
    Started { files: usize, bytes: u64 },
    Progress { bytes: u64 },
    Finished { files: usize, bytes: u64 },
    Failed { reason: String },
    /// The batch finished; these (and only these) files are ready for the
    /// local clipboard. The session loop publishes them, because ALL OS
    /// clipboard access must happen on one thread — the 300ms poll and this
    /// publish otherwise race the global clipboard lock and surface as
    /// "permission denied" on Windows.
    Publish { paths: Vec<PathBuf> },
}

/// Commands the session loop feeds into the inbound transfer thread.
#[derive(Debug)]
pub enum TransferCommand {
    /// Begin downloading: each entry carries the (stream_id, file index in
    /// the remote descriptor list) pair the session loop allocated, so the
    /// requests the thread emits can carry the correct lindex without the
    /// thread ever seeing the remote list.
    Download(Vec<(u32, i32, FileDescriptor)>),
    /// Chunk data (or error) for an in-flight download stream.
    Chunk { stream_id: u32, response: FileContentsResponse<'static> },
    /// Session ended: drop staging state, stop.
    Shutdown,
}

/// Shared handle the session loop uses to talk to the transfer thread.
pub type TransferHandle = SyncSender<TransferCommand>;

/// A read crosses the COM/session boundary with its own bounded rendezvous.
/// Wire stream IDs never repeat within a session, so delayed replies cannot
/// satisfy a new generation (or a different seek on the same file).
pub struct VirtualReadRequest {
    pub generation: u64,
    pub file_index: i32,
    pub position: u64,
    pub length: u32,
    answer: SyncSender<Result<Vec<u8>, String>>,
}

#[derive(Clone)]
pub struct VirtualReader {
    sender: SyncSender<VirtualReadRequest>,
    active: Arc<std::sync::atomic::AtomicU64>,
    timeout: std::time::Duration,
}

impl VirtualReader {
    pub fn read(&self, generation: u64, index: usize, position: u64, length: u32) -> Result<Vec<u8>, String> {
        if generation == 0 || self.active.load(Ordering::Acquire) != generation { return Err("clipboard generation expired".into()); }
        let index = i32::try_from(index).map_err(|_| "file index overflow")?;
        let (answer, receiver) = std::sync::mpsc::sync_channel(1);
        self.sender.try_send(VirtualReadRequest { generation, file_index: index, position, length: length.min(MAX_CHUNK), answer })
            .map_err(|_| "virtual clipboard request queue unavailable")?;
        let started = std::time::Instant::now();
        loop {
            if self.active.load(Ordering::Acquire) != generation { return Err("clipboard generation expired".into()); }
            let remaining = self.timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() { return Err("virtual clipboard read timed out".into()); }
            match receiver.recv_timeout(remaining.min(std::time::Duration::from_millis(50))) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err("virtual clipboard session ended".into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            }
        }
    }
}

struct VirtualPending { request: VirtualReadRequest, started: std::time::Instant }

pub struct VirtualTransferBridge {
    receiver: std::sync::mpsc::Receiver<VirtualReadRequest>,
    active: Arc<std::sync::atomic::AtomicU64>,
    data_id: Option<u32>,
    next_id: u32,
    pending: HashMap<u32, VirtualPending>,
    timeout: std::time::Duration,
}

impl VirtualTransferBridge {
    pub fn new(timeout: std::time::Duration) -> (VirtualReader, Self) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(32);
        let active = Arc::new(std::sync::atomic::AtomicU64::new(0));
        (VirtualReader { sender, active: active.clone(), timeout }, Self { receiver, active, data_id: None, next_id: 0x80000000, pending: HashMap::new(), timeout })
    }
    pub fn begin_generation(&mut self, generation: u64, data_id: Option<u32>) {
        self.active.store(generation, Ordering::Release);
        self.data_id = data_id;
        self.pending.clear();
        while self.receiver.try_recv().is_ok() {}
    }
    pub fn next_request(&mut self) -> Option<FileContentsRequest> {
        self.pending.retain(|_, p| p.started.elapsed() < self.timeout);
        while let Ok(request) = self.receiver.try_recv() {
            if request.generation != self.active.load(Ordering::Acquire) { continue; }
            let id = self.next_id;
            // Exhaustion fails closed rather than recycling an ID with a late reply.
            self.next_id = self.next_id.checked_add(1)?;
            let wire = FileContentsRequest { stream_id: id, index: request.file_index, flags: FileContentsFlags::RANGE,
                position: request.position, requested_size: request.length, data_id: self.data_id };
            self.pending.insert(id, VirtualPending { request, started: std::time::Instant::now() });
            return Some(wire);
        }
        None
    }
    pub fn respond(&mut self, response: FileContentsResponse<'_>) -> bool {
        let Some(pending) = self.pending.remove(&response.stream_id()) else { return false; };
        let request = pending.request;
        let result = if response.is_error() { Err("remote file read failed".into()) }
            else if response.data().is_empty() || response.data().len() > request.length as usize { Err("remote file returned invalid length".into()) }
            else { Ok(response.data().to_vec()) };
        let _ = request.answer.try_send(result);
        true
    }
}

impl Drop for VirtualTransferBridge {
    fn drop(&mut self) { self.active.store(0, Ordering::Release); }
}

/// What the session loop needs per-inbound-download: where responses go.
pub struct DownloadState {
    pub next_stream_id: u32,
}

/// Outbound side: files we advertised, keyed by their index in the
/// FileGroupDescriptorW list. One open handle per file, reused across all
/// RANGE requests of a paste, so a >4 GiB paste costs one file descriptor,
/// not one per chunk.
pub struct OutboundTable {
    files: Vec<OutboundFile>,
}

impl OutboundTable {
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Replace the advertised set. Called after a successful
    /// `initiate_file_copy`; handles from the previous copy are dropped when
    /// the vectors swap.
    pub fn replace(&mut self, files: Vec<OutboundFile>) {
        self.files = files;
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Serve one FileContentsRequest: SIZE returns the descriptor's u64
    /// size, RANGE reads at the u64 position. Errors map to CB_RESPONSE_FAIL.
    pub fn serve(
        &mut self,
        request: &FileContentsRequest,
    ) -> Result<Option<FileContentsResponse<'static>>, String> {
        let index = usize::try_from(request.index).map_err(|_| "negative file index".to_string())?;
        if index >= self.files.len() {
            return Err(format!("file index {index} out of bounds ({} files)", self.files.len()));
        }
        if request.flags.contains(FileContentsFlags::SIZE) {
            let size = self.files[index].descriptor.file_size.unwrap_or(0);
            return Ok(Some(FileContentsResponse::new_size_response(request.stream_id, size)));
        }
        if request.flags.contains(FileContentsFlags::RANGE) {
            let file = &mut self.files[index];
            let len = request.requested_size.min(MAX_CHUNK);
            let data = clip_files::read_chunk(&mut file.handle, request.position, len)
                .map_err(|e| format!("read at {}: {e}", request.position))?;
            return Ok(Some(FileContentsResponse::new_data_response(request.stream_id, data)));
        }
        Err("neither SIZE nor RANGE set".to_string())
    }
}

impl Default for OutboundTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Inbound side: one entry per in-flight download stream.
struct InboundFile {
    path: PathBuf,
    handle: Option<std::fs::File>,
    /// Total bytes of this file (from descriptor or a SIZE probe).
    size: u64,
    /// lindex of this file in the remote descriptor list, echoed in every
    /// request so the server serves the right stream.
    index: i32,
    received: u64,
}

/// Spawns the inbound download thread. Returns its command channel.
///
/// The thread owns all staging files and the clipboard publish step; the
/// session loop stays non-blocking. `clip` is a closure the thread uses to
/// run channel operations on the session thread (see `run_active_session`'s
/// wiring) — keeping it as a trait object would fight the macro-based
/// `clip_op!` borrow pattern, so the closure receives the request PDU and
/// returns the encoded SVC frame to write.
pub fn spawn_download_thread(
    staging_root: PathBuf,
    events: SyncSender<TransferEvent>,
    to_session: SyncSender<FileContentsRequest>,
) -> TransferHandle {
    let (tx, rx) = std::sync::mpsc::sync_channel::<TransferCommand>(64);
    let inbound: Arc<Mutex<HashMap<u32, InboundFile>>> = Arc::new(Mutex::new(HashMap::new()));
    let inbound_for_thread = Arc::clone(&inbound);
    let events_for_thread = events.clone();
    let to_session_for_thread = to_session.clone();

    std::thread::Builder::new()
        .name("dbx-rdp-clip-files".to_string())
        .spawn(move || {
            let inbound = inbound_for_thread;
            let events = events_for_thread;
            let to_session = to_session_for_thread;
            let mut shutdown = false;
            // Per-batch accounting: how many streams of the current download
            // still need a terminal state (data or failure). The clipboard is
            // published exactly once, when this hits zero.
            let mut batch_pending: usize = 0;
            // Successfully staged paths of the current batch. Only these are
            // published — never the whole staging root, which also holds
            // previous batches (publishing stale files was the "old files
            // pasted again" bug).
            let mut batch_staged: Vec<PathBuf> = Vec::new();
            let mut batch_failed = false;
            let mut requests = VecDeque::new();
            let mut outstanding = None;

            while !shutdown {
                // One outstanding request bounds memory and avoids silently
                // dropping files when the session's bounded queue fills.
                if outstanding.is_none() {
                    if let Some(request) = requests.pop_front() {
                        let request: FileContentsRequest = request;
                        let id = request.stream_id;
                        match to_session.try_send(request) {
                            Ok(()) => outstanding = Some(id),
                            Err(std::sync::mpsc::TrySendError::Full(request)) => requests.push_front(request),
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                        }
                    }
                }
                match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Ok(TransferCommand::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        shutdown = true;
                    }
                    Ok(TransferCommand::Download(entries)) => {
                        let total: u64 = entries
                            .iter()
                            .map(|(_, _, d)| d.file_size.unwrap_or(0))
                            .sum();
                        let _ = events.try_send(TransferEvent::Started { files: entries.len(), bytes: total });
                        // A new remote copy supersedes any half-finished one.
                        inbound.lock().map(|mut m| m.clear()).ok();
                        batch_pending = 0;
                        batch_staged.clear();
                        batch_failed = false;
                        requests.clear();
                        outstanding = None;
                        // Sweep previous batches' staging output so the
                        // directory never accumulates stale copies (and so
                        // a previously published-but-since-moved file can
                        // never shadow a fresh download of the same name).
                        let _ = std::fs::remove_dir_all(&staging_root);
                        let _ = std::fs::create_dir_all(&staging_root);
                        for (stream_id, index, descriptor) in entries {
                            let path = clip_files::staging_path_for(&staging_root, &descriptor);
                            // Parent dirs may be nested; create them all.
                            if let Some(parent) = path.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            // Publish the copied top-level items, not every
                            // leaf file: Explorer must retain the folder tree.
                            if let Ok(relative) = path.strip_prefix(&staging_root) {
                                if let Some(component) = relative.components().next() {
                                    let root = staging_root.join(component);
                                    if !batch_staged.contains(&root) {
                                        batch_staged.push(root);
                                    }
                                }
                            }
                            // Directory descriptors carry no stream: the
                            // structure exists once the dirs are created, and
                            // a RANGE request against a directory would fail
                            // the whole server-side paste stream.
                            let is_dir = descriptor
                                .attributes
                                .is_some_and(|a| a.contains(ironrdp_cliprdr::pdu::ClipboardFileAttributes::DIRECTORY));
                            if is_dir {
                                if let Err(error) = std::fs::create_dir_all(&path) {
                                    batch_failed = true;
                                    let _ = events.try_send(TransferEvent::Failed {
                                        reason: format!("cannot create staging directory: {error}"),
                                    });
                                }
                                continue;
                            }
                            let size = descriptor.file_size.unwrap_or(0);
                            let file = std::fs::OpenOptions::new()
                                .create(true)
                                .write(true)
                                .read(true)
                                .truncate(true)
                                .open(&path)
                                .ok();
                            if file.is_none() {
                                batch_failed = true;
                                let _ = events.try_send(TransferEvent::Failed {
                                    reason: format!("cannot create staging file {}", path.display()),
                                });
                                continue;
                            }
                            batch_pending += 1;
                            inbound.lock().map(|mut m| {
                                m.insert(
                                    stream_id,
                                    InboundFile {
                                        path: path.clone(),
                                        handle: file,
                                        size,
                                        index,
                                        received: 0,
                                    },
                                )
                            }).ok();
                            // Kick the ladder: a known size goes straight to
                            // the first RANGE; unknown sizes probe with SIZE
                            // first (some servers omit file_size).
                            let request = if size > 0 {
                                FileContentsRequest {
                                    stream_id,
                                    index,
                                    flags: FileContentsFlags::RANGE,
                                    position: 0,
                                    requested_size: size.min(u64::from(MAX_CHUNK)) as u32,
                                    data_id: None,
                                }
                            } else {
                                FileContentsRequest {
                                    stream_id,
                                    index,
                                    flags: FileContentsFlags::SIZE,
                                    position: 0,
                                    requested_size: 8,
                                    data_id: None,
                                }
                            };
                            requests.push_back(request);
                        }
                        // A directory-only copy has no file streams whose
                        // completion could otherwise trigger publication.
                        if batch_pending == 0 && !batch_failed && !batch_staged.is_empty() {
                            let paths = std::mem::take(&mut batch_staged);
                            let _ = events.try_send(TransferEvent::Publish { paths });
                        }
                    }
                    Ok(TransferCommand::Chunk { stream_id: id, response }) => {
                        // First lock scope: mutate, decide follow-up, release.
                        enum Next {
                            Fail(String),
                            NeedRange { index: i32, position: u64, length: u32 },
                            Completed { path: PathBuf, bytes: u64 },
                        }
                        let next = {
                            let mut guard = match inbound.lock() {
                                Ok(g) => g,
                                Err(_) => continue,
                            };
                            let Some(entry) = guard.get_mut(&id) else { continue };
                            if outstanding == Some(id) {
                                outstanding = None;
                            }
                            if response.is_error() {
                                Next::Fail(format!("stream {id} failed at byte {}", entry.received))
                            } else if entry.handle.is_none() {
                                Next::Fail(format!("stream {id}: no staging handle"))
                            } else {
                                let data = response.data();
                                // A SIZE probe: adopt the reported length.
                                if entry.size == 0 && data.len() == 8 && entry.received == 0 {
                                    if let Ok(size) = response.data_as_size() {
                                        entry.size = size;
                                    }
                                    if entry.size == 0 {
                                        // Zero-length file: nothing to fetch.
                                        Next::Completed { path: entry.path.clone(), bytes: 0 }
                                    } else {
                                        Next::NeedRange {
                                            index: entry.index,
                                            position: 0,
                                            length: entry.size.min(u64::from(MAX_CHUNK)) as u32,
                                        }
                                    }
                                } else if data.is_empty() {
                                    Next::Fail(format!("stream {id}: unexpected EOF at {} of {} bytes", entry.received, entry.size))
                                } else if data.len() as u64 > entry.size.saturating_sub(entry.received) {
                                    Next::Fail(format!("stream {id}: response exceeds advertised file size"))
                                } else {
                                    let position = entry.received;
                                    let handle = entry.handle.as_mut().expect("checked above");
                                    match clip_files::write_chunk(handle, position, data) {
                                        Err(e) => Next::Fail(format!("write at {position}: {e}")),
                                        Ok(()) => {
                                            entry.received += data.len() as u64;
                                            let received = entry.received;
                                            let size = entry.size;
                                            if received < size {
                                                let length = MAX_CHUNK.min(
                                                    u32::try_from(size - received).unwrap_or(MAX_CHUNK),
                                                );
                                                Next::NeedRange { index: entry.index, position: received, length }
                                            } else {
                                                Next::Completed { path: entry.path.clone(), bytes: size }
                                            }
                                        }
                                    }
                                }
                            }
                        };
                        match next {
                            Next::Fail(reason) => {
                                inbound.lock().ok().and_then(|mut m| m.remove(&id));
                                // Never offer an incomplete folder as a
                                // successful copy, even if other files finish.
                                batch_failed = true;
                                batch_pending = batch_pending.saturating_sub(1);
                                let _ = events.try_send(TransferEvent::Failed { reason });
                            }
                            Next::NeedRange { index, position, length } => {
                                let _ = events.try_send(TransferEvent::Progress { bytes: position });
                                let request = FileContentsRequest {
                                    stream_id: id,
                                    index,
                                    flags: FileContentsFlags::RANGE,
                                    position,
                                    requested_size: length,
                                    data_id: None,
                                };
                                requests.push_front(request);
                            }
                            Next::Completed { path, bytes } => {
                                inbound.lock().ok().and_then(|mut m| m.remove(&id));
                                // The next request is not issued until the
                                // session drains events, so this reliable send
                                // cannot race a burst of incoming file chunks.
                                if events.send(TransferEvent::Finished { files: 1, bytes }).is_err() {
                                    break;
                                }
                                batch_pending = batch_pending.saturating_sub(1);
                                if batch_pending == 0 && !batch_failed {
                                    let paths = std::mem::take(&mut batch_staged);
                                    if !paths.is_empty() {
                                        if events.send(TransferEvent::Publish { paths }).is_err() {
                                            break;
                                        }
                                    }
                                }
                                let _ = path;
                            }
                        }
                    }
                }
            }
            let _ = inbound.lock().map(|mut m| m.clear());
        })
        .expect("spawn clip file thread");

    tx
}

/// Session-loop helper: patch a request's file index before sending. The
/// download thread does not know the index (it thinks in stream ids), so the
/// session loop, which owns the remote file list, fills it in from a
/// stream-id -> descriptor-index map it maintains.
pub struct StreamIndexMap {
    map: HashMap<u32, i32>,
    next: AtomicU32,
}

impl StreamIndexMap {
    pub fn new() -> Self {
        Self { map: HashMap::new(), next: AtomicU32::new(1) }
    }

    pub fn alloc(&mut self) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.map.insert(id, 0);
        id
    }

    pub fn assign(&mut self, stream_id: u32, index: i32) {
        self.map.insert(stream_id, index);
    }

    pub fn index_of(&self, stream_id: u32) -> Option<i32> {
        self.map.get(&stream_id).copied()
    }
}

impl Default for StreamIndexMap {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_cliprdr::pdu::ClipboardFormatId;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dbx-rdp-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn outbound_serve_size_then_range() {
        let dir = temp_dir("outbound");
        let path = dir.join("hello.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let mut table = OutboundTable::new();
        table.replace(clip_files::build_descriptors(&[path]).unwrap());
        assert_eq!(table.len(), 1);

        let size_req = FileContentsRequest {
            stream_id: 7,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };
        let resp = table.serve(&size_req).unwrap().unwrap();
        assert_eq!(resp.data_as_size().unwrap(), 10);

        let range_req = FileContentsRequest {
            stream_id: 8,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 4,
            requested_size: 4,
            data_id: None,
        };
        let resp = table.serve(&range_req).unwrap().unwrap();
        assert_eq!(resp.data(), b"4567");
        let _ = ClipboardFormatId::CF_HDROP; // import guard
    }

    #[test]
    fn outbound_rejects_bad_index_and_flags() {
        let mut table = OutboundTable::new();
        let req = FileContentsRequest {
            stream_id: 1,
            index: 5,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };
        assert!(table.serve(&req).is_err());

        let dir = temp_dir("outflag");
        let path = dir.join("x");
        std::fs::write(&path, b"xy").unwrap();
        table.replace(clip_files::build_descriptors(&[path]).unwrap());
        let bad = FileContentsRequest {
            stream_id: 2,
            index: 0,
            flags: FileContentsFlags::empty(),
            position: 0,
            requested_size: 4,
            data_id: None,
        };
        assert!(table.serve(&bad).is_err());
    }

    #[test]
    fn outbound_huge_position_does_not_overflow() {
        let dir = temp_dir("huge");
        let path = dir.join("big.bin");
        // 4 GiB + 1 byte of content would be too slow to materialize; the
        // position math is what we test: a RANGE at u64::MAX-4 must not wrap
        // and must return an empty/short read, not garbage.
        std::fs::write(&path, b"tiny").unwrap();
        let mut table = OutboundTable::new();
        table.replace(clip_files::build_descriptors(&[path]).unwrap());
        let req = FileContentsRequest {
            stream_id: 3,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: u64::MAX - 4,
            requested_size: 8,
            data_id: None,
        };
        // Past EOF: serve returns Ok with empty data (seek beyond end reads
        // nothing), never a wrapped position.
        let resp = table.serve(&req).unwrap().unwrap();
        assert!(resp.data().is_empty());
    }

    #[test]
    fn descriptors_expand_nested_directories() {
        let dir = temp_dir("nested");
        let nested = dir.join("root").join("sub").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("root").join("top.txt"), b"t").unwrap();
        let files = clip_files::build_descriptors(&[dir.join("root")]).unwrap();
        assert_eq!(files.len(), 2);
        // Windows Explorer semantics: copying `root` yields root\top.txt and
        // root\sub\inner\a.txt on the remote.
        let names: Vec<(String, Option<String>)> = files
            .iter()
            .map(|f| (f.descriptor.name.clone(), f.descriptor.relative_path.clone()))
            .collect();
        assert!(names.contains(&("a.txt".into(), Some("root\\sub\\inner".into()))));
        assert!(names.contains(&("top.txt".into(), Some("root".into()))));
    }

    #[test]
    fn multiple_roots_keep_their_own_names() {
        let dir = temp_dir("multi");
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("b").join("c.txt"), b"c").unwrap();
        let files = clip_files::build_descriptors(&[dir.join("a.txt"), dir.join("b")]).unwrap();
        let names: Vec<(String, Option<String>)> = files
            .iter()
            .map(|f| (f.descriptor.name.clone(), f.descriptor.relative_path.clone()))
            .collect();
        assert!(names.contains(&("a.txt".into(), None)));
        assert!(names.contains(&("c.txt".into(), Some("b".into()))));
    }

    #[test]
    fn stream_index_map_roundtrip() {
        let mut map = StreamIndexMap::new();
        map.assign(11, 3);
        assert_eq!(map.index_of(11), Some(3));
        assert_eq!(map.index_of(99), None);
    }
}
