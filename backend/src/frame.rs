//! Phase 3 wire codec: desktop frames (sidecar -> UI) and input events (UI -> sidecar).
//!
//! Frame layout (big endian, one stdio-framed binary message per tile):
//!   magic "RDP1"(4) | connIdLen u8 | connectionId | desktopWidth u16 | desktopHeight u16
//!   | format u8 | x u16 | y u16 | width u16 | height u16 | seq u32 | RGBA bytes
//!
//! Input layout (one binary message per event, channel "input"):
//!   connIdLen u8 | connectionId | kind u8 | payload
//!     kind 0 = mouse  : flags u16 | x u16 | y u16
//!     kind 1 = key    : flags u16 | code u16
//!     kind 2 = wheel  : isHorizontal u8 | rotation i16 | x u16 | y u16
//!     kind 3 = resize : width u16 | height u16
//!     kind 4 = unicode: flags u16 | code u16  (IME commits / pasted text)
//!     kind 5 = apply  : width u16 | height u16  (reconnect at this size)

pub const FRAME_CHANNEL: &str = "frame";
pub const INPUT_CHANNEL: &str = "input";
pub const STATUS_EVENT: &str = "rdp/status";
pub const DESKTOP_EVENT: &str = "rdp/desktop";

pub const FORMAT_RGBA32: u8 = 0;

/// Per-frame pixel budget for tile splitting.
pub const MAX_TILE_PIXELS: usize = 800_000;
const MAX_TILE_ROWS: u16 = 512;

/// Bytes consumed before the pixel payload: magic(4)+len(1)+17 header fields.
const HEADER_BASE: usize = 4 + 1;
const HEADER_TAIL: usize = 2 + 2 + 1 + 2 + 2 + 2 + 2 + 4; // dw dh fmt x y w h seq

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHead {
    pub connection_id: String,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub format: u8,
    /// (x, y, width, height)
    pub rect: (u16, u16, u16, u16),
    pub seq: u32,
    pub pixels_offset: usize,
}

pub fn encode_frame(
    connection_id: &str,
    desktop_width: u16,
    desktop_height: u16,
    format: u8,
    rect: (u16, u16, u16, u16),
    seq: u32,
    pixels: &[u8],
) -> Vec<u8> {
    let id = connection_id.as_bytes();
    let id_len = id.len().min(u8::MAX as usize) as u8;
    let mut out = Vec::with_capacity(HEADER_BASE + id_len as usize + HEADER_TAIL + pixels.len());
    out.extend_from_slice(b"RDP1");
    out.push(id_len);
    out.extend_from_slice(&id[..id_len as usize]);
    out.extend_from_slice(&desktop_width.to_be_bytes());
    out.extend_from_slice(&desktop_height.to_be_bytes());
    out.push(format);
    for value in [rect.0, rect.1, rect.2, rect.3] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(pixels);
    out
}

pub fn parse_header(data: &[u8]) -> Result<FrameHead, String> {
    if data.len() < 5 {
        return Err("frame too short".to_string());
    }
    if &data[0..4] != b"RDP1" {
        return Err("bad frame magic".to_string());
    }
    let id_len = data[4] as usize;
    let need = HEADER_BASE + id_len + HEADER_TAIL;
    if data.len() < need {
        return Err("truncated frame header".to_string());
    }
    let connection_id = String::from_utf8(data[5..5 + id_len].to_vec())
        .map_err(|_| "frame connection id is not UTF-8".to_string())?;
    let mut at = 5 + id_len;
    let desktop_width = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let desktop_height = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let format = data[at];
    at += 1;
    let x = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let y = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let width = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let height = u16::from_be_bytes([data[at], data[at + 1]]);
    at += 2;
    let seq = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    at += 4;
    Ok(FrameHead {
        connection_id,
        desktop_width,
        desktop_height,
        format,
        rect: (x, y, width, height),
        seq,
        pixels_offset: at,
    })
}

/// Split a dirty rectangle into tiles that respect the pixel budget so every
/// stdio frame stays well under the host's binary-size limit.
pub fn tile_rect(_desktop_width: u16, _desktop_height: u16, rect: (u16, u16, u16, u16)) -> Vec<(u16, u16, u16, u16)> {
    let (x, y, width, height) = rect;
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let area = width as usize * height as usize;
    if area <= MAX_TILE_PIXELS && height <= MAX_TILE_ROWS {
        return vec![rect];
    }
    let rows_per_tile = MAX_TILE_ROWS
        .min(((MAX_TILE_PIXELS / width.max(1) as usize).max(1)) as u16)
        .max(1);
    let mut tiles = Vec::new();
    let mut offset = 0u16;
    while offset < height {
        let band = rows_per_tile.min(height - offset);
        tiles.push((x, y + offset, width, band));
        offset += band;
    }
    tiles
}

// ---------------------------------------------------------------- input

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Mouse { flags: u16, x: u16, y: u16 },
    Key { flags: u16, code: u16 },
    Wheel { is_horizontal: bool, rotation: i16, x: u16, y: u16 },
    Resize { width: u16, height: u16 },
    Unicode { flags: u16, code: u16 },
    /// User clicked Apply in the workbench: reconnect at this desktop size.
    ApplySize { width: u16, height: u16 },
    /// User clicked the workbench's "pull remote files" button: re-run the
    /// deferred-offer arm + background pull so a folder copy whose server
    /// render arrived late can be fetched with one explicit click.
    PullFiles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub connection_id: String,
    pub event: InputKind,
}

fn push_id(out: &mut Vec<u8>, connection_id: &str) {
    let id = connection_id.as_bytes();
    let len = id.len().min(u8::MAX as usize);
    out.push(len as u8);
    out.extend_from_slice(&id[..len]);
}

pub fn mouse_bytes(connection_id: &str, flags: u16, x: u16, y: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    push_id(&mut out, connection_id);
    out.push(0);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out
}

pub fn key_bytes(connection_id: &str, flags: u16, code: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    push_id(&mut out, connection_id);
    out.push(1);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    out
}

pub fn wheel_bytes(connection_id: &str, is_horizontal: bool, rotation: i16, x: u16, y: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    push_id(&mut out, connection_id);
    out.push(2);
    out.push(u8::from(is_horizontal));
    out.extend_from_slice(&rotation.to_be_bytes());
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out
}

pub fn resize_bytes(connection_id: &str, width: u16, height: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    push_id(&mut out, connection_id);
    out.push(3);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out
}

pub fn unicode_bytes(connection_id: &str, flags: u16, code: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    push_id(&mut out, connection_id);
    out.push(4);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    out
}

pub fn parse_input(data: &[u8]) -> Result<InputMessage, String> {
    if data.is_empty() {
        return Err("empty input packet".to_string());
    }
    let id_len = data[0] as usize;
    if data.len() < 1 + id_len + 1 {
        return Err("truncated input packet".to_string());
    }
    let connection_id = String::from_utf8(data[1..1 + id_len].to_vec())
        .map_err(|_| "input connection id is not UTF-8".to_string())?;
    let kind = data[1 + id_len];
    let body = &data[2 + id_len..];
    let read_u16 = |index: usize| -> Result<u16, String> {
        if body.len() < index + 2 {
            return Err("truncated input payload".to_string());
        }
        Ok(u16::from_be_bytes([body[index], body[index + 1]]))
    };
    let event = match kind {
        0 => InputKind::Mouse {
            flags: read_u16(0)?,
            x: read_u16(2)?,
            y: read_u16(4)?,
        },
        1 => InputKind::Key {
            flags: read_u16(0)?,
            code: read_u16(2)?,
        },
        2 => InputKind::Wheel {
            is_horizontal: body.first().copied().unwrap_or(0) != 0,
            rotation: read_u16(1)? as i16,
            x: read_u16(3)?,
            y: read_u16(5)?,
        },
        3 => InputKind::Resize {
            width: read_u16(0)?,
            height: read_u16(2)?,
        },
        4 => InputKind::Unicode {
            flags: read_u16(0)?,
            code: read_u16(2)?,
        },
        5 => InputKind::ApplySize {
            width: read_u16(0)?,
            height: read_u16(2)?,
        },
        6 => InputKind::PullFiles,
        other => return Err(format!("unknown input kind {other}")),
    };
    Ok(InputMessage { connection_id, event })
}

/// Status notification payload (no secrets ever).
pub fn status_payload(connection_id: &str, state: &str, detail: Option<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "connectionId": connection_id,
        "state": state,
        "detail": detail.unwrap_or(serde_json::Value::Null)
    })
}
