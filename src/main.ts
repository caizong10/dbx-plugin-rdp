import type { BinaryEnvelope, DbxPluginBridge } from "./dbx";
import { AutoSizeDecision } from "./auto-size";
import { RELEASE_FLAG, mapKey } from "./keymap";
import { fitLayout, fitResolution, remotePoint } from "./layout";
import {
  INPUT_CHANNEL,
  PTR_BUTTON1,
  PTR_BUTTON2,
  PTR_BUTTON3,
  PTR_DOWN,
  PTR_MOVE,
  acceptFrame,
  applySizePacket,
  initialSizePacket,
  keyPacket,
  mousePacket,
  pullFilesPacket,
  retryPacket,
  sessionReadyPacket,
  wheelPacket,
} from "./protocol";

const bridge = window.dbxPlugin as DbxPluginBridge | undefined;

const eyebrow = document.querySelector<HTMLSpanElement>("#eyebrow")!;
const canvasWrap = document.querySelector<HTMLElement>(".canvas-wrap")!;
const canvas = document.querySelector<HTMLCanvasElement>("#screen")!;
const overlay = document.querySelector<HTMLDivElement>("#overlay")!;
const reconnectBar = document.querySelector<HTMLDivElement>("#reconnect")!;
const reconnectNote = document.querySelector<HTMLSpanElement>(".reconnect-note")!;
const clipToast = document.querySelector<HTMLDivElement>("#clip-toast")!;
const pullFilesBtn = document.querySelector<HTMLButtonElement>("#pull-files")!;
const retryBtn = document.querySelector<HTMLButtonElement>("#retry")!;
const resSelect = document.querySelector<HTMLSelectElement>("#resolution")!;
const resLabel = document.querySelector<HTMLSpanElement>("#res-label")!;
const scaleSelect = document.querySelector<HTMLSelectElement>("#scale")!;
const scaleLabel = document.querySelector<HTMLSpanElement>("#scale-label")!;
const applyBtn = document.querySelector<HTMLButtonElement>("#apply")!;

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
    connected: "Connected",
    resizing: "Adjusting remote resolution…",
    resizeFailed: "Resolution change failed",
    resizeUnavailable: "The remote session did not accept the requested size",
    open: "Opening remote desktop…",
    frames: "Receiving screen…",
    noSession: "This workbench has no connection. Open it from a connection.",
    connectFailed: "Connection failed",
    clipStart: "Receiving {files} file(s){size}…",
    clipDone: "{files} file(s) ready — paste anywhere",
    clipFail: "File transfer failed: {reason}",
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
    connected: "已连接",
    resizing: "正在调整远程分辨率…",
    resizeFailed: "调整分辨率失败",
    resizeUnavailable: "远程会话未接受请求的尺寸",
    open: "正在打开远程桌面…",
    frames: "正在接收画面…",
    noSession: "此工作台没有连接，请从连接项打开。",
    connectFailed: "连接失败",
    clipStart: "正在接收 {files} 个文件{size}…",
    clipDone: "{files} 个文件已就绪，可任意粘贴",
    clipFail: "文件传输失败：{reason}",
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
let frameGeneration = 0;

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
  if (frame.generation < frameGeneration) return;
  if (frame.generation > frameGeneration) {
    frameGeneration = frame.generation;
    gotFrame = false;
    imageCtx.clearRect(0, 0, imageBuffer.width, imageBuffer.height);
    ctx2d.clearRect(0, 0, canvas.width, canvas.height);
  }
  if (frame.desktopWidth !== remoteWidth || frame.desktopHeight !== remoteHeight) {
    remoteWidth = frame.desktopWidth;
    remoteHeight = frame.desktopHeight;
    imageBuffer.width = remoteWidth;
    imageBuffer.height = remoteHeight;
    canvas.width = remoteWidth;
    canvas.height = remoteHeight;
    applyScale();
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
    scheduleAutoSize();
  }
  updateDiagnostics();
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
  ctx2d.imageSmoothingEnabled = false;
  ctx2d.drawImage(imageBuffer, x, y, width, height, x, y, width, height);
}

function drawToScreen(): void {
  ctx2d.imageSmoothingEnabled = false;
  ctx2d.clearRect(0, 0, canvas.width, canvas.height);
  ctx2d.drawImage(imageBuffer, 0, 0, canvas.width, canvas.height);
}

function paintPlaceholder(): void {
  const styles = getComputedStyle(document.documentElement);
  ctx2d.fillStyle = styles.getPropertyValue("--color-background").trim() || "#ffffff";
  ctx2d.fillRect(0, 0, canvas.width, canvas.height);
}

// ---------------------------------------------------------------- view scale

// Scale is local and capped at fit so the whole desktop remains visible.
const scaleNotice = document.createElement("span");
scaleNotice.className = "scale-notice";
scaleNotice.setAttribute("role", "status");
scaleNotice.hidden = true;
scaleSelect.parentElement!.after(scaleNotice);
function applyScale(): void {
  const layout = fitLayout(
    { width: canvasWrap.clientWidth, height: canvasWrap.clientHeight },
    { width: remoteWidth || canvas.width, height: remoteHeight || canvas.height },
    Number(scaleSelect.value),
  );
  if (!layout) return;
  canvas.style.width = `${layout.width}px`;
  canvas.style.height = `${layout.height}px`;
  scaleNotice.hidden = !layout.limited;
  scaleNotice.textContent = lang() === "zh" ? "放大受限：为完整显示已适应窗口" : "Zoom limited to fit the entire desktop";
}

scaleSelect.addEventListener("change", applyScale);
window.addEventListener("dbx-plugin-env", applyScale);
new ResizeObserver(() => {
  applyScale();
  scheduleAutoSize();
  updateDiagnostics();
}).observe(canvasWrap);

// ---------------------------------------------------------------- input

function toRemotePoint(event: MouseEvent): { x: number; y: number } | null {
  if (!remoteWidth || !remoteHeight) return null;
  return remotePoint(event.clientX, event.clientY, canvas.getBoundingClientRect(), { width: remoteWidth, height: remoteHeight });
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

// Manual "pull remote files": re-arms the sidecar's deferred offer and
// restarts its background pull — the folder-copy escape hatch.
pullFilesBtn.addEventListener("click", () => {
  showClipboardToast(lang() === "zh" ? "正在重新拉取远端剪贴板…" : "Re-pulling remote clipboard…");
  send(pullFilesPacket(connectionId));
});

// ~60Hz cap with trailing-edge flush: the newest position always reaches the
// server even when events arrive faster than the cap.
let lastMoveAt = 0;
let pendingMove: { x: number; y: number } | null = null;
let moveTimer: number | undefined;
function flushPendingMove(): void {
  if (moveTimer !== undefined) window.clearTimeout(moveTimer);
  moveTimer = undefined;
  if (pendingMove) {
    send(mousePacket(connectionId, PTR_MOVE, pendingMove.x, pendingMove.y));
    pendingMove = null;
    lastMoveAt = performance.now();
  }
}

function cancelPendingMove(): void {
  if (moveTimer !== undefined) window.clearTimeout(moveTimer);
  moveTimer = undefined;
  pendingMove = null;
}

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

function releaseRemoteButtons(): void {
  cancelPendingMove();
  for (const button of heldButtons) {
    send(mousePacket(connectionId, button, lastPoint[0], lastPoint[1]));
  }
  heldButtons.clear();
}

function releaseRemoteInputState(): void {
  releaseRemoteButtons();
  releaseForwardedKeys();
}

canvas.addEventListener("focus", () => {
  canvas.classList.add("remote-focus");
});
canvas.addEventListener("blur", () => {
  canvas.classList.remove("remote-focus");
  releaseRemoteInputState();
});

canvas.addEventListener("pointerdown", (event) => {
  canvas.focus({ preventScroll: true });
  canvas.setPointerCapture(event.pointerId);
  flushPendingMove();
  const point = toRemotePoint(event);
  if (!point) return;
  event.preventDefault();
  const button = buttonFlags(event);
  heldButtons.add(button);
  lastPoint = [point.x, point.y];
  send(mousePacket(connectionId, PTR_DOWN | button, point.x, point.y));
});

function releasePointer(event: PointerEvent): void {
  flushPendingMove();
  const point = toRemotePoint(event) ?? { x: lastPoint[0], y: lastPoint[1] };
  event.preventDefault();
  const button = buttonFlags(event);
  heldButtons.delete(button);
  send(mousePacket(connectionId, button, point.x, point.y));
  if (canvas.hasPointerCapture(event.pointerId)) canvas.releasePointerCapture(event.pointerId);
}

canvas.addEventListener("pointerup", releasePointer);
canvas.addEventListener("pointercancel", releasePointer);
canvas.addEventListener("lostpointercapture", releaseRemoteButtons);

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

window.addEventListener("blur", releaseRemoteInputState);

// Physical keys we actually forwarded as a press. A release is only sent for
// these, so a key pressed before focus can never leave the remote stuck holding
// it down.
const forwardedKeys = new Set<string>();

function releaseForwardedKeys(): void {
  for (const code of forwardedKeys) {
    const mapped = mapKey(code);
    if (mapped) send(keyPacket(connectionId, mapped.flags | RELEASE_FLAG, mapped.scanCode));
  }
  forwardedKeys.clear();
}

function sendKey(event: KeyboardEvent, down: boolean): void {
  const mapped = mapKey(event.code);
  if (!mapped) return;

  if (!down) {
    if (!forwardedKeys.delete(event.code)) return;
    event.preventDefault();
    send(keyPacket(connectionId, mapped.flags | RELEASE_FLAG, mapped.scanCode));
    return;
  }

  if (document.activeElement !== canvas) return;
  // The canvas has no editable local proxy. Physical keys go to the remote
  // keyboard/layout, so the remote IME owns text input.
  event.preventDefault();
  forwardedKeys.add(event.code);
  send(keyPacket(connectionId, mapped.flags, mapped.scanCode));
}

window.addEventListener("keydown", (event) => sendKey(event, true), { capture: true });
window.addEventListener("keyup", (event) => sendKey(event, false), { capture: true });

// Clipboard shortcuts are local unless the remote canvas owns focus. DBX's
// native clipboard channel remains responsible for actual transfer.
for (const type of ["copy", "cut", "paste"]) {
  window.addEventListener(type, (event) => {
    if (document.activeElement === canvas) event.preventDefault();
  }, { capture: true });
}

// Clipboard sync is fully automatic in the sidecar: remote copies land in the
// local clipboard and local copies are advertised to the server, so the UI has
// no manual copy/paste affordances.

// ---------------------------------------------------------------- status

// Clipboard transfer toast: shows while a remote file list downloads, then
// fades. Reuses the reconnect-bar styling vocabulary.
let clipToastTimer: number | undefined;
function showClipboardToast(message: string): void {
  clipToast.textContent = message;
  clipToast.hidden = false;
  if (clipToastTimer !== undefined) window.clearTimeout(clipToastTimer);
  clipToastTimer = window.setTimeout(() => {
    clipToast.hidden = true;
    clipToastTimer = undefined;
  }, 4000);
}

let currentStatus: { state: string; message?: string; attempt?: number } = { state: "disconnected" };

function setStatus(state: string, message?: string, attempt?: number): void {
  currentStatus = { state, message, attempt };
  if (state === "resizing" || state === "resize-failed") {
    reconnectBar.hidden = true;
    overlay.textContent = state === "resizing" ? t("resizing")
      : message ? `${t("resizeFailed")}: ${message}` : t("resizeFailed");
    return;
  }
  const showReconnect = state === "reconnecting" || state === "retry-failed" || state === "retry-exhausted" || state === "connection-lost";
  reconnectBar.hidden = !showReconnect;
  if (showReconnect) {
    reconnectNote.textContent = (state === "retry-failed" || state === "retry-exhausted") && attempt
      ? `${t("lost")} (${attempt})`
      : t("lost");
    overlay.textContent = "";
    return;
  }
  if (state === "connect-failed") {
    gotFrame = false;
    reconnectNote.textContent = t("connectFailed");
    overlay.textContent = message ? `${t("connectFailed")}: ${message}` : t("connectFailed");
    return;
  }
  if (gotFrame) {
    overlay.textContent = "";
    return;
  }
  overlay.textContent = state === "connected" ? t("connected")
    : state === "connecting" ? t("connecting")
    : state === "disconnected" ? t("noSession") : "";
}

// ---------------------------------------------------------------- apply size

function canvasSize(): { width: number; height: number } {
  return fitResolution({ width: canvasWrap.clientWidth, height: canvasWrap.clientHeight }, window.devicePixelRatio);
}

function targetSize(): { width: number; height: number } {
  const value = resSelect.value;
  if (!value || value === "fit") return canvasSize();
  const [width, height] = value.split("x").map(Number);
  if (!width || !height) return canvasSize();
  return { width, height };
}

// No repeated resize on later layout changes or framebuffer generations.
let autoDecision = new AutoSizeDecision();
let autoSessionId = "";
let autoTimer: number | undefined;
let autoEpoch = 0;
let autoState = "waiting";
const diagnostics = document.createElement("span");
diagnostics.className = "resize-diagnostics";
diagnostics.setAttribute("role", "status");
scaleNotice.after(diagnostics);

function updateDiagnostics(): void {
  const size = remoteWidth && remoteHeight ? `${remoteWidth}×${remoteHeight}` : "—";
  const dpr = Number.isFinite(window.devicePixelRatio) ? window.devicePixelRatio.toFixed(2) : "1.00";
  const label = lang() === "zh" ? "远程" : "Remote";
  const states: Record<string, string> = lang() === "zh"
    ? { waiting: "等待就绪", fixed: "固定尺寸", requested: "已申请", same: "尺寸匹配", unavailable: "自动协商不可用", manual: "手动设置" }
    : { waiting: "waiting for readiness", fixed: "fixed size", requested: "requested", same: "size matches", unavailable: "automatic sizing unavailable", manual: "manual sizing" };
  diagnostics.textContent = `${label}: ${size} · DPR ${dpr} · ${states[autoState] ?? states.waiting}`;
}

function scheduleAutoSize(): void {
  if (autoTimer !== undefined) window.clearTimeout(autoTimer);
  const epoch = autoEpoch;
  autoTimer = window.setTimeout(() => {
    autoTimer = undefined;
    if (epoch !== autoEpoch || !gotFrame || !autoSessionId) return;
    autoDecision.display();
    const size = autoDecision.decide({ width: canvasWrap.clientWidth, height: canvasWrap.clientHeight }, window.devicePixelRatio, { width: remoteWidth, height: remoteHeight });
    if (!size) return;
    autoState = "requested";
    updateDiagnostics();
    send(initialSizePacket(connectionId, size.width, size.height, autoSessionId));
  }, 300);
}

function sendApply(): void {
  if (!connectionId) return;
  autoDecision.userApplied();
  autoState = "manual";
  updateDiagnostics();
  const size = targetSize();
  if (size.width !== remoteWidth || size.height !== remoteHeight) setStatus("resizing");
  send(applySizePacket(connectionId, size.width, size.height));
}

applyBtn.addEventListener("click", sendApply);
retryBtn.addEventListener("click", () => {
  if (connectionId) send(retryPacket(connectionId));
});

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
  setStatus(currentStatus.state, currentStatus.message, currentStatus.attempt);
  updateDiagnostics();
}

let resizeFrame = 0;
window.addEventListener("resize", () => {
  if (resizeFrame) return;
  resizeFrame = requestAnimationFrame(() => {
    resizeFrame = 0;
    applyScale();
    drawToScreen();
  });
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
      releaseRemoteInputState();
      wheelDelta = 0;
      wheelAxis = null;
      connectionId = next;
      frameGeneration = 0;
      remoteWidth = 0;
      remoteHeight = 0;
      gotFrame = false;
      autoEpoch++;
      autoDecision = new AutoSizeDecision();
      autoSessionId = "";
      autoState = "waiting";
      updateDiagnostics();
      setStatus("connecting");
      send(sessionReadyPacket(connectionId));
    }
  });
  const offEvent = bridge.onEvent(({ method, params }) => {
    if (method !== "rdp/status" && method !== "rdp/desktop" && method !== "rdp/clipboard") return;
    const detail = (params ?? {}) as {
      connectionId?: string;
      state?: string;
      width?: number;
      height?: number;
      generation?: number;
      resizeAck?: boolean;
      detail?: { attempt?: number; message?: string; generation?: number; resizeMode?: string; sessionId?: string; accepted?: boolean; reason?: string };
      direction?: string;
    };
    if (detail.connectionId && detail.connectionId !== connectionId) return;
    if (method === "rdp/status" && ["session-ready", "session-unavailable", "initial-resize-result"].includes(detail.state ?? "") && detail.connectionId !== connectionId) return;
    if (method === "rdp/status" && detail.state) {
      if (!["session-ready", "session-unavailable", "initial-resize-result"].includes(detail.state)) {
        setStatus(detail.state, detail.detail?.message, detail.detail?.attempt);
      }
      if (detail.state === "session-ready" && detail.detail?.sessionId &&
          (detail.detail.resizeMode === "dynamic" || detail.detail.resizeMode === "fixed")) {
        if (autoSessionId !== detail.detail.sessionId) {
          // Preserve an early manual apply and any first frame on initial ready.
          // A later session reusing this connection id must wait for a new frame.
          if (autoSessionId) {
            autoEpoch++;
            gotFrame = false;
            frameGeneration = 0;
            autoDecision = new AutoSizeDecision();
          }
          autoSessionId = detail.detail.sessionId;
          autoDecision.sessionReady(detail.detail.resizeMode);
          if (autoState !== "manual") autoState = detail.detail.resizeMode === "fixed" ? "fixed" : "waiting";
          updateDiagnostics();
          if (detail.detail.resizeMode === "dynamic" && gotFrame) scheduleAutoSize();
        }
      } else if (detail.state === "session-unavailable") {
        autoState = "unavailable";
        updateDiagnostics();
      } else if (detail.state === "initial-resize-result" && detail.detail?.sessionId === autoSessionId && autoState !== "manual") {
        autoState = detail.detail.accepted ? "requested" : "unavailable";
        updateDiagnostics();
      }
    }
    if (method === "rdp/desktop") {
      if (typeof detail.generation === "number" && detail.generation >= frameGeneration) {
        if (detail.generation > frameGeneration) {
          frameGeneration = detail.generation;
          gotFrame = false;
          imageCtx.clearRect(0, 0, imageBuffer.width, imageBuffer.height);
        }
      }
      if (detail.resizeAck) setStatus("connected");
    }
    if (method === "rdp/clipboard") {
      const payload = (params ?? {}) as {
        state?: string;
        direction?: string;
        detail?: { files?: number; bytes?: number; reason?: string };
      };
      const d = payload.detail ?? {};
      const mb = d.bytes ? ` (${(d.bytes / 1048576).toFixed(1)} MB)` : "";
      if (payload.state === "started") {
        showClipboardToast(t("clipStart", { files: d.files ?? 0, size: mb.trim() }));
        // A remote copy landed: surface the manual pull button so a folder
        // whose server render was late can be fetched with one click.
        pullFilesBtn.hidden = false;
      } else if (payload.state === "finished") {
        showClipboardToast(t("clipDone", { files: d.files ?? 0 }));
        pullFilesBtn.hidden = true;
      } else if (payload.state === "failed") {
        showClipboardToast(t("clipFail", { reason: d.reason ?? "" }));
        pullFilesBtn.hidden = false;
      }
    }
  });

  // Subscribe before querying: session-ready can otherwise race registration.
  send(sessionReadyPacket(connectionId));

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
