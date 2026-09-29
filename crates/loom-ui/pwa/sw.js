// Loom service worker.
//
// Registering a service worker with a fetch handler is what lets browsers
// offer "Install app" and launch Loom in a standalone (chromeless) window on
// Android and desktop. It also keeps a copy of the app shell so an installed
// Loom can start while offline. Fresh builds always win when the network is
// reachable.
const CACHE = "loom-v1";
const SHELL = [
  "./",
  "./loom-client.js",
  "./manifest.webmanifest",
  "./icon.svg",
  "./icon-192.png",
  "./icon-512.png",
  "./apple-touch-icon.png",
];
// The loader fetches this with a random cache-busting query and `no-store`, so
// it must always be read from the network. Cache it under a stable URL that can
// be served as an offline fallback.
const CLIENT_MANIFEST = "./loom-client.json";
const IMMUTABLE_ASSET = /loom-ui-[0-9a-f]+(?:_bg\.wasm|\.js)$/;

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE)
      .then((cache) => cache.addAll(SHELL))
      .catch(() => undefined)
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys.filter((key) => key !== CACHE).map((key) => caches.delete(key)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

async function networkFirst(request, fallbackUrl) {
  const cache = await caches.open(CACHE);
  try {
    const response = await fetch(request);
    if (response.ok) {
      await cache.put(fallbackUrl, response.clone());
    }
    return response;
  } catch (error) {
    const cached = await cache.match(fallbackUrl, { ignoreSearch: true });
    if (cached) return cached;
    throw error;
  }
}

async function cacheFirst(request) {
  const cache = await caches.open(CACHE);
  const cached = await cache.match(request, { ignoreSearch: true });
  if (cached) return cached;
  const response = await fetch(request);
  if (response.ok) {
    await cache.put(request, response.clone());
  }
  return response;
}

self.addEventListener("fetch", (event) => {
  const { request } = event;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;

  if (url.pathname.endsWith("/loom-client.json")) {
    event.respondWith(networkFirst(request, CLIENT_MANIFEST));
    return;
  }

  if (request.mode === "navigate") {
    event.respondWith(networkFirst(request, "./"));
    return;
  }

  if (IMMUTABLE_ASSET.test(url.pathname)) {
    event.respondWith(cacheFirst(request));
    return;
  }

  event.respondWith(networkFirst(request));
});
