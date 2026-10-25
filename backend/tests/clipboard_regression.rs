//! Reproductions of the protocol and staging failures in remote folder copies.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbx_plugin_rdp::clipboard::{ClipCommand, TextClipboard};
use dbx_plugin_rdp::clip_transfer::{spawn_download_thread, TransferCommand, TransferEvent};
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    ClipboardGeneralCapabilityFlags, FileContentsFlags, FileContentsResponse, FileDescriptor,
};

#[test]
fn virtual_reads_use_positions_and_reject_stale_responses() {
    use dbx_plugin_rdp::clip_transfer::VirtualTransferBridge;
    let (reader, mut bridge) = VirtualTransferBridge::new(Duration::from_secs(1));
    bridge.begin_generation(1, Some(42));
    let read = reader.clone();
    let worker = std::thread::spawn(move || read.read(1, 2, 1024 * 1024, 1024 * 1024));
    let request = loop { if let Some(r) = bridge.next_request() { break r; } std::thread::yield_now(); };
    assert_eq!((request.index, request.position, request.requested_size, request.data_id), (2, 1024 * 1024, 1024 * 1024, Some(42)));
    bridge.respond(FileContentsResponse::new_data_response(request.stream_id, b"short".to_vec()));
    assert_eq!(worker.join().unwrap().unwrap(), b"short");

    let read = reader.clone();
    let old = std::thread::spawn(move || read.read(1, 2, 0, 3));
    let stale = loop { if let Some(r) = bridge.next_request() { break r; } std::thread::yield_now(); };
    bridge.begin_generation(2, None);
    assert!(old.join().unwrap().is_err());
    let read = reader.clone();
    let new = std::thread::spawn(move || read.read(2, 0, 0, 3));
    let current = loop { if let Some(r) = bridge.next_request() { break r; } std::thread::yield_now(); };
    assert_ne!(stale.stream_id, current.stream_id);
    assert!(!bridge.respond(FileContentsResponse::new_data_response(stale.stream_id, b"old".to_vec())));
    assert!(bridge.respond(FileContentsResponse::new_data_response(current.stream_id, b"new".to_vec())));
    assert_eq!(new.join().unwrap().unwrap(), b"new");
    assert!(reader.read(1, 0, 0, 3).is_err());
}

#[test]
fn virtual_reads_surface_remote_errors_and_shutdown_without_fake_eof() {
    use dbx_plugin_rdp::clip_transfer::VirtualTransferBridge;
    let (reader, mut bridge) = VirtualTransferBridge::new(Duration::from_secs(1));
    bridge.begin_generation(1, None);
    for error in [true, false] {
        let read = reader.clone();
        let worker = std::thread::spawn(move || read.read(1, 0, 0, 4));
        let request = loop { if let Some(r) = bridge.next_request() { break r; } std::thread::yield_now(); };
        let response = if error { FileContentsResponse::new_error(request.stream_id) } else { FileContentsResponse::new_data_response(request.stream_id, vec![]) };
        bridge.respond(response);
        assert!(worker.join().unwrap().is_err());
    }
    let read = reader.clone();
    let worker = std::thread::spawn(move || read.read(1, 0, 0, 4));
    loop { if bridge.next_request().is_some() { break; } std::thread::yield_now(); }
    drop(bridge);
    assert!(worker.join().unwrap().is_err());
    assert!(reader.read(1, 0, 0, 4).is_err());
}

fn backend() -> (TextClipboard, mpsc::Receiver<ClipCommand>, Arc<AtomicU32>) {
    let (tx, rx) = mpsc::sync_channel(32);
    let file_format = Arc::new(AtomicU32::new(0));
    (
        TextClipboard::new(
            tx,
            Arc::new(AtomicU32::new(0)),
            Arc::clone(&file_format),
            Arc::new(AtomicU32::new(0)),
        ),
        rx,
        file_format,
    )
}

fn format(id: u32, name: &'static str) -> ClipboardFormat {
    ClipboardFormat::new(ClipboardFormatId::new(id))
        .with_name(ClipboardFormatName::new_static(name))
}

#[test]
#[cfg(windows)]
fn stream_only_client_negotiates_no_remote_source_paths() {
    let (backend, _, _) = backend();
    let capabilities = backend.client_capabilities();
    assert!(capabilities.contains(ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED));
    assert!(
        capabilities.contains(ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS),
        "without NO_FILE_PATHS Windows can offer CF_HDROP source paths instead of stream descriptors"
    );
}

#[test]
#[cfg(windows)]
fn initial_format_names_do_not_reuse_drop_effect_id_for_file_contents() {
    let formats = dbx_plugin_rdp::clipboard::supported_clipboard_formats();
    let find = |name: &str| formats.iter().find(|f| f.name().is_some_and(|n| n.value() == name)).map(|f| f.id().0);
    assert_eq!(find("FileGroupDescriptorW"), Some(0xC0FE));
    assert_eq!(find("Preferred DropEffect"), Some(0xC0FD));
    assert_eq!(find("FileContents"), Some(0xC0FC));
}

#[test]
fn filename_and_drop_hints_are_not_requested_as_file_descriptors() {
    let (mut backend, rx, file_format) = backend();
    backend.on_remote_copy(&[
        ClipboardFormat::new(ClipboardFormatId::CF_HDROP),
        format(49159, "FileNameW"),
        format(49453, "Preferred DropEffect"),
    ]);
    assert_eq!(file_format.load(Ordering::Relaxed), 0);
    assert!(rx.try_iter().all(|c| !matches!(c, ClipCommand::OfferRemoteFiles | ClipCommand::FetchRemoteFiles(_))));
}

#[test]
fn text_copy_clears_previous_remote_file_format() {
    let (mut backend, rx, file_format) = backend();
    backend.on_remote_copy(&[format(49352, "FileGroupDescriptorW")]);
    assert_eq!(file_format.load(Ordering::Relaxed), if cfg!(windows) { 49352 } else { 0 });
    let _ = rx.try_iter().collect::<Vec<_>>();
    backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
    assert_eq!(file_format.load(Ordering::Relaxed), 0);
    assert!(rx.try_iter().any(|c| matches!(c, ClipCommand::FetchRemoteText)));
}

#[test]
#[cfg(windows)]
fn remote_file_list_preserves_protocol_lock_for_virtual_reads() {
    let (mut backend, rx, _) = backend();
    backend.on_remote_file_list(&[file("a", None, Some(1))], Some(73));
    match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        ClipCommand::RemoteFileList { descriptors, data_id } => {
            assert_eq!(data_id, Some(73));
            assert_eq!(descriptors[0].name, "a");
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "dbx-rdp-regression-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ))
}

fn file(name: &str, relative: Option<&str>, size: Option<u64>) -> FileDescriptor {
    let mut descriptor = FileDescriptor::new(name);
    descriptor.relative_path = relative.map(str::to_owned);
    if let Some(size) = size {
        descriptor = descriptor.with_file_size(size);
    }
    descriptor
}

fn wait_publish(rx: &mpsc::Receiver<TransferEvent>) -> Vec<PathBuf> {
    loop {
        match rx.recv_timeout(Duration::from_secs(2)).expect("batch must publish") {
            TransferEvent::Publish { paths } => return paths,
            TransferEvent::Failed { reason } => panic!("transfer failed: {reason}"),
            _ => {}
        }
    }
}

#[test]
fn small_file_first_range_stays_within_descriptor_size() {
    let root = scratch("range");
    let (events_tx, _events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root, events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![(1, 0, file("a.txt", None, Some(3)))])).unwrap();
    let request = requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert_eq!(request.flags, FileContentsFlags::RANGE);
    assert_eq!(request.requested_size, 3, "IronRDP rejects ranges extending past the advertised EOF");
}

#[test]
fn size_probe_followup_range_stays_within_reported_size() {
    let root = scratch("size");
    let (events_tx, _events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root, events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![(1, 0, file("a.txt", None, None))])).unwrap();
    let first = requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first.flags, FileContentsFlags::SIZE);
    transfer.send(TransferCommand::Chunk { stream_id: 1, response: FileContentsResponse::new_size_response(1, 3) }).unwrap();
    let request = requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert_eq!(request.requested_size, 3);
}

#[test]
fn folder_download_publishes_copied_root_not_flattened_children() {
    let root = scratch("folder");
    let (events_tx, events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root.clone(), events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![
        (1, 0, FileDescriptor::new("folder").with_attributes(ClipboardFileAttributes::DIRECTORY)),
        (3, 1, file("a.txt", Some("folder\\sub"), Some(3))),
    ])).unwrap();
    let request = requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(request.index, 1, "directory entries must still count in the wire lindex");
    transfer.send(TransferCommand::Chunk { stream_id: 3, response: FileContentsResponse::new_data_response(3, b"abc".to_vec()) }).unwrap();
    let paths = wait_publish(&events_rx);
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert_eq!(std::fs::read(root.join("folder/sub/a.txt")).unwrap(), b"abc");
    assert_eq!(paths, vec![root.join("folder")], "Explorer must copy the folder as a unit, including its hierarchy");
}

#[test]
fn empty_folder_batch_is_published_without_file_stream_requests() {
    let root = scratch("empty-folder");
    let (events_tx, events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root.clone(), events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![
        (1, 0, FileDescriptor::new("empty").with_attributes(ClipboardFileAttributes::DIRECTORY)),
    ])).unwrap();
    let result = std::panic::catch_unwind(|| wait_publish(&events_rx));
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert!(requests_rx.try_recv().is_err());
    assert_eq!(result.unwrap(), vec![root.join("empty")]);
}

#[test]
fn premature_eof_does_not_publish_incomplete_folder() {
    let root = scratch("truncated");
    let (events_tx, events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root, events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![(1, 0, file("a.txt", Some("folder"), Some(3)))])).unwrap();
    requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    transfer.send(TransferCommand::Chunk { stream_id: 1, response: FileContentsResponse::new_data_response(1, Vec::new()) }).unwrap();
    let mut failed = false;
    while let Ok(event) = events_rx.recv_timeout(Duration::from_millis(200)) {
        assert!(!matches!(event, TransferEvent::Publish { .. }), "incomplete folder must never be published");
        failed |= matches!(event, TransferEvent::Failed { .. });
    }
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert!(failed, "premature EOF must report failure");
}

#[test]
fn failed_stream_does_not_publish_partial_folder() {
    let root = scratch("failed");
    let (events_tx, events_rx) = mpsc::sync_channel(32);
    let (requests_tx, requests_rx) = mpsc::sync_channel(32);
    let transfer = spawn_download_thread(root, events_tx, requests_tx);
    transfer.send(TransferCommand::Download(vec![(1, 0, file("a.txt", Some("folder"), Some(3)))])).unwrap();
    requests_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    transfer.send(TransferCommand::Chunk { stream_id: 1, response: FileContentsResponse::new_error(1) }).unwrap();
    let mut failed = false;
    while let Ok(event) = events_rx.recv_timeout(Duration::from_millis(200)) {
        assert!(!matches!(event, TransferEvent::Publish { .. }), "failed folder must never be published");
        failed |= matches!(event, TransferEvent::Failed { .. });
    }
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert!(failed);
}

#[test]
fn folder_with_more_files_than_request_queue_does_not_stall() {
    let root = scratch("many-files");
    let (events_tx, events_rx) = mpsc::sync_channel(32);
    let published = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        wait_publish(&events_rx)
    });
    let (requests_tx, requests_rx) = mpsc::sync_channel(4);
    let transfer = spawn_download_thread(root, events_tx, requests_tx);
    let entries = (1..=70).map(|id| (id, id as i32 - 1,
        file(&format!("{id}.txt"), Some("folder"), Some(1)))).collect();
    transfer.send(TransferCommand::Download(entries)).unwrap();
    // Wait until initialization has consumed a burst of descriptors before
    // draining requests; production also has bounded queues and a slow peer.
    std::thread::sleep(Duration::from_millis(100));
    let mut received = 0;
    while received < 70 {
        let Ok(request) = requests_rx.recv_timeout(Duration::from_secs(2)) else { break };
        transfer.send(TransferCommand::Chunk {
            stream_id: request.stream_id,
            response: FileContentsResponse::new_data_response(request.stream_id, vec![b'x']),
        }).unwrap();
        received += 1;
    }
    transfer.send(TransferCommand::Shutdown).unwrap();
    assert_eq!(received, 70, "queue pressure must not silently discard file requests");
    assert_eq!(published.join().unwrap().len(), 1);
}
