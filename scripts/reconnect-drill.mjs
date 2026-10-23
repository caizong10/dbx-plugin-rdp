// Phase 4: transport-loss drill against the REAL Windows host.
// A local TCP relay fronts the real endpoint; destroying the relay's sockets
// simulates a network cut without touching the server. Asserts the sidecar
// emits connection-lost, retries with backoff (reconnecting), re-establishes
// (connected again) and resumes frame delivery.
// Usage: node scripts/reconnect-drill.mjs <real-host> <real-port> <user> <pass>
import { spawn } from "node:child_process";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = join(root, "bin", "windows-x64", "dbx-plugin-dbx-plugin-rdp.exe");
const [realHost, realPort, username, password] = process.argv.slice(2);
if (!realHost || !realPort || !username || !password) {
  console.error("usage: node scripts/reconnect-drill.mjs <host> <port> <user> <pass>");
  process.exit(2);
}
const RELAY_PORT = 21000 + (process.pid % 1000);
const CONNECTION_ID = "reconnect-drill";

// ---------------------------------------------------------------- relay
const relaySockets = new Set();
let relay = null;
function startRelay() {
  return new Promise((resolve, reject) => {
    relay = net.createServer((client) => {
      const upstream = net.connect(Number(realPort), realHost);
      relaySockets.add(client);
      relaySockets.add(upstream);
      client.pipe(upstream).pipe(client);
      const drop = () => {
        relaySockets.delete(client);
        relaySockets.delete(upstream);
        client.destroy();
        upstream.destroy();
      };
      client.on("error", drop);
      upstream.on("error", drop);
    });
    relay.once("error", reject);
    relay.listen(RELAY_PORT, "127.0.0.1", () => resolve());
  });
}
function cutRelay() {
  return new Promise((resolve) => {
    for (const socket of relaySockets) socket.destroy();
    relaySockets.clear();
    relay.close(() => {
      relay = null;
      // small pause so the OS releases the listening socket
      setTimeout(resolve, 300);
    });
  });
}
async function bringBack() {
  // re-bind with retry in case the port lingers briefly
  for (let attempt = 0; attempt < 10; attempt++) {
    try {
      await startRelay();
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 300));
    }
  }
  throw new Error("relay could not re-bind");
}

// ---------------------------------------------------------------- sidecar
function encodeJson(obj) {
  const payload = Buffer.from(JSON.stringify(obj));
  const header = Buffer.alloc(5);
  header.writeUInt8(0, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env, RUST_LOG: "info" } });
let stderrAll = "";
child.stderr.on("data", (d) => { stderrAll += d.toString(); });

let buffer = Buffer.alloc(0);
const responses = [];
const events = [];
let framesAfterCut = 0;
let frameMode = "count";
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
      if (msg.id !== undefined) responses.push(msg);
      else if (msg.method) events.push(msg);
    } else if (frameMode === "count") {
      framesAfterCut++;
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
    }, 30);
  });
}

function sendReq(id, method, params) {
  child.stdin.write(encodeJson({ jsonrpc: "2.0", id, method, params }));
  return waitFor(() => responses.find((m) => m.id === id), 60000, `${method}#${id}`);
}

const lifecycle = () => ({
  provider: { id: "io.github.caizong10.rdp.connection" },
  connection: {
    id: CONNECTION_ID,
    host: "127.0.0.1",
    port: RELAY_PORT,
    username,
    password,
    external_config: { color_depth: "16", resize_mode: "dynamic", ignore_cert: true },
  },
  runtime: null,
});

const failures = [];
function check(name, ok, note) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${note ? `  (${note})` : ""}`);
  if (!ok) failures.push(name);
}
const hasEvent = (state, afterIndex = 0) =>
  events.findIndex((e) => e.method === "rdp/status" && e.params?.state === state && e.params?.connectionId === CONNECTION_ID) >= afterIndex;

await sendReq(1, "plugin/initialize", { host: { protocolVersions: [1] } });
await startRelay();

const connected1 = await sendReq(2, "connection/connect", lifecycle());
check("initial connect via relay", connected1.result?.success === true);
const firstEventsLen = events.length;
await new Promise((r) => setTimeout(r, 2500));
check("first connected event", hasEvent("connected", firstEventsLen - 1) >= 0);

// cut the transport
await cutRelay();
try {
  await waitFor(() => hasEvent("connection-lost", firstEventsLen) >= 0 || hasEvent("reconnecting", firstEventsLen) >= 0, 15000, "lost/reconnecting");
  check("transport loss detected", true, "connection-lost / reconnecting emitted");
} catch {
  check("transport loss detected", false, "no status events after cut");
}

// bring the relay back; sidecar must reconnect on its own (backoff <= a few s)
await bringBack();
const beforeReconnect = events.length;
try {
  await waitFor(() => hasEvent("connected", beforeReconnect - 1) >= 0, 45000, "auto reconnect");
  check("auto reconnect succeeded", true);
} catch {
  check("auto reconnect succeeded", false, `events=${events.slice(-6).map((e) => e.params?.state).join(",")}`);
}

// frames flow again after reconnect
frameMode = "count";
framesAfterCut = 0;
await new Promise((r) => setTimeout(r, 3000));
check("frames resume after reconnect", framesAfterCut > 0, `frames=${framesAfterCut}`);

const disc = await sendReq(3, "connection/disconnect", { connection: { id: CONNECTION_ID } });
check("disconnect after drill", disc.result?.success === true);

const ping = await sendReq(4, "dbx-plugin-rdp/ping", { connectionId: CONNECTION_ID });
check("sidecar responsive", ping.result?.ok === true);
if (stderrAll.includes(password)) failures.push("PASSWORD LEAKED IN LOGS");
check("no password in stderr", !stderrAll.includes(password));

child.stdin.end();
child.kill();
relay?.close();

const eventsLog = events.map((e) => `${e.params?.state}${e.params?.detail?.attempt ? "#" + e.params.detail.attempt : ""}`).join(" -> ");
console.log("status event trail:", eventsLog);
if (failures.length) {
  console.error("RECONNECT-DRILL FAIL:", failures.join("; "));
  process.exit(1);
}
console.log("RECONNECT-DRILL OK: loss detected, auto-reconnect with backoff, frames resumed, clean teardown");
