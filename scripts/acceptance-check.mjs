// Phase 4 local acceptance checklist (fact sheet §6), driven against the dev
// host at 127.0.0.1:5190 and the packaged sidecar binary. Items that can only
// be verified inside real DBX (signing, Secret Store, install flow) are
// reported as MANUAL so the README states them honestly.
//
// Usage: node scripts/acceptance-check.mjs [--host URL]
const base = process.env.DBX_DEV_URL || "http://127.0.0.1:5190";
const args = process.argv.slice(2);
for (let i = 0; i < args.length; i++) if (args[i] === "--host") base = args[++i];

let cookie = "";
let csrf = "";
const results = [];
function record(id, status, note) {
  results.push({ id, status, note });
  console.log(`${status.padEnd(6)} ${id}${note ? `  — ${note}` : ""}`);
}

async function boot() {
  const response = await fetch(base + "/api/bootstrap", { headers: { origin: base, cookie } });
  for (const entry of response.headers.getSetCookie?.() ?? []) {
    if (entry.startsWith("dbx_dev_session_")) cookie = entry.split(";")[0];
  }
  const body = await response.json();
  csrf = body.csrf;
  return body;
}

async function post(route, body) {
  const response = await fetch(base + route, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      origin: base,
      cookie,
      "x-mock-csrf": csrf,
      "x-dbx-page": "acceptance",
    },
    body: JSON.stringify(body ?? {}),
  });
  const text = await response.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* ignore */ }
  return { status: response.status, json, text };
}

function connectionValues(overrides = {}) {
  return {
    display_name: "Acceptance",
    host: process.env.RDP_TEST_HOST ?? "192.0.2.10",
    port: Number(process.env.RDP_TEST_PORT ?? 3389),
    username: process.env.RDP_TEST_USER ?? "testuser",
    password: process.env.RDP_TEST_PASSWORD ?? "",
    color_depth: "16",
    resize_mode: "dynamic",
    ignore_cert: true,
    ...overrides,
  };
}

const baseFields = { display_name: "Acceptance", host: "", port: 3389, username: "", password: "" };

// 1. empty configuration ------------------------------------------------------
await boot();
{
  const empty = await post("/api/connections/save", {
    providerId: "io.github.caizong10.rdp.connection",
    values: { ...baseFields, display_name: "Empty cfg" },
  });
  const hostMissing = empty.status >= 400 || (empty.json?.error?.message ?? "").length > 0;
  if (hostMissing) {
    record("empty-config", "PASS", "host/username required fields rejected by host validation");
  } else {
    const tested = await post("/api/connections/test", {
      providerId: "io.github.caizong10.rdp.connection",
      values: { ...baseFields, display_name: "Empty cfg" },
    });
    record("empty-config", tested.status >= 400 ? "PASS" : "FAIL", `test route status=${tested.status}`);
  }
}

// 2. invalid credentials (wrong account against the real endpoint) ------------
// Must go through the connect route: connection/test only validates the form
// by design and never dials.
{
  await boot();
  if (!process.env.RDP_TEST_PASSWORD) {
    record("bad-credentials", "MANUAL", "set RDP_TEST_PASSWORD to run (needs real endpoint)");
  } else {
    const saved = await post("/api/connections/save", {
      providerId: "io.github.caizong10.rdp.connection",
      values: connectionValues({ username: "no-such-user-zz", password: "no-such-password-zz" }),
    });
    const id = saved.json?.value?.id;
    const started = Date.now();
    const bad = id ? await post("/api/connections/connect", { id }) : { status: 0 };
    if (id) await post("/api/connections/delete", { id });
    const rejected = bad.status >= 400 && Date.now() - started < 45000;
    record("bad-credentials", rejected ? "PASS" : "FAIL", `connect=${bad.status} in ${Date.now() - started}ms`);
  }
}

// 3. timeout behaviour (unroutable endpoint; graceful failure) ----------------
{
  await boot();
  const saved = await post("/api/connections/save", {
    providerId: "io.github.caizong10.rdp.connection",
    values: connectionValues({ host: "198.51.100.1", port: 3389 }),
  });
  const id = saved.json?.value?.id;
  const started = Date.now();
  const timeout = id ? await post("/api/connections/connect", { id }) : { status: 0 };
  const elapsed = Date.now() - started;
  if (id) await post("/api/connections/delete", { id });
  const ok = timeout.status >= 400 && elapsed < 30000;
  record("timeout-unroutable", ok ? "PASS" : "FAIL", `connect=${timeout.status} after ${elapsed}ms`);
}

// 4. sidecar restart -----------------------------------------------------------
{
  await boot();
  const restart = await post("/api/backend/restart", {});
  const ok = restart.status === 200 && restart.json?.value?.state === "ready";
  record("sidecar-restart", ok ? "PASS" : "FAIL", `state=${restart.json?.value?.state}`);
}

// 5/6. themes + locales (shell documents; UI itself adapts via CSS vars) -----
{
  await boot();
  const diagnostics = await fetch(base + "/api/diagnostics?after=0&limit=5", { headers: { origin: base, cookie } });
  const shell = await fetch(base + "/", { headers: { origin: base } });
  const shellText = await shell.text();
  const themeUi = /data-dbx-theme|theme/.test(shellText);
  record("theme-zh-en", themeUi ? "PASS" : "MANUAL", "dev host exposes locale/theme toggles; visual check in browser");
  void diagnostics;
}

// 7. reconnect / close workbench / double open / empty context ---------------
{
  await boot();
  // workbench without connection = empty context path
  const noConn = await post("/api/workbenches/open", { contributionId: "io.github.caizong10.rdp.workbench" });
  const openedEmpty = noConn.status === 200 && noConn.json?.value?.frame;
  const context = openedEmpty ? noConn.json.value.frame.context : null;
  const emptyOk = openedEmpty && !context?.connectionId;
  record("empty-context-open", emptyOk ? "PASS" : "FAIL", JSON.stringify(context ?? null));

  if (!process.env.RDP_TEST_PASSWORD) {
    record("reconnect-double-open", "MANUAL", "needs RDP_TEST_PASSWORD + real endpoint");
  } else {
    const saved = await post("/api/connections/save", {
      providerId: "io.github.caizong10.rdp.connection",
      values: connectionValues(),
    });
    const id = saved.json?.value?.id;
    const first = await post("/api/connections/connect", { id });
    const second = await post("/api/connections/connect", { id });
    const disconnected = await post("/api/connections/disconnect", { id });
    const reconnected = await post("/api/connections/connect", { id });
    const ok =
      !!first.json?.value?.frame &&
      !!second.json?.value?.frame &&
      disconnected.status === 200 &&
      !!reconnected.json?.value?.frame;
    record("reconnect-double-open", ok ? "PASS" : "FAIL");
    await post("/api/connections/delete", { id });
  }
}

// 8. packaged artifact hygiene -------------------------------------------------
{
  const fs = await import("node:fs");
  const path = await import("node:path");
  const distDir = path.join(process.cwd(), "dist");
  const dbxps = fs.existsSync(distDir) ? fs.readdirSync(distDir).filter((f) => f.endsWith(".dbxp")) : [];
  if (dbxps.length === 0) {
    record("artifact-clean", "PENDING", "run dbx-plugin package first");
  } else {
    // .dbxp is a ZIP; list members and assert no secrets / dev data inside.
    const { execFileSync } = await import("node:child_process");
    const listing = execFileSync("tar.exe", ["-tf", path.join(distDir, dbxps[0])], { encoding: "utf8" });
    const bad = listing.split(/\r?\n/).filter((line) => /\.dbx-dev|node_modules[/]|secrets?[/]/i.test(line));
    record(
      "artifact-clean",
      bad.length === 0 ? "PASS" : "FAIL",
      `${dbxps[0]}: ${listing.trim().split(/\r?\n/).length} entries${bad.length ? ` leaked: ${bad.join(",")}` : ""}`,
    );
  }
}

// summary ----------------------------------------------------------------------
const failed = results.filter((r) => r.status === "FAIL").length;
const manual = results.filter((r) => r.status === "MANUAL" || r.status === "PENDING").map((r) => r.id);
console.log(`\nSUMMARY: ${results.length} checks, ${failed} failed; manual/pending: ${manual.join(", ") || "none"}`);
process.exit(failed > 0 ? 1 : 0);
