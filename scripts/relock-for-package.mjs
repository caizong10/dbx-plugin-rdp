// Bring backend/Cargo.lock to the exact state `dbx-plugin package --locked`
// expects: the CLI injects
//   --config patch.crates-io.dbx-plugin-sdk.path="<cli sdk-root copy>"
// which cargo records in the lock as `[[patch.unused]]` (our dependency is a
// path dependency, so the patch never resolves; the lock entry stores only
// name+version, hence this works with any valid sdk-root copy path).
// Without the entry, the CLI's --locked build fails with
// "cannot update the lock file ... because --locked was passed".
//
// Run right before `dbx-plugin package .`:
//   node scripts/relock-for-package.mjs
// The sdk-root copy is auto-discovered: DBX_PLUGIN_SDK_ROOT, then the locally
// installed @dbx-app/plugin-cli package, then npx's cache. Install the CLI
// first in CI (`npm i -D @dbx-app/plugin-cli` or `npm i -g`) so it is found
// without network access at this step.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Resolve the installed @dbx-app/plugin-cli directory via Node's resolver. */
function installedCli() {
  try {
    const require = createRequire(join(root, "package.json"));
    return dirname(require.resolve("@dbx-app/plugin-cli/package.json"));
  } catch {
    return null;
  }
}

function findSdkRoot() {
  if (process.env.DBX_PLUGIN_SDK_ROOT) return process.env.DBX_PLUGIN_SDK_ROOT;
  const candidates = [
    installedCli() && join(installedCli(), "sdk-root"),
    join(root, "node_modules", "@dbx-app", "plugin-cli", "sdk-root"),
  ];
  for (const candidate of candidates) {
    if (candidate && existsSync(candidate)) return candidate;
  }
  // npx keeps package copies under the npm cache (_npx/<hash>/node_modules/…).
  const cache = join(process.env.LOCALAPPDATA ?? "", "npm-cache", "_npx");
  const homeCache = join(process.env.HOME ?? "", ".npm", "_npx");
  for (const base of [cache, homeCache]) {
    if (!base || !existsSync(base)) continue;
    for (const entry of readdirSync(base)) {
      const candidate = join(base, entry, "node_modules", "@dbx-app", "plugin-cli", "sdk-root");
      if (existsSync(candidate)) return candidate;
    }
  }
  console.error(
    "sdk-root not found. Install @dbx-app/plugin-cli (npm i -D @dbx-app/plugin-cli) " +
      "or set DBX_PLUGIN_SDK_ROOT to the @dbx-app/plugin-cli package dir.",
  );
  process.exit(2);
}

const SDK = findSdkRoot().replace(/\\/g, "/") + "/plugins/sdk/rust/dbx-plugin-sdk";

const args = [
  "generate-lockfile",
  "--manifest-path", join(root, "backend", "Cargo.toml").replace(/\\/g, "/"),
  // Forward slashes avoid every TOML escaping pitfall.
  "--config", `patch.crates-io.dbx-plugin-sdk.path="${SDK}"`,
];
const result = spawnSync("cargo", args, { encoding: "utf8", shell: false });
console.log("CONFIG:", args[args.length - 1]);
console.log("EXIT:", result.status);
const out = ((result.stdout ?? "") + (result.stderr ?? "")).split(/\r?\n/).filter(Boolean);
if (out.length) console.log(out.slice(-3).join("\n"));
process.exit(result.status ?? 1);
