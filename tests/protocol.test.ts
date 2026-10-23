// Wire-contract tests for src/protocol.ts. The byte layout is mirrored by
// backend/tests/frame_protocol.rs; both must agree with backend/src/frame.rs.
import { describe, expect, it } from "vitest";
import {
  FRAME_CHANNEL,
  INPUT_CHANNEL,
  PTR_BUTTON1,
  PTR_DOWN,
  PTR_MOVE,
  acceptFrame,
  keyPacket,
  mousePacket,
  parseFrame,
  resizePacket,
  unicodePacket,
  wheelPacket,
} from "../src/protocol";

function encodeReferenceFrame(
  connectionId: string,
  desktopWidth: number,
  desktopHeight: number,
  rect: [number, number, number, number],
  seq: number,
  pixels: Uint8Array,
): Uint8Array {
  const id = new TextEncoder().encode(connectionId);
  const head = new Uint8Array(4 + 1 + id.length + 17);
  const view = new DataView(head.buffer);
  head.set([0x52, 0x44, 0x50, 0x31], 0); // "RDP1"
  head[4] = id.length;
  head.set(id, 5);
  let at = 5 + id.length;
  view.setUint16(at, desktopWidth); at += 2;
  view.setUint16(at, desktopHeight); at += 2;
  head[at++] = 0; // RGBA32
  for (const value of rect) { view.setUint16(at, value); at += 2; }
  view.setUint32(at, seq);
  const out = new Uint8Array(head.length + pixels.length);
  out.set(head, 0);
  out.set(pixels, head.length);
  return out;
}

describe("frame decode", () => {
  it("parses a tile produced by the reference encoder", () => {
    const pixels = new Uint8Array(2 * 2 * 4).fill(7);
    const data = encodeReferenceFrame("conn-7", 1280, 800, [8, 9, 2, 2], 42, pixels);
    const frame = parseFrame(data);
    expect(frame).not.toBeNull();
    expect(frame!.connectionId).toBe("conn-7");
    expect([frame!.desktopWidth, frame!.desktopHeight]).toEqual([1280, 800]);
    expect([frame!.x, frame!.y, frame!.width, frame!.height]).toEqual([8, 9, 2, 2]);
    expect(frame!.seq).toBe(42);
    expect(Array.from(frame!.pixels)).toEqual(Array.from(pixels));
  });

  it("rejects a truncated frame", () => {
    const pixels = new Uint8Array(4).fill(1);
    const data = encodeReferenceFrame("c1", 10, 10, [0, 0, 1, 1], 1, pixels);
    expect(parseFrame(data.subarray(0, 10))).toBeNull();
  });
});

describe("acceptFrame routing", () => {
  it("accepts only the frame channel addressed to this connection", () => {
    const pixels = new Uint8Array(4).fill(2);
    const mine = encodeReferenceFrame("me", 64, 64, [0, 0, 1, 1], 1, pixels);
    const theirs = encodeReferenceFrame("other", 64, 64, [0, 0, 1, 1], 1, pixels);
    expect(acceptFrame({ channel: FRAME_CHANNEL, data: mine }, "me")).not.toBeNull();
    expect(acceptFrame({ channel: FRAME_CHANNEL, data: theirs }, "me")).toBeNull();
    expect(acceptFrame({ channel: "input", data: mine }, "me")).toBeNull();
  });
});

describe("input encoders", () => {
  const decodeHead = (packet: Uint8Array) => {
    const len = packet[0];
    const id = new TextDecoder().decode(packet.subarray(1, 1 + len));
    return { id, kind: packet[1 + len], body: packet.subarray(2 + len) };
  };

  it("mouse packet carries flags and coordinates big-endian", () => {
    const { id, kind, body } = decodeHead(mousePacket("c1", PTR_DOWN | PTR_BUTTON1, 300, 200));
    expect(id).toBe("c1");
    expect(kind).toBe(0);
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(view.getUint16(0)).toBe(PTR_DOWN | PTR_BUTTON1);
    expect(view.getUint16(2)).toBe(300);
    expect(view.getUint16(4)).toBe(200);
  });

  it("move packet uses the move flag", () => {
    const { kind, body } = decodeHead(mousePacket("c1", PTR_MOVE, 1, 2));
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(kind).toBe(0);
    expect(view.getUint16(0)).toBe(PTR_MOVE);
  });

  it("key packet uses kind 1 and keeps the extended scancode intact", () => {
    const { kind, body } = decodeHead(keyPacket("c1", 0x0100, 0xe01d));
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(kind).toBe(1);
    expect(view.getUint16(0)).toBe(0x0100);
    expect(view.getUint16(2)).toBe(0xe01d);
  });

  it("wheel packet encodes negative rotation as two's complement", () => {
    const { kind, body } = decodeHead(wheelPacket("c1", false, -120, 5, 6));
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(kind).toBe(2);
    expect(body[0]).toBe(0);
    expect(view.getInt16(1)).toBe(-120);
    expect(view.getUint16(3)).toBe(5);
    expect(view.getUint16(5)).toBe(6);
  });

  it("resize packet uses kind 3", () => {
    const { kind, body } = decodeHead(resizePacket("c1", 1024, 768));
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(kind).toBe(3);
    expect([view.getUint16(0), view.getUint16(2)]).toEqual([1024, 768]);
  });

  it("unicode packet uses kind 4 and carries a UTF-16 unit", () => {
    const { kind, body } = decodeHead(unicodePacket("c1", 0, 0x4e2d));
    const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
    expect(kind).toBe(4);
    expect(view.getUint16(2)).toBe(0x4e2d);
  });

  it("input channel name matches the sidecar contract", () => {
    expect(INPUT_CHANNEL).toBe("input");
  });
});
