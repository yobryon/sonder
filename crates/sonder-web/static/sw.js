// Sonder service worker (module worker; registered by pwa.js in worker mode only).
//
// Precaches the app shell (UI + wasm + source adapters + catalog) under a cache named after
// the build hash that scripts/build-site.sh injects below, serves those cache-first, and is
// network-first (cache fallback) for everything else under its scope. `api/*`, the `ws` /
// `audio` sockets and `records/` are never touched. Before the build script runs, BUILD is
// the literal placeholder and PRECACHE is just pwa.js's SHELL list (dev servers).

import { precacheList, requestKind, cacheName } from './pwa.js';

const BUILD = '__BUILD__';
const PRECACHE = precacheList(/*__PRECACHE__*/[]);
const SHELL = new Set(PRECACHE);
const CACHE = cacheName(BUILD);
const SCOPE = self.registration ? self.registration.scope : './';

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(CACHE);
    // One missing file (e.g. no pkg/ on a dev server) must not sink the whole install.
    const results = await Promise.allSettled(PRECACHE.map((u) => cache.add(new Request(u, { cache: 'reload' }))));
    const failed = results.filter((r) => r.status === 'rejected').length;
    if (failed) console.warn(`[sw] ${failed}/${PRECACHE.length} shell files not cached`);
    await self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) if (name.startsWith('sonder-') && name !== CACHE) await caches.delete(name);
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.method !== 'GET') return;
  const kind = requestKind(req.url, SCOPE, SHELL);
  if (kind === 'bypass') return;
  event.respondWith(kind === 'shell' ? cacheFirst(req) : networkFirst(req));
});

async function cacheFirst(req) {
  const cache = await caches.open(CACHE);
  const hit = await cache.match(req, { ignoreSearch: true });
  if (hit) return hit;
  try {
    const res = await fetch(req);
    if (res.ok) cache.put(req, res.clone());
    return res;
  } catch (e) {
    if (req.mode === 'navigate') { const index = await cache.match('./index.html'); if (index) return index; }
    throw e;
  }
}

async function networkFirst(req) {
  const cache = await caches.open(CACHE);
  try {
    const res = await fetch(req);
    if (res.ok && res.type === 'basic') cache.put(req, res.clone());
    return res;
  } catch (e) {
    const hit = await cache.match(req, { ignoreSearch: true });
    if (hit) return hit;
    if (req.mode === 'navigate') { const index = await cache.match('./index.html'); if (index) return index; }
    throw e;
  }
}
