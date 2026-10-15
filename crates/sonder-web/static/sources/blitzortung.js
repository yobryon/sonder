// `blitzortung.lightning` — live lightning strikes from the Blitzortung.org community
// network (wss://ws{1,7,8}.blitzortung.org/). Mirrors adapters/blitzortung.rs.
//
// Protocol: open the socket, send {"a":111}; every text frame is one strike, obfuscated
// with an LZW-style scheme over UTF-16 code units (decoder ported from
// https://www.gkbrk.com/blitzortung). Decoded JSON: {time (ns), lat, lon, alt, pol, mds,
// mcg, status, region, sig:[{sta, time, lat, lon, alt, status}...], delay (s), lonc, latc}.
//
//   params: bbox [lamin, lomin, lamax, lomax] (optional), max_rate (30 strikes/s cap)
//   event `strike`: lat, lon, lat01, lon01, polarity, stations, delay (ms);
//                   intensity = clamp(stations/40, 0.1, 1)
//   readings (2 s): rate (strikes/s over 10 s), active (distinct 5°×5° cells in 60 s)
//   viz (1 s):      {"strikes":[{lat01, lon01, age_s}]} for the last 5 s
import { Attrs, Connector, Emitter, RateWindow, clamp01, jf, nowSec, paramF64 } from './util.js';

export const HOSTS = ['wss://ws1.blitzortung.org/', 'wss://ws7.blitzortung.org/', 'wss://ws8.blitzortung.org/'];
export const HELLO = '{"a":111}';
export const DEFAULT_MAX_RATE = 30;

/** De-obfuscate one frame (string of UTF-16 code units) into JSON text. */
export function decodeFrame(b) {
  if (!b || !b.length) return '';
  const dict = new Map();
  let c = b[0];
  let f = c;
  const out = [c];
  let next = 256;
  for (let i = 1; i < b.length; i++) {
    const code = b.charCodeAt(i);
    let a;
    if (code < 256) a = b[i];
    else if (dict.has(code)) a = dict.get(code);
    else a = f + c;
    out.push(a);
    c = a[0];
    dict.set(next++, f + c);
    f = a;
  }
  return out.join('');
}

/** Decoded strike JSON → {lat, lon, pol, stations, delayMs, timeNs} or null. */
export function parseStrike(v) {
  const lat = jf(v, 'lat'), lon = jf(v, 'lon');
  if (lat === null || lon === null || Math.abs(lat) > 90 || Math.abs(lon) > 180) return null;
  const sig = Array.isArray(v.sig) ? v.sig.length : null;
  const stations = sig !== null && sig > 0 ? sig : (jf(v, 'mds') ?? 0);
  const delayS = jf(v, 'delay');
  return { lat, lon, pol: jf(v, 'pol') ?? 0, stations, delayMs: delayS === null ? null : delayS * 1000, timeNs: jf(v, 'time') };
}

export const strikeIntensity = (s) => Math.min(1, Math.max(0.1, s.stations / 40));
export const strikeAttrs = (s) => new Attrs()
  .num('lat', s.lat).num('lon', s.lon).num('lat01', clamp01((s.lat + 90) / 180)).num('lon01', clamp01((s.lon + 180) / 360))
  .num('polarity', s.pol).num('stations', s.stations).optNum('delay', s.delayMs).intoMap();

export function parseBbox(spec) {
  const b = spec.params?.bbox;
  if (b === undefined || b === null) return null;
  const arr = typeof b === 'string' ? b.split(',').map((x) => parseFloat(x.trim())) : Array.isArray(b) ? b.map(Number) : null;
  if (!arr || arr.length !== 4 || arr.some((x) => !Number.isFinite(x))) throw new Error(`${spec.kind}: param \`bbox\`: expected [lamin, lomin, lamax, lomax]`);
  const [lamin, lomin, lamax, lomax] = arr;
  if (lamin >= lamax || lomin >= lomax) throw new Error(`${spec.kind}: param \`bbox\`: min must be < max`);
  return arr;
}
export const inBbox = (s, b) => !b || (s.lat >= b[0] && s.lat <= b[2] && s.lon >= b[1] && s.lon <= b[3]);
export const cellOf = (s) => `${Math.floor(s.lat / 5)},${Math.floor(s.lon / 5)}`;

/** Rolling windows for readings and viz. */
export class StrikeStats {
  constructor() { this.rate = new RateWindow(); this.recent = []; }   // recent: [at, lat01, lon01, cell]
  push(at, s) {
    this.rate.push(at);
    this.recent.push([at, clamp01((s.lat + 90) / 180), clamp01((s.lon + 180) / 360), cellOf(s)]);
    while (this.recent.length > 50000) this.recent.shift();
  }
  prune(now) { while (this.recent.length && now - this.recent[0][0] > 60) this.recent.shift(); }
  readings(now) {
    this.prune(now);
    const cells = new Set(); for (const r of this.recent) cells.add(r[3]);
    return { rate: this.rate.rate(now, 10), active: cells.size };
  }
  viz(now) {
    this.prune(now);
    const strikes = [];
    for (const [at, lat01, lon01] of this.recent) if (now - at <= 5) strikes.push({ lat01, lon01, age_s: Math.round((now - at) * 100) / 100 });
    return { strikes };
  }
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.spec = spec;
    this.bbox = parseBbox(spec);
    this.maxRate = Math.max(1, paramF64(spec, 'max_rate', DEFAULT_MAX_RATE));
    this.em = new Emitter(emit, log);
    this.stats = new StrikeStats();
    this.emitted = new RateWindow();
    this.hostIdx = 0;
  }
  start() { this.conn = new Connector(this.em); this.conn.run((signal) => this.session(signal)); }
  stop() { this.conn?.stop(); this.em.close(); }

  session(signal) {
    const url = HOSTS[this.hostIdx % HOSTS.length];
    return new Promise((resolve, reject) => {
      let ws;
      try { ws = new WebSocket(url); } catch (e) { this.hostIdx++; return reject(`connect failed: ${e.message || e}`); }
      let opened = false, gotData = false;
      let lastMsg = performance.now();
      let nextReading = nowSec() + 2, nextViz = nowSec() + 1;
      const finish = (fn, v) => { clearInterval(timer); signal.removeEventListener('abort', onAbort); try { ws.close(); } catch (_) { /* ignore */ } fn(v); };
      const onAbort = () => finish(resolve);
      const fail = (msg) => { if (!gotData) this.hostIdx++; finish(reject, `${msg} [${new URL(url).host}]`); };
      signal.addEventListener('abort', onAbort, { once: true });
      ws.onopen = () => { opened = true; lastMsg = performance.now(); ws.send(HELLO); this.em.status('connecting', 'connected, waiting for strikes'); };
      ws.onerror = () => { /* onclose follows */ };
      ws.onclose = () => fail(opened ? 'server closed' : 'connect failed (network or CORS)');
      ws.onmessage = (ev) => {
        lastMsg = performance.now();
        if (typeof ev.data !== 'string') return;
        let v;
        try { v = JSON.parse(decodeFrame(ev.data)); } catch (_) { return; }
        const s = parseStrike(v);
        if (!s) return;
        gotData = true;
        if (this.conn) this.conn.attempt = 0;
        if (!inBbox(s, this.bbox)) return;
        const now = nowSec();
        this.stats.push(now, s);
        this.em.status('live', 'live');
        if (this.emitted.count(now, 1) >= this.maxRate) return;
        this.emitted.push(now);
        this.em.event('strike', strikeIntensity(s), strikeAttrs(s));
      };
      const timer = setInterval(() => {
        if (!opened && performance.now() - lastMsg > 20000) return fail('connect timeout');
        if (performance.now() - lastMsg > 120000) return fail('no data for 2 min');
        const now = nowSec();
        if (now >= nextViz) { nextViz = now + 1; if (gotData) this.em.viz(this.stats.viz(now)); }
        if (now >= nextReading) {
          nextReading = now + 2;
          if (gotData) {
            const r = this.stats.readings(now);
            this.em.reading(r);
            if (r.rate === 0 && performance.now() - lastMsg > 30000) this.em.status('idle', 'quiet');
          }
        }
      }, 250);
    });
  }
}
