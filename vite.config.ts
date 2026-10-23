import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

// DBX sandbox loads the entry through an injected <base> under ui.root,
// so all asset URLs must be relative (base "./" per the plugin docs).
// root/outDir are pinned to the config file location because the dev host
// may spawn builds with a verbatim (`\\?\...`) working directory.
export default defineConfig({
  root: fileURLToPath(new URL("./src", import.meta.url)),
  base: "./",
  build: {
    outDir: fileURLToPath(new URL("./ui", import.meta.url)),
    emptyOutDir: true,
    target: "es2022",
    modulePreload: false,
  },
});
