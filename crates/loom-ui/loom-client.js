// Keep this bootstrap independent of build hashes so cached copies still load
// the current release. Resolve assets against this module, not the host page.
const manifestUrl = new URL("./loom-client.json", import.meta.url);
manifestUrl.searchParams.set("v", crypto.randomUUID());
const response = await fetch(manifestUrl, { cache: "no-store" });
if (!response.ok) {
  throw new Error(`Could not load Loom client manifest: HTTP ${response.status}`);
}
const { js, wasm } = await response.json();
const bindings = await import(new URL(js, import.meta.url).href);
const instance = await bindings.default({
  module_or_path: new URL(wasm, import.meta.url).href,
});
window.wasmBindings = bindings;
dispatchEvent(new CustomEvent("TrunkApplicationStarted", { detail: { wasm: instance } }));
