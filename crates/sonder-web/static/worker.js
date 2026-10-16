// Sonder engine Worker (module worker). Hosts the wasm engine (or the dev mock), runs
// the browser source adapters, owns the clock and streams rendered audio + snapshots
// to the main thread.
//
// In:  {type:'init', engine:'./pkg/sonder_wasm.js'|'./engine-mock.js'}
//      {type:'start', sceneToml, seed, sampleRate, feed}   feed:'room' = no adapters/sims; samples arrive via 'sample'
//      {type:'command', cmd}                      UserCommand JSON object
//      {type:'sample', source, json}              from main-thread adapters (browser.input)
//      {type:'frame', source, gray, w, h}         webcam luma frame (transferable)
//      {type:'audio', on}                         post rendered blocks or not
//      {type:'snapshot-rate', hz}                 snapshots per second (2 when the tab is hidden)
//      {type:'validate', id, toml}
//      {type:'load_log', bytes, title}            a .sonder data log (plain JSONL bytes, transferable):
//                                                 switches the engine to replay mode (adapters stop)
//      {type:'stop'}
// Out: {type:'ready', scenes, version}  {type:'started', scene, seed, specs, mainSpecs}
//      {type:'audio', data:Float32Array, sampleRate}  {type:'snapshot', json}
//      {type:'validated', id, errors}  {type:'error', message}  {type:'log', message}  {type:'stopped'}
//      {type:'log_loaded', info}       info = {title, scene, seed, length, sources, fed, audio_dropped, …}

import { planTicks } from './timing.js';
import { ADAPTERS, UNAVAILABLE, isMainThreadKind, loadAdapter } from './sources/index.js';

let mod = null;            // the engine module (wasm or mock)
let engine = null;
let adapters = [];         // running in-worker adapters
let sampleRate = 48000;
let audioOn = false;
let timer = null;
let t0 = 0;                // performance.now() at start, shifted forward by skips
let tickCount = 0;
let tickSec = 0.02;
let framesPerTick = 960;
let running = false;
let snapshotEvery = 2;     // ticks per snapshot: 25 Hz visible, ~2 Hz when the tab is hidden

const post = (m, transfer) => self.postMessage(m, transfer || []);
const log = (message) => post({ type: 'log', message });
const fail = (e) => post({ type: 'error', message: e && e.message ? e.message : String(e) });

self.onmessage = async (ev) => {
  const m = ev.data;
  try {
    switch (m.type) {
      case 'init': await init(m.engine); break;
      case 'start': await start(m); break;
      case 'command': if (engine) engine.command(JSON.stringify(m.cmd)); break;
      case 'sample': if (engine && running) pushSample(m.source, m.json); break;
      case 'frame': if (engine && running) engine.push_frame(m.source, m.gray, m.w, m.h); break;
      case 'audio': audioOn = !!m.on; break;
      case 'snapshot-rate': snapshotEvery = Math.max(1, Math.round(1 / (Math.max(0.1, +m.hz || 25) * tickSec))); break;
      case 'validate': post({ type: 'validated', id: m.id, errors: mod ? Array.from(mod.validate_scene(m.toml)) : ['engine not loaded'] }); break;
      case 'load_log': loadLog(m.bytes, m.title || 'log'); break;
      case 'stop': stop(); post({ type: 'stopped' }); break;
      default: break;
    }
  } catch (e) {
    fail(e);
  }
};

async function init(engineUrl) {
  mod = await import(engineUrl);
  if (typeof mod.default === 'function') await mod.default();
  const scenes = Array.from(mod.builtin_scenes()).map((s) => ({
    name: s.name, title: s.title, description: s.description, tags: Array.from(s.tags || []), toml: s.toml,
  }));
  post({ type: 'ready', scenes, version: typeof mod.version === 'function' ? mod.version() : '?' });
}

async function start({ sceneToml, seed, sampleRate: sr, feed }) {
  if (!mod) throw new Error('engine not initialised');
  stop();
  sampleRate = sr || 48000;
  const seedBig = BigInt(String(seed ?? Math.floor(Math.random() * 2 ** 52)));
  engine = new mod.Engine(sceneToml, seedBig, sampleRate);
  framesPerTick = engine.frames_per_tick();
  tickSec = framesPerTick / sampleRate;
  const specs = Array.from(engine.source_specs()).map((s) => ({ name: s.name, kind: s.kind, params: plain(s.params), builtin: !!s.builtin }));
  const mainSpecs = [];
  // A room guest is fed the host's samples: no adapters here, no built-in sims.
  const fed = feed === 'room';
  if (fed && typeof engine.feed_externally === 'function') engine.feed_externally();
  for (const spec of specs) {
    if (fed || spec.builtin) continue;
    if (isMainThreadKind(spec.kind)) { mainSpecs.push(spec); continue; }
    if (UNAVAILABLE[spec.kind]) { pushSample(spec.name, JSON.stringify({ type: 'status', state: 'failed', message: UNAVAILABLE[spec.kind] })); continue; }
    if (!ADAPTERS[spec.kind]) { pushSample(spec.name, JSON.stringify({ type: 'status', state: 'failed', message: `no browser adapter for kind \`${spec.kind}\`` })); continue; }
    try {
      const Adapter = await loadAdapter(spec.kind);
      const a = new Adapter(spec, (json) => pushSample(spec.name, json), (msg) => log(`[${spec.name}] ${msg}`));
      adapters.push(a);
    } catch (e) {
      pushSample(spec.name, JSON.stringify({ type: 'status', state: 'failed', message: e && e.message ? e.message : String(e) }));
    }
  }
  running = true;
  t0 = performance.now();
  tickCount = 0;
  post({ type: 'started', seed: seedBig.toString(), sampleRate, specs, mainSpecs, framesPerTick });
  for (const a of adapters) { try { a.start(); } catch (e) { log(`adapter start failed: ${e.message || e}`); } }
  loop();
}

// Time travel: hand a data log to the engine. The adapters are stopped (the engine
// ignores their samples anyway) and the page learns what the log carries.
function loadLog(bytes, title) {
  if (!engine || !running) throw new Error('start a scene before loading a log');
  if (typeof engine.load_log !== 'function') throw new Error('this engine build cannot replay logs');
  const info = plain(engine.load_log(bytes, title));
  for (const a of adapters) { try { a.stop(); } catch (_) { /* ignore */ } }
  adapters = [];
  post({ type: 'log_loaded', info: { ...info, sources: Array.from(info.sources || []), fed: Array.from(info.fed || []), audio_dropped: Array.from(info.audio_dropped || []) } });
}

function pushSample(source, json) {
  try { engine.push_sample(source, json); } catch (e) { log(`push_sample(${source}) rejected: ${e.message || e}`); }
}

function plain(v) {
  // wasm-bindgen may hand us a Map or a plain object for `params`.
  if (v instanceof Map) return Object.fromEntries(v);
  return v && typeof v === 'object' ? v : {};
}

function loop() {
  if (!running) return;
  const elapsed = (performance.now() - t0) / 1000;
  const { ticks, skip, nextMs } = planTicks({ engineTime: engine.time(), elapsed, tickSec });
  if (skip > 0) t0 += skip * 1000;
  for (let i = 0; i < ticks; i++) {
    const out = new Float32Array(2 * framesPerTick);   // fresh buffer: it is transferred away
    engine.tick(out);
    tickCount++;
    if (audioOn) post({ type: 'audio', data: out, sampleRate }, [out.buffer]);
    if (tickCount % snapshotEvery === 0) post({ type: 'snapshot', json: engine.snapshot_json() });
  }
  timer = setTimeout(loop, nextMs);
}

function stop() {
  running = false;
  if (timer) { clearTimeout(timer); timer = null; }
  for (const a of adapters) { try { a.stop(); } catch (_) { /* ignore */ } }
  adapters = [];
  if (engine) { try { engine.free(); } catch (_) { /* ignore */ } engine = null; }
}
