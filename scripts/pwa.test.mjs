import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import test from "node:test";

const pwa = (name) => new URL(`../crates/loom-ui/pwa/${name}`, import.meta.url);
const load = (name) => readFile(pwa(name), "utf8");
const manifest = JSON.parse(await load("manifest.webmanifest"));

function pngSize(buffer) {
  assert.equal(buffer.toString("ascii", 1, 4), "PNG", "expected a PNG file");
  return { width: buffer.readUInt32BE(16), height: buffer.readUInt32BE(20) };
}

test("manifest makes Loom installable as a standalone app", () => {
  assert.equal(manifest.name, "Loom");
  assert.equal(manifest.short_name, "Loom");
  assert.equal(manifest.display, "standalone");
  assert.equal(manifest.start_url, ".");
  assert.match(manifest.theme_color, /^#[0-9a-f]{6}$/i);
  assert.match(manifest.background_color, /^#[0-9a-f]{6}$/i);

  const sizes = new Set(manifest.icons.map((icon) => icon.sizes));
  assert.ok(sizes.has("192x192"), "missing a 192x192 icon");
  assert.ok(sizes.has("512x512"), "missing a 512x512 icon");
  assert.ok(
    manifest.icons.some((icon) => icon.purpose?.includes("maskable")),
    "missing a maskable icon",
  );
});

test("manifest icons exist at the declared sizes", async () => {
  const expected = new Map([
    ["icon-192.png", 192],
    ["icon-512.png", 512],
    ["icon-maskable-512.png", 512],
    ["apple-touch-icon.png", 180],
  ]);
  for (const [name, size] of expected) {
    const { width, height } = pngSize(await readFile(pwa(name)));
    assert.deepEqual({ width, height }, { width: size, height: size }, name);
  }
});

test("service worker precaches the shell and handles fetch", async () => {
  const source = await load("sw.js");
  assert.match(source, /addEventListener\(\s*["']fetch["']/);
  assert.match(source, /addEventListener\(\s*["']install["']/);
  for (const asset of ["loom-client.js", "manifest.webmanifest", "icon-192.png"]) {
    assert.ok(source.includes(asset), `service worker does not precache ${asset}`);
  }
});

test("index.html links the manifest and registers the service worker", async () => {
  const html = await readFile(
    fileURLToPath(new URL("../crates/loom-ui/index.html", import.meta.url)),
    "utf8",
  );
  assert.match(html, /rel="manifest" href="manifest\.webmanifest"/);
  assert.match(html, /rel="apple-touch-icon" href="apple-touch-icon\.png"/);
  assert.match(html, /name="apple-mobile-web-app-capable" content="yes"/);
  assert.match(html, /navigator\.serviceWorker\.register\("sw\.js"\)/);
  assert.match(html, /viewport-fit=cover/);
});
