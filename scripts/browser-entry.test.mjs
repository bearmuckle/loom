import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createContext, SourceTextModule, SyntheticModule } from "node:vm";
import test from "node:test";

const buildScript = fileURLToPath(new URL("./build-browser-entry.mjs", import.meta.url));
const loaderSource = await readFile(new URL("../crates/loom-ui/loom-client.js", import.meta.url), "utf8");

async function fixture(t, names) {
  const dist = await mkdtemp(join(tmpdir(), "loom-browser-entry-"));
  t.after(() => rm(dist, { recursive: true, force: true }));
  for (const name of names) await writeFile(join(dist, name), "");
  return dist;
}

function build(dist) {
  execFileSync(process.execPath, [buildScript, dist], { stdio: "pipe" });
}

test("publishes a matching manifest and unchanged stable loader", async (t) => {
  const dist = await fixture(t, ["loom-ui-abc123.js", "loom-ui-abc123_bg.wasm", "unrelated.js"]);
  build(dist);
  assert.deepEqual(JSON.parse(await readFile(join(dist, "loom-client.json"), "utf8")), {
    js: "loom-ui-abc123.js",
    wasm: "loom-ui-abc123_bg.wasm",
  });
  assert.equal(await readFile(join(dist, "loom-client.js"), "utf8"), loaderSource);
});

test("rejects missing, ambiguous, and incomplete builds", async (t) => {
  for (const names of [[], ["loom-ui-aaa.js", "loom-ui-bbb.js"], ["loom-ui-aaa.js"]]) {
    const dist = await fixture(t, names);
    assert.throws(() => build(dist));
    await assert.rejects(readFile(join(dist, "loom-client.json")), { code: "ENOENT" });
  }
});

async function load(manifest, status = 200) {
  const calls = {};
  const window = { location: new URL("https://loom-ai.org/client?demo=true") };
  const context = createContext({
    URL, crypto, window, CustomEvent,
    fetch: async (url, options) => {
      calls.manifestUrl = url;
      calls.cache = options.cache;
      return { ok: status === 200, status, json: async () => manifest };
    },
    dispatchEvent: (event) => { calls.event = event; },
  });
  const bindings = new SyntheticModule(["default"], function () {
    this.setExport("default", async (options) => {
      calls.wasmUrl = options.module_or_path;
      return "wasm-instance";
    });
  }, { context });
  const loader = new SourceTextModule(loaderSource, {
    context,
    initializeImportMeta: (meta) => {
      meta.url = "https://bearmuckle.github.io/loom/loom-client.js?v=embed-nonce";
    },
    importModuleDynamically: async (url) => {
      calls.jsUrl = url;
      await bindings.link(() => {});
      await bindings.evaluate();
      return bindings;
    },
  });
  await loader.link(() => {});
  await loader.evaluate();
  assert.equal(window.wasmBindings, bindings.namespace);
  return calls;
}

test("cached bootstrap discovers successive releases from Pages on a different host", async () => {
  const requests = [];
  for (const hash of ["aaa", "bbb"]) {
    const calls = await load({ js: `loom-ui-${hash}.js`, wasm: `loom-ui-${hash}_bg.wasm` });
    assert.equal(calls.manifestUrl.origin, "https://bearmuckle.github.io");
    assert.equal(calls.manifestUrl.pathname, "/loom/loom-client.json");
    assert.equal(calls.cache, "no-store");
    requests.push(calls.manifestUrl.searchParams.get("v"));
    assert.equal(calls.jsUrl, `https://bearmuckle.github.io/loom/loom-ui-${hash}.js`);
    assert.equal(calls.wasmUrl, `https://bearmuckle.github.io/loom/loom-ui-${hash}_bg.wasm`);
    assert.equal(calls.event.type, "TrunkApplicationStarted");
    assert.equal(calls.event.detail.wasm, "wasm-instance");
  }
  assert.ok(requests.every(Boolean));
  assert.notEqual(requests[0], requests[1]);
});

test("reports manifest HTTP failures before loading the client", async () => {
  await assert.rejects(load(null, 404), /Could not load Loom client manifest: HTTP 404/);
});
