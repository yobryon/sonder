// Transport between the UI and an engine. Two modes with one event surface:
//   ws     – the native Sonder server: snapshots over /ws, i16 PCM over /audio, commands up /ws
//   worker – no server: worker.js hosts the wasm engine; JS adapters feed it live data
//
//   const t = await connect({ mode });          // mode: 'ws' | 'worker' | undefined (auto)
//   t.on('snapshot', (obj) => …)                // EngineSnapshot
//   t.on('audio', (f32, sampleRate) => …)       // interleaved stereo Float32Array
//   t.on('status', (state) => …)                // 'connecting' | 'live' | 'closed'
//   t.send(userCommand); t.startAudio(); t.stopAudio(); t.close()
// Worker-only extras: 'ready' {scenes, version}, 'started' {seed, specs, mainSpecs},
//   'error' {message}, t.start(sceneToml, seed), t.postSample(source, json), t.postFrame(...)
//
// All URLs are relative so the same files work from the server root and from a
// GitHub Pages sub-path.

export const WS_PROTO = typeof location !== 'undefined' && location.protocol === 'https:' ? 'wss:' : 'ws:';

/** Pure: pick the mode from the ?mode= override and whether the server answered. */
export function chooseMode(param, serverAnswered) {
  if (param === 'ws' || param === 'worker') return param;
  return serverAnswered ? 'ws' : 'worker';
}

/** Does a Sonder server sit behind this page? (`api/snapshot` answers with a snapshot.) */
export async function probeServer(timeoutMs = 1500) {
  try {
    const ctl = new AbortController();
    const t = setTimeout(() => ctl.abort(), timeoutMs);
    const r = await fetch('api/snapshot', { signal: ctl.signal, cache: 'no-store' });
    clearTimeout(t);
    if (!r.ok) return null;
    const snap = await r.json();
    return snap && typeof snap === 'object' && 'scene_name' in snap ? snap : null;
  } catch (_) {
    return null;
  }
}

class Emitter {
  constructor() { this.handlers = new Map(); }
  on(type, fn) { if (!this.handlers.has(type)) this.handlers.set(type, []); this.handlers.get(type).push(fn); return this; }
  emit(type, ...args) { for (const fn of this.handlers.get(type) || []) { try { fn(...args); } catch (e) { console.error(e); } } }
}

// ---------------------------------------------------------------------------
// ws
// ---------------------------------------------------------------------------
export class WsTransport extends Emitter {
  constructor() {
    super();
    this.mode = 'ws';
    this.ws = null;
    this.audioWs = null;
    this.audioOn = false;
    this.backoff = 500;
    this.closed = false;
    this.sampleRate = 48000;
  }
  url(path) { const u = new URL(path, location.href); u.protocol = WS_PROTO; return u.href; }
  open() {
    if (this.closed) return;
    this.emit('status', 'connecting');
    const ws = new WebSocket(this.url('ws'));
    ws.onopen = () => { this.emit('status', 'live'); this.backoff = 500; };
    ws.onmessage = (ev) => { try { this.emit('snapshot', JSON.parse(ev.data)); } catch (e) { console.warn('bad snapshot', e); } };
    ws.onclose = () => {
      this.emit('status', 'closed');
      this.ws = null;
      if (!this.closed) { setTimeout(() => this.open(), this.backoff); this.backoff = Math.min(5000, this.backoff * 1.7); }
    };
    ws.onerror = () => { /* onclose follows */ };
    this.ws = ws;
    return this;
  }
  send(cmd) { if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(cmd)); }
  startAudio() {
    if (this.audioOn) return;
    this.audioOn = true;
    this.openAudio();
  }
  openAudio() {
    const sock = new WebSocket(this.url('audio'));
    sock.binaryType = 'arraybuffer';
    sock.onmessage = (ev) => {
      if (typeof ev.data === 'string') {
        let h;
        try { h = JSON.parse(ev.data); } catch (_) { return; }
        if (h.error) { this.emit('audio-error', h.error); return; }
        this.sampleRate = h.sample_rate || this.sampleRate;
        this.emit('audio-config', { sampleRate: this.sampleRate, channels: h.channels || 2 });
        return;
      }
      if (!this.audioOn) return;
      const i16 = new Int16Array(ev.data);
      const f32 = new Float32Array(i16.length);
      for (let i = 0; i < i16.length; i++) f32[i] = i16[i] / 32768;
      this.emit('audio', f32, this.sampleRate);
    };
    sock.onclose = () => { if (this.audioOn && this.audioWs === sock) setTimeout(() => { if (this.audioOn) this.openAudio(); }, 1000); };
    this.audioWs = sock;
  }
  stopAudio() {
    this.audioOn = false;
    if (this.audioWs) { try { this.audioWs.close(); } catch (_) { /* ignore */ } this.audioWs = null; }
  }
  close() {
    this.closed = true;
    this.stopAudio();
    if (this.ws) { try { this.ws.close(); } catch (_) { /* ignore */ } }
  }
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------
export class WorkerTransport extends Emitter {
  constructor({ engine = './pkg/sonder_wasm.js' } = {}) {
    super();
    this.mode = 'worker';
    this.engineUrl = engine;
    this.worker = null;
    this.ready = false;
    this.started = false;
    this.audioOn = false;
    this.sampleRate = 48000;
    this.pendingValidate = new Map();
    this.validateId = 0;
  }
  open() {
    this.emit('status', 'connecting');
    const w = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
    w.onmessage = (ev) => this.onMessage(ev.data);
    w.onerror = (e) => { this.emit('error', { message: e.message || 'worker error' }); this.emit('status', 'closed'); };
    w.postMessage({ type: 'init', engine: this.engineUrl });
    this.worker = w;
    return this;
  }
  onMessage(m) {
    switch (m.type) {
      case 'ready': this.ready = true; this.scenes = m.scenes; this.version = m.version; this.emit('ready', m); break;
      case 'started': this.started = true; this.sampleRate = m.sampleRate; this.emit('started', m); this.emit('status', 'live'); break;
      case 'snapshot': try { this.emit('snapshot', JSON.parse(m.json)); } catch (e) { console.warn('bad snapshot', e); } break;
      case 'audio': if (this.audioOn) this.emit('audio', m.data, m.sampleRate); break;
      case 'validated': { const r = this.pendingValidate.get(m.id); if (r) { this.pendingValidate.delete(m.id); r(m.errors); } break; }
      case 'error': console.error('[worker]', m.message); this.emit('error', m); this.emit('status', 'closed'); break;
      case 'log': console.debug('[worker]', m.message); break;
      case 'stopped': this.started = false; this.emit('status', 'closed'); break;
      default: break;
    }
  }
  /** Start (or restart) the engine with a scene. `seed` is a decimal string / number. */
  start(sceneToml, seed, sampleRate = 48000) {
    this.sampleRate = sampleRate;
    this.worker.postMessage({ type: 'start', sceneToml, seed: seed === undefined || seed === null ? null : String(seed), sampleRate });
  }
  validate(toml) {
    const id = ++this.validateId;
    return new Promise((res) => { this.pendingValidate.set(id, res); this.worker.postMessage({ type: 'validate', id, toml }); });
  }
  send(cmd) { if (this.worker) this.worker.postMessage({ type: 'command', cmd }); }
  postSample(source, json) { if (this.worker) this.worker.postMessage({ type: 'sample', source, json }); }
  postFrame(source, gray, w, h) { if (this.worker) this.worker.postMessage({ type: 'frame', source, gray, w, h }, [gray.buffer]); }
  startAudio() { this.audioOn = true; this.worker?.postMessage({ type: 'audio', on: true }); this.emit('audio-config', { sampleRate: this.sampleRate, channels: 2 }); }
  stopAudio() { this.audioOn = false; this.worker?.postMessage({ type: 'audio', on: false }); }
  stop() { this.worker?.postMessage({ type: 'stop' }); }
  close() { if (this.worker) { this.worker.terminate(); this.worker = null; } this.emit('status', 'closed'); }
}

/**
 * Build a transport. `mode` undefined → auto-detect (server answers `api/snapshot` → ws).
 * Returns the transport after `open()`; `t.probe` holds the probe snapshot in ws mode.
 */
export async function connect({ mode, engine } = {}) {
  let probe = null;
  if (mode !== 'worker') probe = await probeServer();
  const chosen = chooseMode(mode, !!probe);
  const t = chosen === 'ws' ? new WsTransport() : new WorkerTransport({ engine });
  t.probe = probe;
  return t.open();
}
