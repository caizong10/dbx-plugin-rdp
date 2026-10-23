import type { BinaryEnvelope, DbxPluginBridge } from "./dbx";
import { RELEASE_FLAG, isModifier, mapKey } from "./keymap";
import {
  INPUT_CHANNEL,
  PTR_BUTTON1,
  PTR_BUTTON2,
  PTR_BUTTON3,
  PTR_DOWN,
  PTR_MOVE,
  acceptFrame,
  applySizePacket,
  keyPacket,
  mousePacket,
  unicodePacket,
  wheelPacket,
} from "./protocol";

const bridge = window.dbxPlugin as DbxPluginBridge | undefined;

const eyebrow = document.querySelector<HTMLSpanElement>("#eyebrow")!;
const canvasWrap = document.querySelector<HTMLElement>(".canvas-wrap")!;
const canvas = document.querySelector<HTMLCanvasElement>("#screen")!;
const overlay = document.querySelector<HTMLDivElement>("#overlay")!;
const reconnectBar = document.querySelector<HTMLDivElement>("#reconnect")!;
const retryBtn = document.querySelector<HTMLButtonElement>("#retry")!;
const resSelect = document.querySelector<HTMLSelectElement>("#resolution")!;
const resLabel = document.querySelector<HTMLSpanElement>("#res-label")!;
const scaleSelect = document.querySelector<HTMLSelectElement>("#scale")!;
const scaleLabel = document.querySelector<HTMLSpanElement>("#scale-label")!;
const applyBtn = document.querySelector<HTMLButtonElement>("#apply")!;
const imeInput = document.querySelector<HTMLTextAreaElement>("#ime")!;

const ctx2d = canvas.getContext("2d", { alpha: false })!;

const RESOLUTIONS = [
  "1280x720",
  "1366x768",
  "1600x900",
  "1920x1080",
  "2560x1440",
];

const COPY: Record<"en" | "zh", Record<string, string>> = {
  en: {
    eyebrow: "DBX RDP Workbench",
    res: "Resolution",
    scale: "Scale",
    apply: "Apply",
    fitWindow: "Fit window",
    retry: "Retry now",
    lost: "Connection lost — retry with the button",
    connecting: "Connecting…",
    open: "Opening remote desktop…",
    frames: "Receiving screen…",
    noSession: "This workbench has no connection. Open it from a connection.",
    connectFailed: "Connection failed",
  },
  zh: {
    eyebrow: "DBX RDP 工作台",
    res: "分辨率",
    scale: "缩放",
    apply: "应用",
    fitWindow: "适应窗口",
    retry: "立即重试",
    lost: "连接已断开 — 可点击按钮重试",
    connecting: "正在连接…",
    open: "正在打开远程桌面…",
    frames: "正在接收画面…",
    noSession: "此工作台没有连接，请从连接项打开。",
    connectFailed: "连接失败",
  },
};

let locale = bridge?.locale ?? "en";
function lang(): "en" | "zh" {
  return locale.toLowerCase().startsWith("zh") ? "zh" : "en";
}
function t(key: string, args?: Record<string, string | number>): string {
  const table = COPY[lang()] ?? COPY.en;
  let value = table[key] ?? COPY.en[key] ?? key;
  if (args) for (const [name, replacement] of Object.entries(args)) value = value.replace(`{${name}}`, String(replacement));
  return value;
}

let connectionId = "";
let remoteWidth = 0;
let remoteHeight = 0;
let gotFrame = false;

// Off-screen copy of the remote framebuffer; the visible canvas scales it.
const imageBuffer = document.createElement("canvas");
const imageCtx = imageBuffer.getContext("2d")!;

// Released buttons bookkeeping so a blur cannot leave the remote stuck.
const heldButtons = new Set<number>();
let lastPoint: [number, number] = [0, 0];

// ---------------------------------------------------------------- painting

function applyFrame(envelope: BinaryEnvelope): void {
  const frame = acceptFrame(envelope, connectionId);
  if (!frame) return;
  if (frame.desktopWidth !== remoteWidth || frame.desktopHeight !== remoteHeight) {
    remoteWidth = frame.desktopWidth;
    remoteHeight = frame.desktopHeight;
    imageBuffer.width = remoteWidth;
    imageBuffer.height = remoteHeight;
    canvas.width = remoteWidth;
    canvas.height = remoteHeight;
    applyScale();
    paintPlaceholder();
  }
  if (frame.x + frame.width > remoteWidth || frame.y + frame.height > remoteHeight) return;
  const source = new ImageData(new Uint8ClampedArray(frame.pixels), frame.width, frame.height);
  imageCtx.putImageData(source, frame.x, frame.y);
  // Blit only the dirty region, batched to one paint per animation frame:
  // the canvas backing store is 1:1 with the remote desktop, so source and
  // destination coordinates match exactly. A full-screen copy per tile was
  // the main source of perceived lag.
  markDirty(frame.x, frame.y, frame.width, frame.height);
  if (!gotFrame) {
    gotFrame = true;
    overlay.textContent = "";
    reconnectBar.hidden = true;
  }
}

// One pending dirty rectangle (union of tiles) flushed on the next frame.
let dirty: { x: number; y: number; w: number; h: number } | null = null;
let drawScheduled = false;

function markDirty(x: number, y: number, w: number, h: number): void {
  if (!dirty) {
    dirty = { x, y, w, h };
  } else {
    const right = Math.max(dirty.x + dirty.w, x + w);
    const bottom = Math.max(dirty.y + dirty.h, y + h);
    dirty.x = Math.min(dirty.x, x);
    dirty.y = Math.min(dirty.y, y);
    dirty.w = right - dirty.x;
    dirty.h = bottom - dirty.y;
  }
  if (!drawScheduled) {
    drawScheduled = true;
    requestAnimationFrame(flushDirty);
  }
}

function flushDirty(): void {
  drawScheduled = false;
  const region = dirty;
  dirty = null;
  if (region) drawRegion(region.x, region.y, region.w, region.h);
}

function drawRegion(x: number, y: number, width: number, height: number): void {
  ctx2d.imageSmoothingEnabled = true;
  ctx2d.drawImage(imageBuffer, x, y, width, height, x, y, width, height);
}

function drawToScreen(): void {
  ctx2d.imageSmoothingEnabled = true;
  ctx2d.clearRect(0, 0, canvas.width, canvas.height);
  ctx2d.drawImage(imageBuffer, 0, 0, canvas.width, canvas.height);
}

function paintPlaceholder(): void {
  const styles = getComputedStyle(document.documentElement);
  ctx2d.fillStyle = styles.getPropertyValue("--color-background").trim() || "#ffffff";
  ctx2d.fillRect(0, 0, canvas.width, canvas.height);
  ctx2d.fillStyle = styles.getPropertyValue("--color-muted-foreground").trim() || "#6b7280";
  ctx2d.font = "14px sans-serif";
  ctx2d.textAlign = "center";
  ctx2d.fillText(t("frames"), canvas.width / 2, canvas.height / 2);
}

// ---------------------------------------------------------------- view scale

// Scale is purely local: it changes how large the remote desktop is painted
// on screen (with scrollbars when >100%), never the negotiated resolution.
function applyScale(): void {
  const scale = Number(scaleSelect.value) || 1;
  if (scale <= 1.001) {
    canvas.style.width = "100%";
    canvas.style.height = "100%";
  } else {
    canvas.style.width = `${Math.round(canvasWrap.clientWidth * scale)}px`;
    canvas.style.height = `${Math.round(canvasWrap.clientHeight * scale)}px`;
  }
}

scaleSelect.addEventListener("change", () => {
  applyScale();
  drawToScreen();
});

// ---------------------------------------------------------------- input

function toRemotePoint(event: MouseEvent): { x: number; y: number } | null {
  if (!remoteWidth || !remoteHeight) return null;
  const rect = canvas.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return null;
  const x = Math.round(((event.clientX - rect.left) / rect.width) * remoteWidth);
  const y = Math.round(((event.clientY - rect.top) / rect.height) * remoteHeight);
  return {
    x: Math.min(Math.max(x, 0), remoteWidth - 1),
    y: Math.min(Math.max(y, 0), remoteHeight - 1),
  };
}

function buttonFlags(event: MouseEvent): number {
  if (event.button === 2) return PTR_BUTTON2;
  if (event.button === 1) return PTR_BUTTON3;
  return PTR_BUTTON1;
}

function send(data: Uint8Array): void {
  if (!bridge || !connectionId) return;
  try {
    bridge.sendBinary(INPUT_CHANNEL, data);
  } catch (error) {
    console.warn("sendBinary failed", error);
  }
}

// ~60Hz cap with trailing-edge flush: the newest position always reaches the
// server even when events arrive faster than the cap.
let lastMoveAt = 0;
let pendingMove: { x: number; y: number } | null = null;
let moveTimer: number | undefined;
function throttledMove(x: number, y: number): void {
  const wait = 16 - (performance.now() - lastMoveAt);
  if (wait <= 0) {
    lastMoveAt = performance.now();
    pendingMove = null;
    send(mousePacket(connectionId, PTR_MOVE, x, y));
    return;
  }
  pendingMove = { x, y };
  if (moveTimer === undefined) {
    moveTimer = window.setTimeout(() => {
      moveTimer = undefined;
      if (pendingMove) {
        lastMoveAt = performance.now();
        send(mousePacket(connectionId, PTR_MOVE, pendingMove.x, pendingMove.y));
        pendingMove = null;
      }
    }, wait);
  }
}

canvas.addEventListener("pointerdown", (event) => {
  canvas.setPointerCapture(event.pointerId);
  imeInput.focus({ preventScroll: true });
  const point = toRemotePoint(event);
  if (!point) return;
  event.preventDefault();
  const button = buttonFlags(event);
  heldButtons.add(button);
  lastPoint = [point.x, point.y];
  send(mousePacket(connectionId, PTR_DOWN | button, point.x, point.y));
});

canvas.addEventListener("pointerup", (event) => {
  const point = toRemotePoint(event);
  if (!point) return;
  event.preventDefault();
  const button = buttonFlags(event);
  heldButtons.delete(button);
  send(mousePacket(connectionId, button, point.x, point.y));
});

canvas.addEventListener("pointermove", (event) => {
  const point = toRemotePoint(event);
  if (!point) return;
  lastPoint = [point.x, point.y];
  throttledMove(point.x, point.y);
});

canvas.addEventListener("contextmenu", (event) => event.preventDefault());

// Wheel coalescing state: momentum scrolling fires many small wheel events;
// sending each as its own RDP packet builds a backlog, so we accumulate the
// raw delta and emit one packet per animation frame at the last pointer
// position. Sign: browser deltaY is positive scrolling down, while the RDP
// wheel unit is positive scrolling up, so the accumulated delta is inverted.
let wheelAxis: boolean | null = null;
let wheelDelta = 0;
let wheelPos: [number, number] = [0, 0];
let wheelScheduled = false;
function flushWheel(): void {
  wheelScheduled = false;
  if (wheelDelta === 0 || wheelAxis === null) return;
  send(wheelPacket(connectionId, wheelAxis, -wheelDelta, wheelPos[0], wheelPos[1]));
  wheelDelta = 0;
}

canvas.addEventListener("wheel", (event) => {
  const point = toRemotePoint(event);
  if (!point) return;
  event.preventDefault();
  const horizontal = Math.abs(event.deltaX) > Math.abs(event.deltaY);
  const delta = horizontal ? event.deltaX : event.deltaY;
  const rotation = Math.round(delta);
  if (rotation === 0) return;
  if (wheelAxis === null) {
    wheelAxis = horizontal;
    wheelPos = [point.x, point.y];
  } else if (wheelAxis !== horizontal) {
    // Axis changed mid-burst: flush what we have first.
    flushWheel();
    wheelAxis = horizontal;
    wheelPos = [point.x, point.y];
  }
  wheelDelta = Math.max(-32768, Math.min(32767, wheelDelta + rotation));
  wheelPos = [point.x, point.y];
  if (!wheelScheduled) {
    wheelScheduled = true;
    requestAnimationFrame(flushWheel);
  }
}, { passive: false });

window.addEventListener("blur", () => {
  for (const button of heldButtons) {
    send(mousePacket(connectionId, button, lastPoint[0], lastPoint[1]));
  }
  heldButtons.clear();
  // Losing focus swallows the keyup events, so release every forwarded key
  // here. Otherwise a Ctrl pressed just before an Alt+Tab would stay down on
  // the remote and turn the next click into a Ctrl+click.
  releaseForwardedKeys();
});

let imeComposing = false;

// Physical keys we actually forwarded as a press. A release is only sent for
// these, so a key pressed before focus (or mid-IME) can never leave the remote
// stuck holding it down.
const forwardedKeys = new Set<string>();

function releaseForwardedKeys(): void {
  for (const code of forwardedKeys) {
    const mapped = mapKey(code);
    if (mapped) send(keyPacket(connectionId, mapped.flags | RELEASE_FLAG, mapped.scanCode));
  }
  forwardedKeys.clear();
}

function sendKey(event: KeyboardEvent, down: boolean): void {
  if (imeComposing) return;
  // IME composition arrives as Process; ignore it here.
  if (event.key === "Process") return;
  const mapped = mapKey(event.code);
  if (!mapped) return;

  if (!down) {
    if (!forwardedKeys.delete(event.code)) return;
    event.preventDefault();
    send(keyPacket(connectionId, mapped.flags | RELEASE_FLAG, mapped.scanCode));
    return;
  }

  // A printable character typed without a chord is delivered by the hidden
  // input as a Unicode keyboard event; sending the physical key too would
  // double it. Modifiers and chords (Ctrl+C / Ctrl+V / Ctrl+X / Alt+…) must
  // travel as physical keys: that is the only form the server turns into a
  // shortcut, and it is what the remote's own clipboard handling listens for.
  if (!isModifier(event.code) && event.key.length === 1 && !event.ctrlKey && !event.altKey && !event.metaKey) {
    return;
  }

  event.preventDefault();
  forwardedKeys.add(event.code);
  send(keyPacket(connectionId, mapped.flags, mapped.scanCode));
}

window.addEventListener("keydown", (event) => sendKey(event, true), { capture: true });
window.addEventListener("keyup", (event) => sendKey(event, false), { capture: true });

// Ctrl+C / Ctrl+V / Ctrl+X are forwarded to the remote as physical keys, so the
// browser must not *also* act on them locally. Without this the hidden IME
// field receives the local clipboard on paste and re-sends it as Unicode text
// on top of the remote's own paste — the text lands twice. Clipboard transfer
// itself is owned by the sidecar's CLIPRDR channel.
for (const type of ["copy", "cut", "paste"]) {
  window.addEventListener(type, (event) => event.preventDefault(), { capture: true });
}

// Chinese/Japanese IME: the hidden input collects composition; committed text
// goes out as RDP Unicode keyboard events (MS-RDPBCGR 2.2.8.1.2.4.2).
imeInput.addEventListener("compositionstart", () => { imeComposing = true; });
imeInput.addEventListener("compositionend", () => { imeComposing = false; });
imeInput.addEventListener("input", (event) => {
  // Belt and braces: a paste that somehow still reaches the field has already
  // been handled remotely.
  const source = (event as InputEvent).inputType;
  if (source === "insertFromPaste" || source === "insertFromDrop") return;
  const text = imeInput.value;
  imeInput.value = "";
  if (!text || !connectionId) return;
  for (const unit of text) {
    const code = unit.codePointAt(0) ?? 0;
    if (code > 0xffff) continue;
    send(unicodePacket(connectionId, 0, code));
    send(unicodePacket(connectionId, 0x8000, code));
  }
});

// Clipboard sync is fully automatic in the sidecar: remote copies land in the
// local clipboard and local copies are advertised to the server, so the UI has
// no manual copy/paste affordances.

// ---------------------------------------------------------------- status

function setStatus(state: string, message?: string): void {
  const showReconnect = state === "reconnecting" || state === "retry-failed" || state === "connection-lost";
  reconnectBar.hidden = !showReconnect;
  if (state === "connect-failed") {
    gotFrame = false;
    overlay.textContent = message ? `${t("connectFailed")}: ${message}` : t("connectFailed");
    return;
  }
  if (gotFrame) {
    overlay.textContent = "";
    return;
  }
  if (showReconnect) overlay.textContent = t("lost");
  else if (state === "connected") overlay.textContent = t("open");
  else if (state === "connecting") overlay.textContent = t("connecting");
  else if (state === "disconnected") overlay.textContent = t("noSession");
}

// ---------------------------------------------------------------- apply size

function canvasSize(): { width: number; height: number } {
  return {
    width: Math.min(8192, Math.max(256, Math.round(canvasWrap.clientWidth / 4) * 4)),
    height: Math.min(8192, Math.max(200, Math.round(canvasWrap.clientHeight / 4) * 4)),
  };
}

function targetSize(): { width: number; height: number } {
  const value = resSelect.value;
  if (!value || value === "fit") return canvasSize();
  const [width, height] = value.split("x").map(Number);
  if (!width || !height) return canvasSize();
  return { width, height };
}

// Only the Apply button changes the remote desktop size. There is deliberately
// no ResizeObserver auto-negotiation: servers without DisplayControl used to
// be re-dialed in a loop by it.
function sendApply(): void {
  if (!connectionId) return;
  const size = targetSize();
  setStatus("connecting");
  send(applySizePacket(connectionId, size.width, size.height));
}

applyBtn.addEventListener("click", sendApply);
retryBtn.addEventListener("click", sendApply);

// ---------------------------------------------------------------- bootstrap

function fillResolutionOptions(): void {
  const current = resSelect.value || "fit";
  resSelect.replaceChildren();
  const fit = document.createElement("option");
  fit.value = "fit";
  fit.textContent = t("fitWindow");
  resSelect.appendChild(fit);
  for (const value of RESOLUTIONS) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = value.replace("x", "×");
    resSelect.appendChild(option);
  }
  resSelect.value = current;
}

function applyEnv(): void {
  eyebrow.textContent = t("eyebrow");
  resLabel.textContent = t("res");
  scaleLabel.textContent = t("scale");
  applyBtn.textContent = t("apply");
  retryBtn.textContent = t("retry");
  fillResolutionOptions();
  if (!gotFrame) overlay.textContent = connectionId ? t("open") : t("noSession");
}

window.addEventListener("resize", () => {
  applyScale();
  drawToScreen();
});

async function boot(): Promise<void> {
  if (!bridge) {
    overlay.textContent = "no dbxPlugin bridge";
    return;
  }
  await bridge.ready;
  locale = bridge.locale ?? locale;
  applyEnv();
  paintPlaceholder();

  connectionId = String(bridge.context?.connectionId ?? "");
  if (!connectionId) {
    overlay.textContent = t("noSession");
    return;
  }
  setStatus("connecting");

  const offBinary = bridge.onBinary(applyFrame);
  const offContext = bridge.onContext((context) => {
    const next = String(context?.connectionId ?? "");
    if (next && next !== connectionId) {
      connectionId = next;
      gotFrame = false;
      setStatus("connecting");
    }
  });
  const offEvent = bridge.onEvent(({ method, params }) => {
    if (method !== "rdp/status" && method !== "rdp/desktop") return;
    const detail = (params ?? {}) as {
      connectionId?: string;
      state?: string;
      width?: number;
      height?: number;
      detail?: { attempt?: number; message?: string };
    };
    if (detail.connectionId && detail.connectionId !== connectionId) return;
    if (method === "rdp/status" && detail.state) {
      setStatus(detail.state, detail.detail?.message);
    }
  });

  window.addEventListener("dbx-plugin-env", () => {
    locale = bridge.locale ?? locale;
    applyEnv();
  });

  window.addEventListener("pagehide", () => {
    offBinary();
    offContext();
    offEvent();
  });
}

void boot();
