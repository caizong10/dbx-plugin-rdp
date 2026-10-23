// Phase 2 acceptance: drive one real connection/test through the sidecar and
// print stderr negotiation lines. Usage:
//   node scripts/real-handshake.mjs <host> <port> <username> <password> [ignoreCert]
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = join(root, "bin", "windows-x64", "dbx-plugin-dbx-plugin-rdp.exe");
const [host, port, username, password, ignoreCert] = process.argv.slice(2);
if (!host || !port || !username || password === undefined) {
  console.error("usage: node scripts/real-handshake.mjs <host> <port> <user> <pass> [ignoreCert]");
  process.exit(2);
}

function encodeFrame(payload) {
  const header = Buffer.alloc(5);
  header.writeUInt8(0, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "pipe"] });
const stderrLines = [];
child.stderr.on("data", (d) => {
  for (const line of d.toString().split(/\r?\n/)) if (line.trim()) stderrLines.push(line);
});

let buffer = Buffer.alloc(0);
const responses = [];
child.stdout.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    if (buffer.length < 5) return;
    const kind = buffer.readUInt8(0);
    const length = buffer.readUInt32BE(1);
    if (buffer.length < 5 + length) return;
    const payload = buffer.subarray(5, 5 + length);
    buffer = buffer.subarray(5 + length);
    if (kind === 0) responses.push(JSON.parse(payload.toString()));
  }
});

function waitFor(id, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const tick = setInterval(() => {
      const msg = responses.find((m) => m.id === id);
      if (msg) { clearInterval(tick); resolve(msg); }
      else if (Date.now() > deadline) { clearInterval(tick); reject(new Error(`timeout id=${id}`)); }
    }, 50);
  });
}

const send = (id, method, params) =>
  child.stdin.write(encodeFrame(Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, method, params })))) &&
  waitFor(id, 60000);

await send(1, "plugin/initialize", { host: { protocolVersions: [1], hostApiVersion: "1.1.0", features: [] } });
const test = await send(2, "connection/test", {
  provider: { id: "io.github.caizong10.rdp.connection" },
  connection: { id: "accept-1", host, port: Number(port), username, password, external_config: { color_depth: "16", resize_mode: "dynamic", ignore_cert: ignoreCert === "1" } },
  runtime: null,
});
console.log("TEST   :", JSON.stringify(test.result ?? test.error));

const connect = await send(3, "connection/connect", {
  provider: { id: "io.github.caizong10.rdp.connection" },
  connection: { id: "accept-1", host, port: Number(port), username, password, external_config: { color_depth: "16", resize_mode: "dynamic", ignore_cert: ignoreCert === "1" } },
  runtime: null,
});
console.log("CONNECT:", JSON.stringify(connect.result ?? connect.error).slice(0, 400));

// Keep stderr flowing briefly to capture negotiation traces, then clean up.
await new Promise((resolve) => setTimeout(resolve, 1500));
await send(4, "connection/disconnect", {
  connection: { id: "accept-1", host, port: Number(port) },
});
child.stdin.end();
child.kill();

console.log("--- stderr (last 25 lines) ---");
for (const line of stderrLines.slice(-25)) console.log(line.replace(/\u001b\[[0-9;]*m/g, ""));
const success = !!(connect.result && connect.result.success === true);
console.log(`REAL-HANDSHAKE ${success ? "SUCCESS" : "FAILED"} (connect=${JSON.stringify(connect.result ?? connect.error).slice(0, 160)})`);
process.exit(success ? 0 : 1);
