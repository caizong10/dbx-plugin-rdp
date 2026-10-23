// Phase 1 closure check against `dbx-plugin dev` (http://127.0.0.1:5190).
// Drives the mock host exactly like the browser UI does:
//   save connection -> test -> connect -> frame-document -> bridge invoke
//   -> frames/close (auto disconnect) -> reconnect idempotency -> cleanup.
// Asserts happy paths, error paths, and that no plaintext password ever
// appears in host diagnostics.
const base = process.env.DBX_DEV_URL || "http://127.0.0.1:5190";
// Generated per run: the value itself is irrelevant, it only has to be a
// string the assertions can prove never reached context, events or logs.
// Never commit real credentials for these scripts.
const PASSWORD = `probe-${Math.random().toString(36).slice(2)}-${Date.now().toString(36)}`;
const failures = [];
let cookie = "";
let csrf = "";

function check(name, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures.push(name);
}

async function get(path) {
  const response = await fetch(base + path, { headers: { origin: base, cookie } });
  const setCookie = response.headers.getSetCookie?.() ?? [];
  for (const entry of setCookie) if (entry.startsWith("dbx_dev_session_")) cookie = entry.split(";")[0];
  const text = await response.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* non-JSON */ }
  return { status: response.status, json, text };
}

async function post(path, body, { expectError = false } = {}) {
  const response = await fetch(base + path, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      origin: base,
      cookie,
      "x-mock-csrf": csrf,
      "x-dbx-page": "closure",
    },
    body: JSON.stringify(body ?? {}),
  });
  const text = await response.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* non-JSON */ }
  if (!expectError && response.status >= 400) {
    throw new Error(`POST ${path} -> ${response.status} ${text.slice(0, 200)}`);
  }
  return { status: response.status, json, text };
}

// 1. session
const boot = await get("/api/bootstrap");
csrf = boot.json.csrf;
check("bootstrap session + csrf", boot.status === 200 && !!csrf && !!cookie);

const values = {
  display_name: "Closure RDP",
  host: process.env.RDP_TEST_HOST ?? "192.0.2.10",
  port: Number(process.env.RDP_TEST_PORT ?? 3389),
  username: "tester",
  password: PASSWORD,
  domain: "WORKGROUP",
  color_depth: "16",
  resize_mode: "fixed",
  width: 1024,
  height: 768,
  ignore_cert: true,
};

// 2. save connection  (host wraps every /api response in { value })
const saved = await post("/api/connections/save", { providerId: "io.github.caizong10.rdp.connection", values });
const connectionId = saved.json?.value?.id;
check("save connection", !!connectionId, `id=${connectionId}`);

// 3. test (valid)
const tested = await post("/api/connections/test", { providerId: "io.github.caizong10.rdp.connection", values });
check("connection/test accepted", tested.status === 200);

// 4. test (invalid: empty host) must fail with a readable error
const badTest = await post(
  "/api/connections/test",
  { providerId: "io.github.caizong10.rdp.connection", values: { ...values, host: "" } },
  { expectError: true },
);
const badMessage = JSON.stringify(badTest.json ?? {});
check("empty-host test rejected", badTest.status >= 400 && /host/i.test(badMessage), `status=${badTest.status}`);

// 5. connect -> frame
const connected = await post("/api/connections/connect", { id: connectionId });
const frame = connected.json?.value?.frame;
check("connect opens workbench frame", !!frame?.id && frame.connectionId === connectionId);

// 6. frame document (sandbox html + fresh channel)
const doc = await post("/api/frame-document", { frameId: frame.id });
const docValue = doc.json?.value ?? {};
check("frame document serves built UI", /<div id="app"|dbxPlugin/i.test(docValue.html ?? "") && !!docValue.channel);

// 7. bridge invoke -> sidecar ping through the same frame
const ping = await post("/api/bridge", {
  frameId: frame.id,
  channel: docValue.channel,
  method: "backend.invoke",
  params: { method: "dbx-plugin-rdp/ping", params: { connectionId } },
});
const pingValue = ping.json?.value ?? {};
check("bridge invoke ping", pingValue.ok === true || pingValue.result?.ok === true, JSON.stringify(pingValue));

// 8. bridge context carries connectionId (no credentials!)
const ctx = await post("/api/bridge", {
  frameId: frame.id,
  channel: docValue.channel,
  method: "host.getContext",
  params: {},
});
const context = ctx.json?.value ?? {};
check("workbench context has connectionId", context.connectionId === connectionId);
check("context never carries the password", !JSON.stringify(context).includes(PASSWORD));

// 9. stale channel must be rejected
const stale = await post("/api/bridge", {
  frameId: frame.id,
  channel: "stale-channel",
  method: "backend.invoke",
  params: { method: "dbx-plugin-rdp/ping", params: {} },
}, { expectError: true });
check("stale frame channel rejected", stale.status >= 400);

// 10. close frame -> host disconnects the connection automatically
const closed = await post("/api/frames/close", { id: frame.id });
const afterClose = closed.json?.value?.connections ?? [];
check("frame close disconnects", afterClose.some((c) => c.id === connectionId && c.connected === false));

// 11. reconnect (idempotent re-connect on the same session key)
const reconnected = await post("/api/connections/connect", { id: connectionId });
const reconnectedValue = reconnected.json?.value ?? {};
check("reconnect works", !!reconnectedValue.frame?.id && reconnectedValue.connections.some((c) => c.id === connectionId && c.connected === true));
const reDisconnect = await post("/api/connections/disconnect", { id: connectionId });
check("explicit disconnect ok", reDisconnect.status === 200);
// double disconnect must not error (idempotent)
const doubleDisconnect = await post("/api/connections/disconnect", { id: connectionId });
check("double disconnect idempotent", doubleDisconnect.status === 200);

// 12. diagnostics must not contain the plaintext password (host redacts,
// but our handler messages must not echo it either)
const diag = await get("/api/diagnostics?after=0&limit=1000");
check("diagnostics never contain password", !diag.text.includes(PASSWORD));

// 13. cleanup
const removed = await post("/api/connections/delete", { id: connectionId });
check("connection deleted", !(removed.json?.value?.connections ?? []).some((c) => c.id === connectionId));

if (failures.length) {
  console.error(`CLOSURE FAIL: ${failures.join("; ")}`);
  process.exit(1);
}
console.log("CLOSURE OK: save/test/connect/frame/bridge/context/disconnect/reconnect/delete all verified");
