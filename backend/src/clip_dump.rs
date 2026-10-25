//! CLIPRDR metadata diagnostics. TX is recorded before transport framing;
//! RX skips the eight-byte SVC header and decodes only FIRST chunks.
//! Payload bytes are deliberately omitted (clipboard contents are private).

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

static DUMP_LOCK: Mutex<()> = Mutex::new(());

fn first_chunk_pdu(chunk: &[u8]) -> Option<&[u8]> {
    let header = chunk.get(..8)?;
    let flags = u32::from_le_bytes(header[4..8].try_into().ok()?);
    // FIRST = 1. Continuations start with arbitrary file/text bytes.
    (flags & 1 != 0).then(|| &chunk[8..])
}

pub fn dump_svc_chunk(chunk: &[u8]) {
    if let Some(pdu) = first_chunk_pdu(chunk) {
        dump("RX-FIRST-CHUNK", pdu);
    }
}

fn enabled() -> bool {
    // Default ON for this diagnostic build (the packaged sidecar inherits
    // no shell env): DBX_RDP_CLIP_DUMP=0 disables explicitly.
    match std::env::var("DBX_RDP_CLIP_DUMP") {
        Ok(v) => v != "0",
        Err(_) => true,
    }
}

fn dump_path() -> std::path::PathBuf {
    let base = std::env::var("DBX_PLUGIN_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    base.join("cliprdr-dump.log")
}

/// CLIPRDR message type names (MS-RDPECLIP 2.2.2).
fn msg_type_name(t: u16) -> &'static str {
    match t {
        0x0001 => "CB_MONITOR_READY",
        0x0002 => "CB_FORMAT_LIST",
        0x0003 => "CB_FORMAT_LIST_RESPONSE",
        0x0004 => "CB_FORMAT_DATA_REQUEST",
        0x0005 => "CB_FORMAT_DATA_RESPONSE",
        0x0006 => "CB_TEMP_DIRECTORY",
        0x0007 => "CB_CLIP_CAPS",
        0x0008 => "CB_FILECONTENTS_REQUEST",
        0x0009 => "CB_FILECONTENTS_RESPONSE",
        0x000A => "CB_LOCK_CLIPDATA",
        0x000B => "CB_UNLOCK_CLIPDATA",
        _ => "CB_?",
    }
}

/// Dump one CLIPRDR channel payload with direction tag.
pub fn dump(direction: &str, data: &[u8]) {
    if !enabled() {
        return;
    }
    let _guard = DUMP_LOCK.lock().ok();
    let (msg_type, flags, data_len) = if data.len() >= 8 {
        let t = u16::from_le_bytes([data[0], data[1]]);
        let f = u16::from_le_bytes([data[2], data[3]]);
        let l = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        (t, f, l)
    } else {
        (0, 0, data.len() as u32)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let head = format!(
        "[{direction}] t={now}us msgType=0x{msg_type:04X}({}) flags=0x{flags:04X} dataLen={data_len} total={}",
        msg_type_name(msg_type),
        data.len(),
    );
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(dump_path()) {
        let _ = writeln!(file, "{head}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_wire_message_types() {
        for (id, name) in [
            (1, "CB_MONITOR_READY"), (2, "CB_FORMAT_LIST"),
            (3, "CB_FORMAT_LIST_RESPONSE"), (4, "CB_FORMAT_DATA_REQUEST"),
            (5, "CB_FORMAT_DATA_RESPONSE"), (6, "CB_TEMP_DIRECTORY"),
            (7, "CB_CLIP_CAPS"), (8, "CB_FILECONTENTS_REQUEST"),
            (9, "CB_FILECONTENTS_RESPONSE"), (10, "CB_LOCK_CLIPDATA"),
            (11, "CB_UNLOCK_CLIPDATA"),
        ] {
            assert_eq!(msg_type_name(id), name, "wire message {id}");
        }
    }

    #[test]
    fn svc_header_is_not_mistaken_for_clipboard_header() {
        let first = [12, 0, 0, 0, 1, 0, 0, 0, 5, 0, 1, 0, 4, 0, 0, 0];
        assert_eq!(first_chunk_pdu(&first), Some(&first[8..]));
        let continuation = [12, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 4];
        assert_eq!(first_chunk_pdu(&continuation), None);
        assert_eq!(first_chunk_pdu(&[0; 7]), None);
    }
}
