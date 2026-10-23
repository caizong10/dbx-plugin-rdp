// Phase 3 real-machine acceptance for the frame pipeline (no UI, protocol only):
//   1. connect to the real Windows host through the sidecar (real credentials)
//   2. wait for desktop frames on the binary "frame" channel; validate layout
//      (magic RDP1, connectionId, tile rect, seq, RGBA payload size)
//   3. assert pixels are not uniformly black (the server is actually painting)
//   4. inject mouse move + wheel packets on channel "input" (must be absorbed
//      without crashing the sidecar)
//   5. send a resize packet (dynamic mode) and wait for a new desktop size via
//      rdp/desktop event or changed frame dimensions
//   6. disconnect; sidecar must stay responsive and session must drop
// Usage: node scripts/frame-smoke-real.mjs <host> <port> <user> <pass>
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = join(root, "bin", "windows-x64", "dbx-plugin-dbx-plugin-rdp.exe");
const [host, port, username, password] = process.argv.slice(2);
if (!host || !port || !username || !password) {
  console.error("usage: node scripts/frame-smoke-real.mjs <host> <port> <user> <pass>");
  process.exit(2);
}
const CONNECTION_ID = "frame-smoke-1";

function encodeJson(obj) {
  const payload = Buffer.from(JSON.stringify(obj));
  const header = Buffer.alloc(5);
  header.writeUInt8(0, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

function encodeBinary(channel, data) {
  const chan = Buffer.from(channel);
  const payload = Buffer.alloc(2 + chan.length + data.length);
  payload.writeUInt16BE(chan.length, 0);
  chan.copy(payload, 2);
  data.copy(payload, 2 + chan.length);
  const header = Buffer.alloc(5);
  header.writeUInt8(1, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

// input packet: connIdLen | id | kind | payload (big endian)
function inputPacket(kind, bodyBytes) {
  const id = Buffer.from(CONNECTION_ID);
  const body = Buffer.alloc(1 + id.length + 1 + bodyBytes.length);
  body[0] = id.length;
  id.copy(body, 1);
  body[1 + id.length] = kind;
  bodyBytes.copy(body, 2 + id.length);
  return body;
}

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env, RUST_LOG: "info" } });
let stderrAll = "";
child.stderr.on("data", (d) => { stderrAll += d.toString(); });

let buffer = Buffer.alloc(0);
const responses = [];
const events = [];
const frames = [];
child.stdout.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    if (buffer.length < 5) return;
    const kind = buffer.readUInt8(0);
    const length = buffer.readUInt32BE(1);
    if (buffer.length < 5 + length) return;
    const payload = buffer.subarray(5, 5 + length);
    buffer = buffer.subarray(5 + length);
    if (kind === 0) {
      const msg = JSON.parse(payload.toString());
      if (msg.method && msg.id === undefined) events.push(msg);
      else responses.push(msg);
    } else {
      const chanLen = payload.readUInt16BE(0);
      const channel = payload.subarray(2, 2 + chanLen).toString();
      frames.push({ channel, data: payload.subarray(2 + chanLen) });
    }
  }
});

function waitFor(pred, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const tick = setInterval(() => {
      const found = pred();
      if (found) { clearInterval(tick); resolve(found); }
      else if (Date.now() > deadline) { clearInterval(tick); reject(new Error(`timeout waiting for ${label}`)); }
    }, 40);
  });
}

const sendReq = (id, method, params) => {
  child.stdin.write(encodeJson({ jsonrpc: "2.0", id, method, params }));
  return waitFor(() => responses.find((m) => m.id === id), 60000, `${method} reply`);
};

const failures = [];
function check(name, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures.push(name);
}

const params = () => ({
  provider: { id: "io.github.caizong10.rdp.connection" },
  connection: {
    id: CONNECTION_ID,
    host, port: Number(port), username, password,
    external_config: { color_depth: "32", resize_mode: "dynamic", ignore_cert: true },
  },
  runtime: null,
});

await sendReq(1, "plugin/initialize", { host: { protocolVersions: [1] } });
const connect = await sendReq(2, "connection/connect", params());
check("real connect", connect.result?.success === true, JSON.stringify(connect.result ?? connect.error).slice(0, 160));

let firstFrame = null;
try {
  firstFrame = await waitFor(() => frames.find((f) => f.channel === "frame"), 20000, "first frame");
} catch { /* handled below */ }

function parseFrame(data) {
  const magic = data.subarray(0, 4).toString();
  const idLen = data[4];
  const connId = data.subarray(5, 5 + idLen).toString();
  let at = 5 + idLen;
  const dw = data.readUInt16BE(at); at += 2;
  const dh = data.readUInt16BE(at); at += 2;
  const format = data[at++];
  const x = data.readUInt16BE(at); at += 2;
  const y = data.readUInt16BE(at); at += 2;
  const w = data.readUInt16BE(at); at += 2;
  const h = data.readUInt16BE(at); at += 2;
  const seq = data.readUInt32BE(at); at += 4;
  return { magic, connId, dw, dh, format, x, y, w, h, seq, pixels: data.subarray(at) };
}

if (!firstFrame) {
  check("frame received", false, "no binary frame within 20s");
} else {
  const head = parseFrame(firstFrame.data);
  check("frame magic/id", head.magic === "RDP1" && head.connId === CONNECTION_ID, `magic=${head.magic} id=${head.connId}`);
  check("frame geometry", head.dw > 0 && head.dh > 0 && head.format === 0, `${head.dw}x${head.dh} fmt=${head.format} seq=${head.seq}`);
  check("frame pixels sized", head.pixels.length === head.w * head.h * 4, `payload=${head.pixels.length} rect=${head.w}x${head.h}`);
}

// input injection: mouse move + wheel (sidecar must stay alive; server may react)
{
  const move = Buffer.alloc(6);
  move.writeUInt16BE(0x0800, 0); // PTR_FLAGS_MOVE
  move.writeUInt16BE(640, 2);
  move.writeUInt16BE(400, 4);
  child.stdin.write(encodeBinary("input", inputPacket(0, move)));
  const wheel = Buffer.alloc(7);
  wheel[0] = 0; // vertical
  wheel.writeInt16BE(-120, 1);
  wheel.writeUInt16BE(640, 3);
  wheel.writeUInt16BE(400, 5);
  child.stdin.write(encodeBinary("input", inputPacket(2, wheel)));
}
const ping = await sendReq(3, "dbx-plugin-rdp/ping", { connectionId: CONNECTION_ID });
check("sidecar alive after input", ping.result?.ok === true);

// dynamic resize request
{
  const r = Buffer.alloc(4);
  r.writeUInt16BE(1024, 0);
  r.writeUInt16BE(768, 2);
  child.stdin.write(encodeBinary("input", inputPacket(3, r)));
}
let resized = null;
try {
  resized = await waitFor(() => {
    const ev = events.find((e) => e.method === "rdp/desktop" && e.params?.resizeAck === true);
    if (ev) return ev.params;
    const latest = frames.filter((f) => f.channel === "frame").at(-1);
    if (latest) {
      const h = parseFrame(latest.data);
      if (h.dw === 1024 && h.dh === 768) return { width: 1024, height: 768 };
    }
    return null;
  }, 25000, "resize ack");
} catch { /* server may not support display control */ }
check("dynamic resize applied", !!resized, resized ? JSON.stringify(resized) : "server did not resize (check DisplayControl support)");

// sample the painting for 3s after the reactivation repaint
await new Promise((resolve) => setTimeout(resolve, 3000));
{
  const totalTiles = frames.filter((f) => f.channel === "frame").length;
  let nonzero = 0;
  const uniq = new Set();
  for (const f of frames.filter((f) => f.channel === "frame")) {
    const px = parseFrame(f.data).pixels;
    for (let i = 0; i + 2 < px.length; i += 4) {
      if (px[i] || px[i + 1] || px[i + 2]) nonzero++;
    }
    if (px.length >= 3) uniq.add(px[0] << 16 | px[1] << 8 | px[2]);
  }
  check("tiles all correctly sized", frames.filter((f) => f.channel === "frame").every((f) => {
    const h = parseFrame(f.data);
    return h.pixels.length === h.w * h.h * 4;
  }), `frames=${totalTiles}`);
  check("screen is painting", nonzero > 1000 && uniq.size > 3, `nonzero=${nonzero} distinctColors=${uniq.size} tiles=${totalTiles}`);
}

// disconnect + session drop
const disc = await sendReq(4, "connection/disconnect", { connection: { id: CONNECTION_ID } });
check("disconnect ok", disc.result?.success === true);
const ping2 = await sendReq(5, "dbx-plugin-rdp/ping", { connectionId: CONNECTION_ID });
check("sidecar responsive after disconnect", ping2.result?.ok === true);
if (stderrAll.includes(password)) failures.push("PASSWORD LEAKED IN LOGS");
check("no password in stderr", !stderrAll.includes(password));

console.log("--- stderr tail (negotiation proof) ---");
console.log(stderrAll.split(/\r?\n/).filter(Boolean).slice(-6).join("\n").replace(/\u001b\[[0-9;]*m/g, ""));

child.stdin.end();
child.kill();
if (failures.length) {
  console.error("FRAME-SMOKE FAIL:", failures.join("; "));
  process.exit(1);
}
console.log("FRAME-SMOKE OK: real desktop frames rendered, input absorbed, resize handled, clean disconnect");
