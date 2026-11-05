// Wire codec shared with the Rust sidecar (backend/src/frame.rs).
// Keep both files byte-compatible when changing either.

export const FRAME_MAGIC = "RDP1";
export const FRAME_MAGIC_GENERATION = "RDP2";
export const INPUT_CHANNEL = "input";
export const FRAME_CHANNEL = "frame";

export interface FrameHead {
  connectionId: string;
  desktopWidth: number;
  desktopHeight: number;
  x: number;
  y: number;
  width: number;
  height: number;
  seq: number;
  generation: number;
  pixels: Uint8Array;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export function parseFrame(data: Uint8Array): FrameHead | null {
  if (data.length < 4 + 1 + 17) return null;
  const magic = decoder.decode(data.subarray(0, 4));
  if (magic !== FRAME_MAGIC && magic !== FRAME_MAGIC_GENERATION) return null;
  const hasGeneration = magic === FRAME_MAGIC_GENERATION;
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const idLen = data[4];
  const idEnd = 5 + idLen;
  const headerLength = 17 + (hasGeneration ? 4 : 0);
  if (data.length < idEnd + headerLength) return null;
  const connectionId = decoder.decode(data.subarray(5, idEnd));
  let at = idEnd;
  const readU16 = () => {
    const value = view.getUint16(at);
    at += 2;
    return value;
  };
  const desktopWidth = readU16();
  const desktopHeight = readU16();
  const format = data[at++];
  const x = readU16();
  const y = readU16();
  const width = readU16();
  const height = readU16();
  const seq = view.getUint32(at);
  at += 4;
  const generation = hasGeneration ? view.getUint32(at) : 0;
  if (hasGeneration) at += 4;
  if (format !== 0 || desktopWidth === 0 || desktopHeight === 0 || width === 0 || height === 0) return null;
  if (x + width > desktopWidth || y + height > desktopHeight) return null;
  const pixels = data.subarray(at);
  if (pixels.length !== width * height * 4) return null;
  return { connectionId, desktopWidth, desktopHeight, x, y, width, height, seq, generation, pixels };
}

function pushId(target: number[], connectionId: string) {
  const id = encoder.encode(connectionId);
  const len = Math.min(id.length, 255);
  target.push(len, ...id.subarray(0, len));
}

const u16 = (value: number) => [(value >> 8) & 0xff, value & 0xff];
const i16 = (value: number) => u16(value & 0xffff);

// PointerFlags bits (MS-RDPBCGR 2.2.8.1.2.4.1)
export const PTR_MOVE = 0x0800;
export const PTR_DOWN = 0x8000;
export const PTR_BUTTON1 = 0x1000;
export const PTR_BUTTON2 = 0x2000;
export const PTR_BUTTON3 = 0x4000;

export function mousePacket(connectionId: string, flags: number, x: number, y: number): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(0, ...u16(flags), ...u16(x), ...u16(y));
  return new Uint8Array(body);
}

export function keyPacket(connectionId: string, flags: number, code: number): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(1, ...u16(flags), ...u16(code));
  return new Uint8Array(body);
}

export function wheelPacket(
  connectionId: string,
  isHorizontal: boolean,
  rotation: number,
  x: number,
  y: number,
): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(2, isHorizontal ? 1 : 0, ...i16(rotation), ...u16(x), ...u16(y));
  return new Uint8Array(body);
}

export function resizePacket(connectionId: string, width: number, height: number): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(3, ...u16(width), ...u16(height));
  return new Uint8Array(body);
}

export function unicodePacket(connectionId: string, flags: number, code: number): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(4, ...u16(flags), ...u16(code));
  return new Uint8Array(body);
}

export function applySizePacket(connectionId: string, width: number, height: number): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(5, ...u16(width), ...u16(height));
  return new Uint8Array(body);
}

/** Initial automatic sizing, atomically deduplicated by the backend session. */
export function sessionReadyPacket(connectionId: string): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(9);
  return new Uint8Array(body);
}

export function initialSizePacket(connectionId: string, width: number, height: number, sessionId: string): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(8, ...u16(width), ...u16(height));
  pushId(body, sessionId);
  return new Uint8Array(body);
}

/// kind 6: the user pressed the workbench's "pull remote files" button —
/// the sidecar re-arms the deferred offer and restarts the background pull.
export function pullFilesPacket(connectionId: string): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(6);
  return new Uint8Array(body);
}

/** kind 7: retry an existing session after its automatic reconnect budget. */
export function retryPacket(connectionId: string): Uint8Array {
  const body: number[] = [];
  pushId(body, connectionId);
  body.push(7);
  return new Uint8Array(body);
}

export interface BinaryEnvelope {
  channel: string;
  data: Uint8Array;
}

/**
 * Decide whether an inbound binary envelope belongs to this workbench and,
 * when so, decode the desktop frame. Frames carry the connection id because
 * the host broadcasts sidecar frames to every open workbench.
 */
export function acceptFrame(envelope: BinaryEnvelope, connectionId: string): FrameHead | null {
  if (envelope.channel !== FRAME_CHANNEL) return null;
  const frame = parseFrame(envelope.data);
  if (!frame || frame.connectionId !== connectionId) return null;
  return frame;
}
