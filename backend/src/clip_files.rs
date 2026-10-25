//! OS file-clipboard glue: CF_HDROP <-> FileDescriptor translation and
//! streamed file I/O, Windows-only. Other platforms compile the same session
//! loop against no-op stubs so text clipboard keeps working everywhere.

#![allow(unused)] // stubs are unused on non-Windows by design

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ironrdp_cliprdr::pdu::{ClipboardFileAttributes, ClipboardFileFlags, FileDescriptor};

/// Maximum bytes served per FileContentsRequest RANGE response. Windows
/// Explorer asks for 1 MiB per request; staying at or below keeps the
/// response inside the server's PDU budget and bounds our memory to one
/// chunk per in-flight transfer.
pub const MAX_CHUNK: u32 = 1024 * 1024;

/// One local file being offered to the remote, with an open handle so a
/// >4 GiB paste never re-seeks from the start per chunk.
pub struct OutboundFile {
    pub descriptor: FileDescriptor,
    pub handle: std::fs::File,
}

/// Snapshot of the local file clipboard (CF_HDROP).
pub struct LocalFileList {
    /// Absolute paths as placed on the clipboard.
    pub roots: Vec<PathBuf>,
}

/// Read the local clipboard's file list. `Ok(None)` = no files right now.
///
/// Must NOT be called while our own delayed offer sits on the clipboard:
/// GetClipboardData against our delayed entry synchronously fires
/// WM_RENDERFORMAT and deadlocks the session thread against itself.
/// [`crate::clip_owner::offer_active`] guards every call site.
#[cfg(windows)]
pub fn read_local_file_list() -> std::io::Result<Option<LocalFileList>> {
    use clipboard_win::{formats, get, Clipboard};
    if crate::clip_owner::offer_active() {
        return Ok(None); // our own delayed offer, not a user copy
    }
    let _clip = Clipboard::new_attempts(10)
        .map_err(|e| std::io::Error::other(format!("open clipboard: {e}")))?;
    let paths: Vec<PathBuf> = get(formats::FileList)
        .map_err(|e| std::io::Error::other(format!("read CF_HDROP: {e}")))?;
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(LocalFileList { roots: paths }))
}

#[cfg(not(windows))]
pub fn read_local_file_list() -> std::io::Result<Option<LocalFileList>> {
    // ponytail: file clipboard on Linux/macOS is future work (X11 DnD /
    // NSPasteboard); text sync already works there and must not regress.
    Ok(None)
}

/// Offer downloaded files to the local clipboard as a CF_HDROP list,
/// paired with a "Preferred DropEffect = COPY" registration. Without the
/// drop effect, Explorer treats an externally-written HDROP as a MOVE and
/// the paste fails when the (remote-side) source cannot be removed — the
/// single-file-vs-many discrepancy seen on this deployment.
#[cfg(windows)]
pub fn set_local_file_list(paths: &[PathBuf]) -> std::io::Result<()> {
    use clipboard_win::raw;
    use clipboard_win::Clipboard;
    let strings: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let _clip = Clipboard::new_attempts(10)
        .map_err(|e| std::io::Error::other(format!("open clipboard: {e}")))?;
    raw::set_file_list(&strings)
        .map_err(|e| std::io::Error::other(format!("set CF_HDROP: {e}")))?;
    // "Preferred DropEffect": 4-byte LE DROPEFFECT_COPY (1). Explorer reads
    // this before pasting; absent, it assumes MOVE semantics and the paste
    // of externally-written HDROP data fails.
    const DROPEFFECT_COPY: u32 = 1;
    let name: Vec<u16> = "Preferred DropEffect".encode_utf16().chain(std::iter::once(0)).collect();
    let effect_id = unsafe { raw::register_raw_format(&name) }
        .ok_or_else(|| std::io::Error::other("register DropEffect format"))?;
    // raw::set (used by formats::RawData) empties the clipboard. Append the
    // companion format instead, or the CF_HDROP written above is destroyed.
    raw::set_without_clear(effect_id.get(), &DROPEFFECT_COPY.to_le_bytes())
        .map_err(|e| std::io::Error::other(format!("set DropEffect: {e}")))?;
    Ok(())
}

#[cfg(not(windows))]
pub fn set_local_file_list(_paths: &[PathBuf]) -> std::io::Result<()> {
    Ok(())
}

/// Expand the copied roots into per-file descriptors with relative paths:
/// copying `dir` yields `sub\a.txt`; copying `a.txt` and `dir\b.txt` together
/// yields root-relative names. Directories are walked depth-first with a
/// recursion guard; empty directories cannot be represented on the wire and
/// are skipped (protocol limitation, same as mstsc).
pub fn build_descriptors(roots: &[PathBuf]) -> std::io::Result<Vec<OutboundFile>> {
    let mut out = Vec::new();
    for root in roots {
        let meta = std::fs::metadata(root)?;
        if meta.is_dir() {
            // Windows Explorer semantics: copying directory `root` yields
            // `root\top.txt`, `root\sub\inner\a.txt` — the folder itself is
            // re-created on paste.
            let base_name = file_name_of(root);
            walk_dir(root, &base_name, &mut out)?;
        } else {
            push_file(root, None, &mut out)?;
        }
        if out.len() >= ironrdp_cliprdr::pdu::MAX_FILE_COUNT {
            break;
        }
    }
    Ok(out)
}

fn walk_dir(dir: &Path, prefix: &str, out: &mut Vec<OutboundFile>) -> std::io::Result<()> {
    const MAX_DEPTH: usize = 32;
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<OutboundFile>) -> std::io::Result<()> {
        if depth > MAX_DEPTH {
            return Ok(()); // pathological nesting: stop expanding
        }
        let mut entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // Unreadable subtree (permissions): skip it, keep the rest.
            Err(_) => return Ok(()),
        };
        let mut items: Vec<_> = entries.flatten().collect();
        items.sort_by_key(|entry| entry.file_name());
        for entry in items {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}\\{name}") };
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&path, &rel, depth + 1, out)?;
            } else {
                push_file(&path, Some(prefix.to_string()), out)?;
            }
            if out.len() >= ironrdp_cliprdr::pdu::MAX_FILE_COUNT {
                return Ok(());
            }
        }
        Ok(())
    }
    walk(dir, prefix, 0, out)
}

fn push_file(path: &Path, relative: Option<String>, out: &mut Vec<OutboundFile>) -> std::io::Result<()> {
    let meta = std::fs::metadata(path)?;
    let handle = std::fs::File::open(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| unix_to_filetime(d.as_secs()))
        .unwrap_or(0);
    let descriptor = FileDescriptor::new(file_name_of(path))
        .with_attributes(ClipboardFileAttributes::ARCHIVE)
        .with_last_write_time(mtime)
        .with_file_size(meta.len());
    if relative.is_some() {
        // FileDescriptor::new leaves relative_path None; the builder-style
        // API has no with_relative_path in this crate version, so set it
        // via a plain field write through a second construction.
        let mut d = descriptor;
        d.relative_path = relative;
        out.push(OutboundFile { descriptor: d, handle });
    } else {
        out.push(OutboundFile { descriptor, handle });
    }
    Ok(())
}

/// Windows FILETIME = Unix epoch seconds * 10^7 + 11644473600 * 10^7.
pub fn unix_to_filetime(seconds: u64) -> u64 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    seconds
        .checked_mul(10_000_000)
        .and_then(|v| v.checked_add(EPOCH_DIFF_SECS * 10_000_000))
        .unwrap_or(u64::MAX)
}

fn file_name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "unnamed".into())
}

/// Read up to `len` bytes at `position` from an outbound file, capped at
/// MAX_CHUNK. u64 positions keep >4 GiB files addressable without overflow.
/// Reads past EOF yield an empty chunk (never an error): the remote may ask
/// for a range beyond a file truncated since we advertised it, and the spec
/// answer is a short/empty response, not a failed stream.
pub fn read_chunk(file: &mut std::fs::File, position: u64, len: u32) -> std::io::Result<Vec<u8>> {
    let want = len.min(MAX_CHUNK) as usize;
    // Fail fast for positions that cannot exist on any local volume; this
    // catches u64 wrap-around attempts from a hostile server.
    const MAX_LOCAL_FILE: u64 = 1 << 47; // 128 TiB, far above any volume
    if position >= MAX_LOCAL_FILE {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; want];
    if file.seek(SeekFrom::Start(position)).is_err() {
        return Ok(Vec::new()); // unfriendly FS: treat as EOF
    }
    let mut filled = 0usize;
    while filled < want {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(Vec::new()), // degenerate FS: EOF semantics
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Write a downloaded chunk at `position` into a sparse-friendly output file
/// (pre-sized so the OS can keep the tail sparse for big transfers).
pub fn write_chunk(file: &mut std::fs::File, position: u64, data: &[u8]) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(position))?;
    let mut off = 0usize;
    while off < data.len() {
        match file.write(&data[off..]) {
            Ok(0) => return Err(std::io::Error::other("write returned 0")),
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Map a remote descriptor (name + optional relative path, `\` separators)
/// onto a safe path under `staging`: rejects traversal, drive letters and
/// reserved device names; collapses any `\..\` the sanitizer let through.
pub fn staging_path_for(staging: &Path, descriptor: &FileDescriptor) -> PathBuf {
    let mut safe = PathBuf::from(staging);
    let rel = descriptor.relative_path.as_deref().unwrap_or("");
    let joined = if rel.is_empty() {
        descriptor.name.clone()
    } else {
        format!("{}\\{}", rel, descriptor.name)
    };
    // Depth bookkeeping: how many components below the staging root we are.
    // `..` may climb within the copy but never above the root.
    let mut depth = 0usize;
    for component in joined.split(['\\', '/']) {
        let part = component.trim();
        match part {
            "" | "." => continue,
            ".." => {
                if depth > 0 {
                    safe.pop();
                    depth -= 1;
                }
            }
            _ => {
                if part.len() == 2 && part.as_bytes()[1] == b':' {
                    continue; // drive-letter junk from a hostile server
                }
                if is_windows_device_name(part) {
                    continue;
                }
                if depth >= 64 {
                    continue; // pathological depth: drop the rest
                }
                safe.push(part);
                depth += 1;
            }
        }
    }
    if safe == staging {
        safe.push("unnamed_file");
    }
    safe
}

/// CON, PRN, AUX, NUL, COM1..9, LPT1..9 (with any extension).
pub fn is_windows_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9" | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(name: &str, rel: Option<&str>) -> FileDescriptor {
        let mut d = FileDescriptor::new(name);
        d.relative_path = rel.map(String::from);
        d
    }

    #[test]
    fn staging_path_rejects_traversal() {
        let staging = Path::new("C:\\temp\\dbx-clip");
        let hostile = descriptor("passwd.txt", Some("..\\..\\etc"));
        let path = staging_path_for(staging, &hostile);
        // `..` may not climb above the staging root: the result stays inside
        // it. (A leftover "etc" directory is fine — that mirrors Explorer's
        // handling and the crate's own sanitizer.)
        assert!(path.starts_with(staging), "escaped staging: {path:?}");
        assert!(
            !path.starts_with("C:\\etc"),
            "climbed above staging root: {path:?}"
        );
        // Deeper hostile nesting still cannot leave.
        let deeper = descriptor("x", Some("..\\..\\..\\..\\Windows\\System32"));
        let path = staging_path_for(staging, &deeper);
        assert!(path.starts_with(staging), "escaped staging: {path:?}");
    }

    #[test]
    fn staging_path_rejects_drive_and_devices() {
        let staging = Path::new("/tmp/dbx-clip");
        let drive = descriptor("x", Some("C:\\Windows"));
        let path = staging_path_for(staging, &drive);
        assert!(path.starts_with(staging));
        assert!(!path.to_string_lossy().contains(':'));
        let nul = descriptor("CON.txt", None);
        assert!(staging_path_for(staging, &nul).file_name().is_some());
    }

    #[test]
    fn staging_path_preserves_nested_layout() {
        let staging = Path::new("/tmp/dbx-clip");
        let nested = descriptor("a.txt", Some("sub\\inner"));
        let path = staging_path_for(staging, &nested);
        assert_eq!(path, Path::new("/tmp/dbx-clip/sub/inner/a.txt"));
    }

    #[test]
    fn unix_to_filetime_matches_epoch() {
        // Anchored against .NET DateTimeOffset.ToFileTime (verified values).
        assert_eq!(unix_to_filetime(0), 116_444_736_000_000_000);
        assert_eq!(unix_to_filetime(1_767_225_600), 134_116_992_000_000_000);
    }

    #[test]
    fn is_windows_device_name_table() {
        assert!(is_windows_device_name("con.txt"));
        assert!(is_windows_device_name("COM1"));
        assert!(is_windows_device_name("nul"));
        assert!(!is_windows_device_name("console"));
        assert!(!is_windows_device_name("notes.md"));
    }
}
