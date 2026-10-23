// Phase 0 self-test: speak Sidecar Protocol v1 (stdio-framed) against the
// built executable and verify the identity handshake + example/ping RPC.
// Frame layout (from dbx-plugin-sdk): kind(1) + len(u32 BE) + payload.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const exe = process.argv[2] ?? join(root, "bin", "windows-x64", "dbx-plugin-dbx-plugin-rdp.exe");
// Read the expected identity from the manifest so a version bump never leaves
// this self-test pinning a stale number.
const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));

function encodeFrame(kind, payload) {
  const header = Buffer.alloc(5);
  header.writeUInt8(kind, 0);
  header.writeUInt32BE(payload.length, 1);
  return Buffer.concat([header, payload]);
}

function decodeFrames(onFrame) {
  let buffer = Buffer.alloc(0);
  return (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      if (buffer.length < 5) return;
      const kind = buffer.readUInt8(0);
      const length = buffer.readUInt32BE(1);
      if (buffer.length < 5 + length) return;
      const payload = buffer.subarray(5, 5 + length);
      buffer = buffer.subarray(5 + length);
      onFrame(kind, payload);
    }
  };
}

const child = spawn(exe, [], { stdio: ["pipe", "pipe", "pipe"] });
let stderrTail = "";
child.stderr.on("data", (d) => { stderrTail += d.toString(); });

const pending = new Map();
let nextId = 1;
function request(method, params) {
  const id = nextId++;
  const frame = encodeFrame(0, Buffer.from(JSON.stringify({ jsonrpc: "2.0", id, method, params })));
  child.stdin.write(frame);
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    setTimeout(() => reject(new Error(`timeout waiting for ${method}`)), 8000).unref?.();
  });
}

const notifications = [];
child.stdout.on("data", decodeFrames((kind, payload) => {
  if (kind !== 0) { notifications.push({ kind, bytes: payload.length }); return; }
  const msg = JSON.parse(payload.toString());
  if (msg.id !== undefined && pending.has(msg.id)) {
    const entry = pending.get(msg.id);
    pending.delete(msg.id);
    msg.error ? entry.reject(new Error(`${msg.error.code} ${msg.error.message}`)) : entry.resolve(msg.result);
  } else {
    notifications.push(msg);
  }
}));

const hostFeatures = ["host.requestUserInput"];
const init = await request("plugin/initialize", {
  host: { protocolVersions: [1], hostApiVersion: "1.1.0", features: hostFeatures },
});
const ping = await request("dbx-plugin-rdp/ping", { connectionId: "self-test" });
const bad = await request("objects/list", {}).then(
  () => ({ leaked: "should have failed" }),
  (error) => ({ code: Number(String(error.message).split(" ")[0]) }),
);

const failures = [];
if (init.protocolVersion !== 1) failures.push("protocolVersion != 1");
if (init.plugin?.id !== manifest.id) failures.push(`plugin.id = ${init.plugin?.id}`);
if (init.plugin?.version !== manifest.version) failures.push(`plugin.version = ${init.plugin?.version} (manifest ${manifest.version})`);
if (!Array.isArray(init.capabilities)) failures.push("capabilities missing");
if (ping?.ok !== true) failures.push("ping.ok != true");
if (bad.code !== -32601) failures.push(`unknown method code = ${JSON.stringify(bad)}`);

console.log(JSON.stringify({ init, ping, unknownMethod: bad, stderrTail: stderrTail.slice(0, 200) }, null, 2));
child.stdin.end();
child.kill();

if (failures.length) {
  console.error("HANDSHAKE FAIL: " + failures.join("; "));
  process.exit(1);
}
console.log("HANDSHAKE OK: framed protocol, identity matches manifest, ping works, unknown method -> -32601");
