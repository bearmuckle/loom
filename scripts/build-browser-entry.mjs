import { copyFile, readdir, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";

// Run after Trunk so the manifest always refers to the exact deployed build.
const dist = process.argv[2];
if (!dist) throw new Error("Usage: node scripts/build-browser-entry.mjs <dist>");
const modules = (await readdir(dist)).filter((name) => /^loom-ui-[a-f0-9]+\.js$/.test(name));
if (modules.length !== 1) {
  throw new Error(`Expected one hashed Loom module in ${dist}, found ${modules.length}`);
}
const js = modules[0];
const wasm = js.replace(/\.js$/, "_bg.wasm");
if (!(await stat(join(dist, wasm))).isFile()) {
  throw new Error(`Missing WASM asset: ${wasm}`);
}
await writeFile(join(dist, "loom-client.json"), `${JSON.stringify({ js, wasm }, null, 2)}\n`);
await copyFile(new URL("../crates/loom-ui/loom-client.js", import.meta.url), join(dist, "loom-client.js"));
