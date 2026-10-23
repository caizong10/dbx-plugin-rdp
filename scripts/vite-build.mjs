// Dev-host UI builder.
// The DBX dev host spawns build commands with a verbatim (`\\?\D:\...`) cwd,
// which breaks Vite's relative root resolution. This script locates the
// project from its own URL, strips the prefix, and drives Vite from a clean
// absolute cwd. Watch mode follows the DBX_UI_BUILD_SUCCESS protocol:
// exactly one standalone marker line after each complete successful build.
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

function projectRoot() {
  let here = fileURLToPath(import.meta.url);
  here = here.replace(/^\\\\\?\\/, "").replace(/^\\\/\?\\/, "");
  return path.resolve(path.dirname(here), "..");
}

const root = projectRoot();
const watch = process.argv.includes("--watch");
const viteBin = path.join(root, "node_modules", "vite", "bin", "vite.js");

const child = spawn(
  process.execPath,
  [viteBin, "build", ...(watch ? ["--watch"] : [])],
  { cwd: root, stdio: ["ignore", "pipe", "pipe"] },
);

// Vite writes progress to both stdout and stderr; forward stderr as-is and
// stdout as-is, but emit the protocol marker only once per build so the host
// hot-reloads without duplicate log noise.
let markerPending = false;
child.stderr.on("data", (chunk) => process.stderr.write(chunk));
child.stdout.on("data", (chunk) => {
  process.stdout.write(chunk);
  if (watch && /built in/.test(chunk.toString()) && !markerPending) {
    markerPending = true;
    setTimeout(() => {
      console.log("DBX_UI_BUILD_SUCCESS");
      markerPending = false;
    }, 100);
  }
});

child.on("exit", (code) => {
  if (code === 0 && !watch) {
    console.log("DBX_UI_BUILD_SUCCESS");
  }
  process.exit(code ?? 1);
});
