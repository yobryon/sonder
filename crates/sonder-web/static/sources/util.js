// Shared plumbing for the browser source adapters: status de-dup, backoff,
// poll/connect loops, sliding-window rates and bounded de-dup sets. Mirrors
// crates/sonder-sources/src/util.rs so adapters emit the same thing natively
// and in the browser.

export const USER_AGENT = 'sonder/0.1 (+https://github.com/genuye/sonder)';
export const HTTP_TIMEOUT_MS = 30000;

export const clamp01 = (x) => (Number.isNaN(x) ? 0 : x < 0 ? 0 : x > 1 ? 1 : x);
export const sanitize = (x) => (Number.isFinite(x) ? x : 0);

/** Monotonic seconds (performance.now based). Adapters use this for their own windows. */
export const nowSec = () => performance.now() / 1000;
export const unixNow = () => Date.now() / 1000;

// ---------------------------------------------------------------------------
// Parameter helpers (spec.params is a plain object)
// ---------------------------------------------------------------------------
export function paramF64(spec, key, def) {
  const v = spec.params?.[key];
  if (v === undefined || v === null) return def;
  if (typeof v === 'number') return v;
  if (typeof v === 'string') { const n = parseFloat(v.trim()); if (Number.isFinite(n)) return n; }
  throw new Error(`${spec.kind}: param \`${key}\`: expected a number`);
}
export function paramSecs(spec, key, def, min) {
  const v = paramF64(spec, key, def);
  if (!Number.isFinite(v) || v < min) throw new Error(`${spec.kind}: param \`${key}\`: expected seconds >= ${min}, got ${v}`);
  return v;
}
export function paramBool(spec, key, def) {
  const v = spec.params?.[key];
  if (v === undefined || v === null) return def;
  if (typeof v === 'boolean') return v;
  if (typeof v === 'number') return v !== 0;
  if (typeof v === 'string') {
    const s = v.trim().toLowerCase();
    if (['true', 'yes', 'on', '1'].includes(s)) return true;
    if (['false', 'no', 'off', '0'].includes(s)) return false;
  }
  throw new Error(`${spec.kind}: param \`${key}\`: expected a boolean`);
}
export function paramStr(spec, key, def) {
  const v = spec.params?.[key];
  return typeof v === 'string' ? v : def;
}
/** A single string, a comma-separated string or an array of strings → string[] (or null if absent). */
export function paramStrList(spec, key) {
  const v = spec.params?.[key];
  if (v === undefined || v === null) return null;
  if (typeof v === 'string') return v.includes(',') ? v.split(',').map((s) => s.trim()).filter(Boolean) : [v];
  if (Array.isArray(v)) return v.filter((x) => typeof x === 'string');
  return null;
}

// ---------------------------------------------------------------------------
// JSON helpers (jf/js/jb in util.rs)
// ---------------------------------------------------------------------------
export function jsonF64(v) {
  if (typeof v === 'number') return Number.isFinite(v) ? v : null;
  if (typeof v === 'string') { const n = parseFloat(v.trim()); return Number.isFinite(n) && /^[-+]?(\d|\.\d)/.test(v.trim()) ? n : null; }
  if (typeof v === 'boolean') return v ? 1 : 0;
  return null;
}
export const jf = (obj, key) => (obj && typeof obj === 'object' ? jsonF64(obj[key]) : null);
export const js = (obj, key) => (obj && typeof obj === 'object' && typeof obj[key] === 'string' ? obj[key] : null);
export function jb(obj, key) {
  if (!obj || typeof obj !== 'object') return null;
  const v = obj[key];
  if (typeof v === 'boolean') return v;
  if (typeof v === 'number') return v !== 0;
  if (v === 'true') return true;
  if (v === 'false') return false;
  return null;
}

// ---------------------------------------------------------------------------
// Backoff: 1 s · 2^attempt, capped at 5 min, ±25 % jitter (retry_delay in util.rs)
// ---------------------------------------------------------------------------
export function retryDelaySec(attempt, rand = Math.random) {
  const base = Math.pow(2, Math.min(attempt, 12));
  const capped = Math.min(base, 300);
  const jitter = 0.75 + rand() * 0.5;
  return Math.min(360, Math.max(0.5, capped * jitter));
}

// ---------------------------------------------------------------------------
// Attrs builder (drops non-finite numbers like util.rs::Attrs)
// ---------------------------------------------------------------------------
export class Attrs {
  constructor() { this.map = {}; }
  num(k, v) { if (typeof v === 'number' && Number.isFinite(v)) this.map[k] = v; return this; }
  optNum(k, v) { return v === null || v === undefined ? this : this.num(k, v); }
  text(k, v) { this.map[k] = String(v); return this; }
  optText(k, v) { return v === null || v === undefined ? this : this.text(k, v); }
  bool(k, v) { this.map[k] = !!v; return this; }
  intoMap() { return this.map; }
}

// ---------------------------------------------------------------------------
// Emitter: builds sample JSON and de-duplicates status transitions
// ---------------------------------------------------------------------------
export class Emitter {
  /** @param {(json: string) => void} send  @param {(msg: string) => void} [log] */
  constructor(send, log) {
    this.send = send;
    this.log = log || (() => {});
    this.lastState = null;
    this.closed = false;
  }
  emit(obj) { if (!this.closed) this.send(JSON.stringify(obj)); }
  status(state, message) {
    if (this.lastState === state) return;
    this.lastState = state;
    this.emit({ type: 'status', state, message: String(message ?? '') });
  }
  statusForce(state, message) { this.lastState = null; this.status(state, message); }
  /** values: object or array of [k, v]; non-finite values are dropped; empty → nothing. */
  reading(values) {
    const out = {};
    const entries = Array.isArray(values) ? values : Object.entries(values);
    for (const [k, v] of entries) if (typeof v === 'number' && Number.isFinite(v)) out[k] = v;
    if (Object.keys(out).length === 0) return;
    this.emit({ type: 'reading', values: out });
  }
  event(name, intensity, attrs) {
    this.emit({ type: 'event', name, intensity: sanitize(intensity), attrs: attrs instanceof Attrs ? attrs.intoMap() : (attrs || {}) });
  }
  viz(payload) { this.emit({ type: 'viz', payload }); }
  /** An already-built datum ({type:'reading'|'event'|...}). */
  datum(d) { this.emit(d); }
  close() { this.closed = true; }
}

// ---------------------------------------------------------------------------
// Sliding windows and de-dup
// ---------------------------------------------------------------------------
export class RateWindow {
  constructor() { this.times = []; }
  push(at) { this.times.push(at); if (this.times.length > 100000) this.times.shift(); }
  prune(now, window) {
    let i = 0;
    while (i < this.times.length && now - this.times[i] > window) i++;
    if (i) this.times.splice(0, i);
  }
  count(now, window) { this.prune(now, window); return this.times.length; }
  rate(now, window) { return window <= 0 ? 0 : this.count(now, window) / window; }
}

export class SeenSet {
  constructor(cap) { this.cap = Math.max(1, cap); this.set = new Set(); this.order = []; }
  /** true if not seen before (and records it). */
  insert(id) {
    if (this.set.has(id)) return false;
    this.set.add(id);
    this.order.push(id);
    while (this.order.length > this.cap) this.set.delete(this.order.shift());
    return true;
  }
  has(id) { return this.set.has(id); }
  get size() { return this.set.size; }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------
/** Describe a fetch failure in one line, calling out CORS/network. */
export function describeFetchError(e, url) {
  if (e && e.name === 'AbortError') return 'timeout';
  const host = (() => { try { return new URL(url).host; } catch (_) { return url; } })();
  return `request failed (network or CORS blocked by ${host})`;
}

/** GET url and parse JSON. Rejects with a one-line string suitable for a status message. */
export async function getJson(url, { headers = {}, signal, timeoutMs = HTTP_TIMEOUT_MS } = {}) {
  const text = await getText(url, { headers, signal, timeoutMs });
  try { return JSON.parse(text); } catch (e) { throw `invalid JSON from ${url}: ${e.message}`; }
}

export async function getText(url, { headers = {}, signal, timeoutMs = HTTP_TIMEOUT_MS } = {}) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  const onAbort = () => ctl.abort();
  if (signal) { if (signal.aborted) ctl.abort(); else signal.addEventListener('abort', onAbort, { once: true }); }
  try {
    let resp;
    try {
      resp = await fetch(url, { headers: { Accept: 'application/json, text/plain, */*', ...headers }, signal: ctl.signal, cache: 'no-store' });
    } catch (e) {
      throw describeFetchError(e, url);
    }
    if (!resp.ok) throw `HTTP ${resp.status} from ${url}`;
    try { return await resp.text(); } catch (e) { throw `body read failed: ${e.message || e}`; }
  } finally {
    clearTimeout(timer);
    if (signal) signal.removeEventListener('abort', onAbort);
  }
}

// ---------------------------------------------------------------------------
// Poll loop (util.rs::Poller): `pollOnce()` resolves 'live' | 'idle', rejects with a message.
// ---------------------------------------------------------------------------
export class Poller {
  constructor(em, intervalSec) { this.em = em; this.interval = intervalSec; this.attempt = 0; this.stopped = false; this.timer = null; this.abort = new AbortController(); }
  get signal() { return this.abort.signal; }
  async run(pollOnce) {
    this.em.status('connecting', 'connecting');
    while (!this.stopped) {
      let delay;
      try {
        const r = await pollOnce(this.signal);
        if (this.stopped) return;
        this.attempt = 0;
        if (r === 'idle') this.em.status('idle', 'no new data'); else this.em.status('live', 'live');
        delay = this.interval;
      } catch (e) {
        if (this.stopped) return;
        const msg = typeof e === 'string' ? e : (e && e.message) || String(e);
        delay = Math.max(retryDelaySec(this.attempt), Math.min(this.interval, 5));
        this.attempt++;
        this.em.log(`poll failed (attempt ${this.attempt}): ${msg}`);
        this.em.statusForce('reconnecting', `${msg}; retry in ${delay.toFixed(0)}s`);
      }
      await this.sleep(delay);
    }
  }
  sleep(sec) { return new Promise((res) => { if (this.stopped) return res(); this.timer = setTimeout(res, sec * 1000); this.wake = res; }); }
  stop() { this.stopped = true; this.abort.abort(); if (this.timer) clearTimeout(this.timer); if (this.wake) this.wake(); }
}

// ---------------------------------------------------------------------------
// Connect loop for long-lived connections (SSE, WebSocket). `session(signal)` resolves
// on a clean end, rejects with a message. A session that lived > 60 s resets backoff.
// ---------------------------------------------------------------------------
export class Connector {
  constructor(em) { this.em = em; this.attempt = 0; this.stopped = false; this.abort = null; this.timer = null; this.wake = null; }
  async run(session) {
    while (!this.stopped) {
      this.em.statusForce('connecting', 'connecting');
      this.abort = new AbortController();
      const started = performance.now();
      let msg;
      try { await session(this.abort.signal); msg = 'connection closed'; }
      catch (e) { msg = typeof e === 'string' ? e : (e && e.message) || String(e); }
      if (this.stopped) return;
      if (performance.now() - started > 60000) this.attempt = 0;
      const d = retryDelaySec(this.attempt);
      this.attempt++;
      this.em.statusForce('reconnecting', `${msg}; reconnect in ${d.toFixed(0)}s`);
      await new Promise((res) => { this.wake = res; this.timer = setTimeout(res, d * 1000); });
    }
  }
  stop() { this.stopped = true; if (this.abort) this.abort.abort(); if (this.timer) clearTimeout(this.timer); if (this.wake) this.wake(); }
}

/** A promise that resolves after ms, or immediately when the signal aborts. */
export function sleep(ms, signal) {
  return new Promise((res) => {
    if (signal?.aborted) return res();
    const t = setTimeout(res, ms);
    signal?.addEventListener('abort', () => { clearTimeout(t); res(); }, { once: true });
  });
}

// ---------------------------------------------------------------------------
// SSE framing (sse.rs::SseParser): feed text, get {event,id,data,retry} objects.
// ---------------------------------------------------------------------------
export class SseParser {
  constructor() { this.buf = ''; this.reset(); this.lastId = null; }
  reset() { this.curEvent = null; this.curId = null; this.curData = []; this.curRetry = null; }
  push(text) {
    this.buf += text;
    if (this.buf.length > 16 * 1024 * 1024) { this.buf = ''; this.reset(); }
    const out = [];
    let nl;
    while ((nl = this.buf.indexOf('\n')) >= 0) {
      let line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.endsWith('\r')) line = line.slice(0, -1);
      const ev = this.feedLine(line);
      if (ev) out.push(ev);
    }
    return out;
  }
  feedLine(line) {
    if (line === '') {
      if (this.curData.length === 0 && this.curEvent === null && this.curId === null && this.curRetry === null) return null;
      if (this.curData.length === 0) { this.reset(); return null; }
      const ev = { event: this.curEvent, id: this.curId, data: this.curData.join('\n'), retry_ms: this.curRetry };
      this.reset();
      return ev;
    }
    if (line.startsWith(':')) return null;
    let field = line, value = '';
    const c = line.indexOf(':');
    if (c >= 0) { field = line.slice(0, c); value = line.slice(c + 1); if (value.startsWith(' ')) value = value.slice(1); }
    switch (field) {
      case 'data': this.curData.push(value); break;
      case 'event': this.curEvent = value; break;
      case 'id': if (!value.includes('\0')) { this.curId = value; this.lastId = value; } break;
      case 'retry': { const n = parseInt(value, 10); if (Number.isFinite(n)) this.curRetry = n; break; }
      default: break;
    }
    return null;
  }
}

/** Read an SSE stream with fetch (allows custom headers); calls onEvent for each event. */
export async function streamSse(url, { headers = {}, lastId = null, signal, onOpen, onEvent, idleTimeoutMs = 0 } = {}) {
  const h = { Accept: 'text/event-stream', ...headers };
  if (lastId) h['Last-Event-ID'] = lastId;
  const connectCtl = new AbortController();
  const connectTimer = setTimeout(() => connectCtl.abort(), 20000);
  const onAbort = () => connectCtl.abort();
  signal?.addEventListener('abort', onAbort, { once: true });
  let resp;
  try {
    resp = await fetch(url, { headers: h, signal: connectCtl.signal, cache: 'no-store' });
  } catch (e) {
    throw connectCtl.signal.aborted && !signal?.aborted ? 'connect timeout' : describeFetchError(e, url);
  } finally { clearTimeout(connectTimer); }
  if (!resp.ok) throw `HTTP ${resp.status}`;
  if (!resp.body) throw 'no response body';
  onOpen?.();
  const reader = resp.body.getReader();
  const dec = new TextDecoder();
  const parser = new SseParser();
  let idleTimer = null;
  const abortRead = () => reader.cancel().catch(() => {});
  signal?.addEventListener('abort', abortRead, { once: true });
  try {
    for (;;) {
      let chunk;
      if (idleTimeoutMs > 0) {
        chunk = await Promise.race([
          reader.read(),
          new Promise((_, rej) => { idleTimer = setTimeout(() => rej(`no data for ${Math.round(idleTimeoutMs / 1000)} s`), idleTimeoutMs); }),
        ]);
        clearTimeout(idleTimer);
      } else {
        chunk = await reader.read();
      }
      if (chunk.done) return;
      for (const ev of parser.push(dec.decode(chunk.value, { stream: true }))) onEvent(ev);
    }
  } catch (e) {
    if (signal?.aborted) return;
    throw typeof e === 'string' ? e : `stream error: ${e.message || e}`;
  } finally {
    clearTimeout(idleTimer);
    signal?.removeEventListener('abort', abortRead);
    abortRead();
  }
}
