import { copyFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

// Run after Trunk so the manifest always refers to the exact deployed build.
const dist = process.argv[2];
if (!dist) throw new Error("Usage: node scripts/build-browser-entry.mjs <dist>");
const files = await readdir(dist);
// Trunk hashes the JavaScript module and the WASM binary separately: wasm-opt
// rewrites the binary after the module is named, so the two hashes differ.
const pick = (pattern, what) => {
  const found = files.filter((name) => pattern.test(name));
  if (found.length !== 1) {
    throw new Error(`Expected one hashed Loom ${what} in ${dist}, found ${found.length}`);
  }
  return found[0];
};
const js = pick(/^loom-ui-[a-f0-9]+\.js$/, "module");
const wasm = pick(/^loom-ui-[a-f0-9]+_bg\.wasm$/, "WASM asset");
await writeFile(join(dist, "loom-client.json"), `${JSON.stringify({ js, wasm }, null, 2)}\n`);
await copyFile(new URL("../crates/loom-ui/loom-client.js", import.meta.url), join(dist, "loom-client.js"));
