// Phase 2 negative-path acceptance for the REAL IronRDP engine (no fake flag).
//  A) connect to 127.0.0.1:1 (nothing listening)  -> TCP refused, graceful.
//  B) connect to a dummy TCP listener that immediately hangs up -> RDP
//     sequence fails on the X.224 response, graceful.
// In both cases the sidecar must stay alive, answer with success:false, never
// leak the password into the response, and log the failure to stderr.
import { spawn } from "node:child_process";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = process.argv[2] ?? join(root, "bin", "windows-x64", "dbx-plugin-dbx-plugin-rdp.exe");
// Generated per run so the "never leaked" assertions stay meaningful without
// any committed credential.
const PASSWORD = `probe-${Math.random().toString(36).slice(2)}-${Date.now().toString(36)}`;

function encodeFrame(kind, payload) {
  const header = Buffer.alloc(5);
  header.writeUInt8(kind, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "pipe"] });
let stderrAll = "";
child.stderr.on("data", (d) => { stderrAll += d.toString(); });

let buffer = Buffer.alloc(0);
const jsonMessages = [];
child.stdout.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    if (buffer.length < 5) return;
    const kind = buffer.readUInt8(0);
    const length = buffer.readUInt32BE(1);
    if (buffer.length < 5 + length) return;
    const payload = buffer.subarray(5, 5 + length);
    buffer = buffer.subarray(5 + length);
    if (kind === 0) jsonMessages.push(JSON.parse(payload.toString()));
  }
});

function frameFor(id, timeoutMs) {
  return new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const tick = setInterval(() => {
      const msg = jsonMessages.find((m) => m.id === id);
      if (msg) { clearInterval(tick); resolve(msg); }
      else if (Date.now() > deadline) { clearInterval(tick); reject(new Error(`timeout waiting for id ${id}`)); }
    }, 25);
  });
}

let nextId = 1;
function send(method, params) {
  const id = nextId++;
  child.stdin.write(encodeFrame(0, Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, method, params }))));
  return frameFor(id, 45000);
}

function connectParams(host, port) {
  return {
    provider: { id: "io.github.caizong10.rdp.connection" },
    connection: {
      id: `neg-${host}-${port}`,
      host, port,
      username: "neguser",
      password: PASSWORD,
      external_config: { color_depth: "16", resize_mode: "dynamic", ignore_cert: false },
    },
    runtime: null,
  };
}

const failures = [];

await send("plugin/initialize", { host: { protocolVersions: [1], hostApiVersion: "1.1.0", features: ["host.requestUserInput"] } });

// A) refused port
const refused = await send("connection/connect", connectParams("127.0.0.1", 1));
if (refused.error) failures.push(`A returned rpc error instead of success:false (${JSON.stringify(refused.error)})`);
else if (refused.result?.success !== false) failures.push("A did not report success:false");
else console.log("A OK  refused TCP:", refused.result.message.slice(0, 90));

// B) dummy listener that accepts then destroys -> RDP sequence fails
const hangup = net.createServer((socket) => socket.destroy());
await new Promise((resolve) => hangup.listen(0, "127.0.0.1", resolve));
const hangupPort = hangup.address().port;
const bad = await send("connection/connect", connectParams("127.0.0.1", hangupPort));
hangup.close();
if (bad.error) failures.push(`B returned rpc error (${JSON.stringify(bad.error)})`);
else if (bad.result?.success !== false) failures.push("B did not report success:false");
else console.log("B OK  RDP sequence failure:", bad.result.message.slice(0, 110));

// Sidecar must still be healthy afterwards
const after = await send("dbx-plugin-rdp/ping", { connectionId: "post-failure" });
if (after.result?.ok !== true) failures.push("sidecar unhealthy after failed connects");
else console.log("C OK  sidecar still responsive after failures");

if (JSON.stringify([refused, bad]).includes(PASSWORD)) failures.push("password leaked in responses");
if (stderrAll.includes(PASSWORD)) failures.push("password leaked in stderr logs");
if (!/RDP|TLS|TCP|sequence|connect/i.test(stderrAll)) failures.push("no handshake diagnostics found on stderr");
else console.log("D OK  stderr carries handshake diagnostics:", stderrAll.split("\n").filter(Boolean).slice(-2).join(" | ").slice(0, 220));

child.stdin.end();
child.kill();

if (failures.length) {
  console.error("NEGATIVE-PATH FAIL:", failures.join("; "));
  process.exit(1);
}
console.log("NEGATIVE-PATH OK: real IronRDP engine fails gracefully, stays alive, leaks nothing");
