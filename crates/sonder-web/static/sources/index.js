// Registry of browser source adapters: scene `kind` → lazy module import.
// Each module default-exports `class Adapter { constructor(spec, emit, log); start(); stop() }`.
// `main: true` kinds need the DOM / user media and run on the main thread (app.js
// forwards their samples to the worker); everything else runs inside worker.js.

export const ADAPTERS = {
  'usgs.earthquakes': { load: () => import('./usgs.js') },
  'open-meteo.weather': { load: () => import('./open_meteo.js') },
  'wikipedia.recent-changes': { load: () => import('./wikipedia.js') },
  'coinbase.trades': { load: () => import('./coinbase.js') },
  'opensky.flights': { load: () => import('./opensky.js') },
  'noaa.space-weather': { load: () => import('./space_weather.js') },
  'noaa.tides': { load: () => import('./tides.js') },
  'hackernews.firehose': { load: () => import('./hackernews.js') },
  'github.events': { load: () => import('./github.js') },
  'iss.position': { load: () => import('./iss.js') },
  'http-json': { load: () => import('./http_json.js') },
  'sse': { load: () => import('./sse.js') },
  'websocket': { load: () => import('./websocket.js') },
  'mqtt': { load: () => import('./mqtt.js') },
  'blitzortung.lightning': { load: () => import('./blitzortung.js') },
  'audio.features': { load: () => import('./audio.js'), main: true },
  'browser.input': { load: () => import('./browser_input.js'), main: true },
  'camera.webcam': { load: () => import('./camera.js'), main: true },
  'camera.mjpeg': { load: () => import('./camera.js'), main: true },
};

/** Kinds that exist natively but cannot run in a browser, with the message shown in the UI. */
export const UNAVAILABLE = {
  'system.metrics': 'not available in the browser (no access to CPU/memory/network); try browser.input',
  'file-tail': 'not available in the browser (no file system access)',
  'stdin': 'not available in the browser (no stdin)',
};

export const isMainThreadKind = (kind) => !!ADAPTERS[kind]?.main;
export const isCameraKind = (kind) => typeof kind === 'string' && kind.startsWith('camera.');
export const isAudioKind = (kind) => kind === 'audio.features';

/** Resolve the adapter class for a kind, or null if unknown. */
export async function loadAdapter(kind) {
  const entry = ADAPTERS[kind];
  if (!entry) return null;
  const mod = await entry.load();
  return mod.default;
}
