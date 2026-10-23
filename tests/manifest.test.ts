import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const manifest = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
const cargo = readFileSync(join(root, "backend", "Cargo.toml"), "utf8");

function field(key: string) {
  const provider = manifest.contributions.find((c: any) => c.type === "connection-provider");
  return provider?.fields?.find((f: any) => f.key === key);
}

describe("manifest contract (fact sheet §3/§4)", () => {
  it("declares host_api engines and a reverse-domain id", () => {
    expect(manifest.manifest_version).toBe(1);
    expect(manifest.id).toBe("io.github.caizong10.rdp");
    // DBX requires a reverse-domain id of lowercase letters, digits, . _ -.
    expect(manifest.id).toMatch(/^[a-z0-9._-]+$/);
    expect(manifest.engines.host_api).toBe("1.4.0");
    expect(manifest.publisher).toBe("caizong10");
  });

  it("points source and homepage at the public repository", () => {
    // Publishing metadata is what DBX shows on the installed-plugin card.
    expect(manifest.source).toBe("https://github.com/caizong10/dbx-plugin-rdp");
    expect(manifest.homepage).toBe("https://github.com/caizong10/dbx-plugin-rdp");
  });

  it("requests the minimal permission set", () => {
    // The UI never reads the clipboard (the sidecar owns it via arboard), so
    // host.clipboard:read must not be requested.
    expect([...manifest.permissions].sort()).toEqual(["host.binary", "host.events", "host.workbench"]);
    expect(manifest.permissions).not.toContain("host.clipboard:read");
  });

  it("uses stdio-framed backend transport with host.binary permission", () => {
    expect(manifest.permissions).toContain("host.binary");
    expect(manifest.permissions).toContain("host.workbench");
    expect(manifest.permissions).toContain("host.events");
    expect(manifest.entrypoints.backend.transport).toBe("stdio-framed");
    expect(manifest.entrypoints.backend.protocol_versions).toEqual([1]);
    // The packager appends the target directory and platform extension itself
    // (bin/foo -> bin/windows-x64/foo.exe, bin/linux-x64/foo, ...), so the
    // manifest must stay target-agnostic. A hard-coded bin/<target>/ path
    // would pin the package to one platform.
    expect(manifest.entrypoints.backend.executable).toMatch(/^bin\/[^/]+$/);
    expect(manifest.entrypoints.backend.executable).not.toMatch(/\.exe$/);
  });

  it("binds the password field to secret and port to a number", () => {
    expect(field("password")?.type).toBe("password");
    expect(field("password")?.binding).toBe("secret");
    expect(field("port")?.binding).toBe("port");
    expect(field("port")?.type).toBe("number");
    expect(field("port")?.default).toBe(3389);
    expect(field("host")?.binding).toBe("host");
    expect(field("display_name")?.binding).toBe("name");
    expect(field("username")?.binding).toBe("username");
  });

  it("keeps fixed-size fields visible only in fixed resize mode", () => {
    const resize = field("resize_mode");
    expect(resize?.type).toBe("select");
    expect(resize?.options?.map((o: any) => o.value)).toEqual(["dynamic", "fixed"]);
    for (const key of ["width", "height"]) {
      const f = field(key);
      expect(f?.type).toBe("number");
      expect(f?.visible_when).toEqual({ field: "resize_mode", one_of: ["fixed"] });
    }
    const depth = field("color_depth");
    expect(depth?.options?.map((o: any) => o.value).sort()).toEqual(["16", "24", "32"]);
  });

  it("points the provider workbench at the declared workbench contribution", () => {
    const provider = manifest.contributions.find((c: any) => c.type === "connection-provider");
    const workbench = manifest.contributions.find((c: any) => c.type === "workbench");
    expect(workbench).toBeDefined();
    expect(provider.workbench).toBe(workbench.id);
    expect(provider.capabilities).toEqual(["test", "connect", "disconnect"]);
  });

  it("ships zh-CN and en localizations for every field", () => {
    const provider = manifest.contributions.find((c: any) => c.type === "connection-provider");
    for (const locale of ["zh-CN", "en"]) {
      const loc = manifest.localizations?.[locale];
      expect(loc, `missing locale ${locale}`).toBeTruthy();
      const fields = loc.contributions?.[provider.id]?.fields ?? {};
      for (const f of provider.fields) {
        expect(fields[f.key]?.label, `${locale}: missing label for ${f.key}`).toBeTruthy();
      }
    }
  });

  it("keeps the Cargo package version equal to the manifest version", () => {
    const versionLine = cargo.match(/^version = "([^"]+)"/m)?.[1];
    expect(versionLine).toBe(manifest.version);
  });
});
