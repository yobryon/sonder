// `opensky.flights` — aircraft state vectors within a bounding box. Mirrors adapters/opensky.rs.
// OpenSky's anonymous API may not send CORS headers; a blocked request surfaces as a
// clear `reconnecting`/`failed` status rather than an exception.
import { Attrs, Emitter, Poller, SeenSet, clamp01, getJson, paramSecs } from './util.js';

export const DEFAULT_BBOX = [50.5, -1.5, 52.0, 1.0];
export const url = (b) => `https://opensky-network.org/api/states/all?lamin=${b[0]}&lomin=${b[1]}&lamax=${b[2]}&lomax=${b[3]}`;

const num = (x) => (typeof x === 'number' && Number.isFinite(x) ? x : null);

export function parseStates(v) {
  const states = Array.isArray(v?.states) ? v.states : [];
  const out = [];
  for (const a of states) {
    if (!Array.isArray(a) || typeof a[0] !== 'string') continue;
    out.push({
      icao24: a[0], callsign: (typeof a[1] === 'string' ? a[1] : '').trim(),
      lon: num(a[5]), lat: num(a[6]), alt: num(a[7]) ?? num(a[13]), on_ground: a[8] === true,
      speed: num(a[9]), heading: num(a[10]), vertical_rate: num(a[11]),
    });
  }
  return out;
}

export function summarize(list) {
  const airborne = list.filter((a) => !a.on_ground);
  const mean = (f) => { const vals = airborne.map(f).filter((x) => x !== null); return vals.length ? vals.reduce((s, x) => s + x, 0) / vals.length : 0; };
  const climbing = airborne.length ? airborne.filter((a) => (a.vertical_rate ?? 0) > 0.5).length / airborne.length : 0;
  return [['count', list.length], ['airborne', airborne.length], ['mean_alt', mean((a) => a.alt)], ['mean_speed', mean((a) => a.speed)], ['climbing', climbing]];
}

export function aircraftIntensity(a) {
  const alt = clamp01((a.alt ?? 0) / 12000), spd = clamp01((a.speed ?? 0) / 280);
  return clamp01(0.1 + 0.5 * alt + 0.4 * spd);
}
export function aircraftAttrs(a, bbox) {
  const lat01 = a.lat === null ? null : clamp01((a.lat - bbox[0]) / Math.max(bbox[2] - bbox[0], 1e-6));
  const lon01 = a.lon === null ? null : clamp01((a.lon - bbox[1]) / Math.max(bbox[3] - bbox[1], 1e-6));
  return new Attrs().text('icao24', a.icao24).text('callsign', a.callsign)
    .optNum('alt', a.alt).optNum('speed', a.speed).optNum('heading', a.heading).optNum('vertical_rate', a.vertical_rate)
    .optNum('lat', a.lat).optNum('lon', a.lon).optNum('lat01', lat01).optNum('lon01', lon01).bool('on_ground', a.on_ground).intoMap();
}

export function parseBbox(spec) {
  const b = spec.params?.bbox;
  if (b === undefined || b === null) return DEFAULT_BBOX;
  if (!Array.isArray(b) || b.length !== 4 || !b.every((x) => typeof x === 'number')) throw new Error('opensky.flights: param `bbox`: expected [lamin, lomin, lamax, lomax]');
  if (b[0] >= b[2] || b[1] >= b[3]) throw new Error('opensky.flights: param `bbox`: expected [lamin, lomin, lamax, lomax] with min < max');
  return b;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.bbox = parseBbox(spec);
    this.url = url(this.bbox);
    this.interval = paramSecs(spec, 'interval', 15, 10);
    this.em = new Emitter(emit, log);
    this.seen = new SeenSet(20000);
    this.first = true;
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(this.url, { signal });
    if (!v || (v.states === undefined && v.time === undefined)) throw 'unexpected payload';
    const list = parseStates(v);
    this.em.reading(summarize(list));
    let fresh = 0;
    for (const a of list) {
      if (this.seen.insert(a.icao24) && !this.first) { fresh++; this.em.event('aircraft', aircraftIntensity(a), aircraftAttrs(a, this.bbox)); }
    }
    this.first = false;
    return fresh > 0 || list.length ? 'live' : 'idle';
  }
}
