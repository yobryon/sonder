// Progressive-web-app glue, shared by the page (app.js) and the service worker (sw.js).
// Everything exported at the top is pure (node-testable); the browser-only parts take the
// objects they touch as arguments or guard every access, so importing this module from a
// worker, a service worker or node never throws.
//
//   page:  install({ transport, installButton, onPlay, onPause })   registers sw.js in worker
//          media.setTitle(scene) / media.setPlaying(bool)             mode, wires the install
//                                                                     button, Media Session,
//                                                                     hidden-tab snapshot rate
//   sw:    precacheList(extra), requestKind(url, scope, set), cacheName(build)

/** The app shell: every file the UI needs to boot offline in worker mode. build-site.sh
 *  appends the real file list (pkg/*, sources/*, catalog.json…) to this at build time. */
export const SHELL = [
  './', './index.html', './app.js', './app.css', './worklet.js', './transport.js', './worker.js',
  './timing.js', './scene-url.js', './captions.js', './roll.js', './geo.js', './editor.js',
  './compose.js', './compose.css', './engine-mock.js', './pwa.js', './coastlines.json', './catalog.json',
  './manifest.webmanifest', './icons/icon.svg', './icons/icon-192.png', './icons/icon-512.png',
  './pkg/sonder_wasm.js', './pkg/sonder_wasm_bg.wasm',
];

/** Paths (relative to the scope) that must never be cached: the live API + sockets, the
 *  worker itself and the records album (large, changes daily, has its own links). */
const NEVER = [/^api\//, /^ws$/, /^audio$/, /^sw\.js$/, /^records\//, /^\.nojekyll$/];

/** Normalise a path to `./x` form (relative to the scope); '' and '.' become './'. */
export function shellPath(p) {
  let s = String(p).replace(/^(\.\/|\/)+/, '');
  s = s.replace(/^\.$/, '');
  return './' + s;
}

/** Pure: the precache list = SHELL + `extra`, normalised, de-duplicated, sorted, minus
 *  anything in NEVER. `extra` is what build-site.sh found on disk. */
export function precacheList(extra = []) {
  const out = new Set();
  for (const p of [...SHELL, ...extra]) {
    const s = shellPath(p);
    const rel = s.slice(2);
    if (NEVER.some((re) => re.test(rel))) continue;
    out.add(s);
  }
  return [...out].sort();
}

/** Pure: how the service worker should treat a GET for `url` under `scope` (a pathname
 *  ending in `/`): 'bypass' (not ours / live API) · 'shell' (cache-first) · 'network'
 *  (network-first, cache fallback — fonts, feeds we proxy, anything new). */
export function requestKind(url, scope, shell) {
  let u;
  try { u = new URL(url); } catch (_) { return 'bypass'; }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return 'bypass';
  if (typeof scope === 'string' && u.origin !== new URL(scope, url).origin) return 'bypass';
  const scopePath = typeof scope === 'string' ? new URL(scope, url).pathname : '/';
  if (!u.pathname.startsWith(scopePath)) return 'bypass';
  const rel = u.pathname.slice(scopePath.length);
  if (NEVER.some((re) => re.test(rel))) return 'bypass';
  const set = shell instanceof Set ? shell : new Set(shell || []);
  return set.has(shellPath(rel)) ? 'shell' : 'network';
}

export const cacheName = (build) => `sonder-${build || 'dev'}`;

/** Snapshot rate for the worker: 2 Hz when the tab is hidden, 25 Hz when visible. The
 *  engine keeps ticking either way; only the UI traffic drops. */
export const snapshotHz = (hidden) => (hidden ? 2 : 25);

/** Media Session metadata for a scene. */
export function mediaMetadata(scene) {
  return { title: scene || 'Sonder', artist: 'Sonder', album: 'live', artwork: [
    { src: './icons/icon-192.png', sizes: '192x192', type: 'image/png' },
    { src: './icons/icon-512.png', sizes: '512x512', type: 'image/png' },
  ] };
}

/** True when the page runs as an installed app (standalone window / iOS home-screen). */
export function isStandalone(win) {
  try {
    if (win && typeof win.matchMedia === 'function' && win.matchMedia('(display-mode: standalone)').matches) return true;
    return !!(win && win.navigator && win.navigator.standalone === true);
  } catch (_) { return false; }
}

// ---------------------------------------------------------------------------
// Page side (browser only)
// ---------------------------------------------------------------------------
const has = (name) => typeof globalThis !== 'undefined' && name in globalThis;

export async function registerServiceWorker() {
  if (!has('navigator') || !('serviceWorker' in navigator)) return null;
  if (has('location') && location.protocol === 'file:') return null;
  try {
    const reg = await navigator.serviceWorker.register('./sw.js', { type: 'module', scope: './' });
    reg.addEventListener('updatefound', () => console.info('[pwa] update found'));
    return reg;
  } catch (e) {
    console.warn('[pwa] service worker registration failed', e);
    return null;
  }
}

/** `beforeinstallprompt` → show `button`; click → prompt. Hidden again once installed. */
export function setupInstallPrompt(button, win = globalThis) {
  if (!button || !win || typeof win.addEventListener !== 'function') return;
  let deferred = null;
  win.addEventListener('beforeinstallprompt', (e) => {
    e.preventDefault();
    deferred = e;
    button.hidden = isStandalone(win);
  });
  button.addEventListener('click', async () => {
    if (!deferred) return;
    button.disabled = true;
    try { deferred.prompt(); await deferred.userChoice; } catch (_) { /* dismissed */ }
    deferred = null; button.hidden = true; button.disabled = false;
  });
  win.addEventListener('appinstalled', () => { deferred = null; button.hidden = true; });
}

/** Media Session: scene title + play/pause mapped to the audio toggle (lock screens, media keys). */
export const media = {
  title: null, playing: false, onPlay: null, onPause: null,
  bind({ onPlay, onPause }) {
    this.onPlay = onPlay; this.onPause = onPause;
    const ms = has('navigator') && navigator.mediaSession;
    if (!ms) return;
    try {
      ms.setActionHandler('play', () => this.onPlay && this.onPlay());
      ms.setActionHandler('pause', () => this.onPause && this.onPause());
      ms.setActionHandler('stop', () => this.onPause && this.onPause());
    } catch (_) { /* an action is unsupported: fine */ }
  },
  setTitle(title) { if (title !== this.title) { this.title = title; this.apply(); } },
  setPlaying(on) { this.playing = !!on; this.apply(); },
  apply() {
    const ms = has('navigator') && navigator.mediaSession;
    if (!ms) return;
    try {
      if (this.playing || ms.metadata) ms.metadata = new MediaMetadata(mediaMetadata(this.title));
      ms.playbackState = this.playing ? 'playing' : (ms.metadata ? 'paused' : 'none');
    } catch (_) { /* no MediaMetadata */ }
  },
};

/** Hidden tab → ask the worker for 2 Hz snapshots; visible → 25 Hz. Worker mode only (the
 *  native server paces its own snapshots). */
export function watchVisibility(getTransport, doc = has('document') ? document : null) {
  if (!doc || typeof doc.addEventListener !== 'function') return;
  doc.addEventListener('visibilitychange', () => {
    const t = getTransport();
    if (t && t.mode === 'worker' && t.worker) t.worker.postMessage({ type: 'snapshot-rate', hz: snapshotHz(doc.hidden) });
  });
}

/** One call from app.js once the transport is up. Registers the service worker only when
 *  the page is a static site (worker mode): behind the native server there is nothing to
 *  cache and `/api` + the sockets must stay live. */
export function install({ transport, installButton, onPlay, onPause, mock = false }) {
  media.bind({ onPlay, onPause });
  watchVisibility(() => transport);
  if (transport && transport.mode === 'worker' && !mock) {
    registerServiceWorker();
    setupInstallPrompt(installButton);
  }
}
