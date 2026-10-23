// Scan-code table tests for src/keymap.ts. The values mirror the PC/AT set-1
// table the sidecar feeds into IronRDP fast-path keyboard events; a wrong entry
// silently types a different character on the remote, so each group is pinned.
import { describe, expect, it } from "vitest";
import { EXTENDED_FLAG, MODIFIER_CODES, RELEASE_FLAG, isModifier, mapKey } from "../src/keymap";

describe("letter scan codes", () => {
  // Real PS/2 set-1 codes. The old arithmetic implementation (0x1e + index)
  // was correct only for 'A', which is why remote typing looked "almost right".
  const expected: Record<string, number> = {
    KeyQ: 0x10, KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14, KeyY: 0x15,
    KeyU: 0x16, KeyI: 0x17, KeyO: 0x18, KeyP: 0x19,
    KeyA: 0x1e, KeyS: 0x1f, KeyD: 0x20, KeyF: 0x21, KeyG: 0x22, KeyH: 0x23,
    KeyJ: 0x24, KeyK: 0x25, KeyL: 0x26,
    KeyZ: 0x2c, KeyX: 0x2d, KeyC: 0x2e, KeyV: 0x2f, KeyB: 0x30, KeyN: 0x31,
    KeyM: 0x32,
  };

  it("maps every letter to its physical position", () => {
    for (const [code, scan] of Object.entries(expected)) {
      expect(mapKey(code), code).toEqual({ flags: 0, scanCode: scan });
    }
  });

  it("is not the old alphabetical formula", () => {
    // KeyC is the Ctrl+C key; the broken formula produced 0x20 (KeyD).
    expect(mapKey("KeyC")!.scanCode).toBe(0x2e);
    expect(mapKey("KeyC")!.scanCode).not.toBe(0x20);
    expect(mapKey("KeyV")!.scanCode).toBe(0x2f);
    expect(mapKey("KeyX")!.scanCode).toBe(0x2d);
  });
});

describe("modifier keys", () => {
  it("forwards all eight physical modifiers", () => {
    for (const code of MODIFIER_CODES) {
      expect(mapKey(code), `${code} must be mappable`).not.toBeNull();
      expect(isModifier(code)).toBe(true);
    }
  });

  it("marks only the right-hand modifiers extended", () => {
    // Left modifiers are single-byte keys; the old EXTENDED_KEYS set wrongly
    // marked them extended, which made the server see an unrelated key.
    expect(mapKey("ControlLeft")).toEqual({ flags: 0, scanCode: 0x1d });
    expect(mapKey("ShiftLeft")).toEqual({ flags: 0, scanCode: 0x2a });
    expect(mapKey("AltLeft")).toEqual({ flags: 0, scanCode: 0x38 });
    expect(mapKey("ControlRight")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe01d });
    expect(mapKey("AltRight")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe038 });
  });

  it("treats non-modifiers as non-modifiers", () => {
    for (const code of ["KeyA", "Escape", "ArrowLeft", "NumpadEnter"]) {
      expect(isModifier(code)).toBe(false);
    }
  });
});

describe("shortcut chords stay reachable", () => {
  // The UI forwards Ctrl/Alt/Meta chords as physical keys; every member of the
  // common clipboard and editing chords must therefore be mappable.
  it("maps every key used by Ctrl+C/V/X/A/Z/Y/S/F", () => {
    for (const code of ["KeyC", "KeyV", "KeyX", "KeyA", "KeyZ", "KeyY", "KeyS", "KeyF"]) {
      expect(mapKey(code), code).not.toBeNull();
    }
  });

  it("maps the navigation and editing keys", () => {
    expect(mapKey("ArrowLeft")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe04b });
    expect(mapKey("ArrowUp")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe048 });
    expect(mapKey("ArrowRight")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe04d });
    expect(mapKey("ArrowDown")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe050 });
    expect(mapKey("Home")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe047 });
    expect(mapKey("End")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe04f });
    expect(mapKey("Delete")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe053 });
    expect(mapKey("Insert")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe052 });
    expect(mapKey("Escape")).toEqual({ flags: 0, scanCode: 0x01 });
    expect(mapKey("Enter")).toEqual({ flags: 0, scanCode: 0x1c });
    expect(mapKey("Tab")).toEqual({ flags: 0, scanCode: 0x0f });
    expect(mapKey("Space")).toEqual({ flags: 0, scanCode: 0x39 });
  });
});

describe("digits, function keys and punctuation", () => {
  it("maps digits to the number row", () => {
    expect(mapKey("Digit1")!.scanCode).toBe(0x02);
    expect(mapKey("Digit0")!.scanCode).toBe(0x0b);
  });

  it("maps F1-F12 to the function row", () => {
    expect(mapKey("F1")!.scanCode).toBe(0x3b);
    expect(mapKey("F10")!.scanCode).toBe(0x44);
    expect(mapKey("F11")!.scanCode).toBe(0x57);
    expect(mapKey("F12")!.scanCode).toBe(0x58);
  });

  it("maps the numeric keypad with an extended Enter and Divide", () => {
    expect(mapKey("Numpad0")).toEqual({ flags: 0, scanCode: 0x52 });
    expect(mapKey("NumpadAdd")).toEqual({ flags: 0, scanCode: 0x4e });
    expect(mapKey("NumpadEnter")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe01c });
    expect(mapKey("NumpadDivide")).toEqual({ flags: EXTENDED_FLAG, scanCode: 0xe035 });
  });

  it("returns null for keys this client does not forward", () => {
    for (const code of ["", "Unidentified", "MediaPlayPause", "BrowserBack"]) {
      expect(mapKey(code)).toBeNull();
    }
  });
});

describe("wire flag constants", () => {
  it("matches the sidecar's slow-path bit layout", () => {
    // backend/src/rdp.rs: EXTENDED 0x0100, RELEASE 0x8000.
    expect(EXTENDED_FLAG).toBe(0x0100);
    expect(RELEASE_FLAG).toBe(0x8000);
  });

  it("keeps extended and release bits independently composable", () => {
    const extended = mapKey("ControlRight")!;
    expect(extended.flags | RELEASE_FLAG).toBe(0x8100);
  });
});
