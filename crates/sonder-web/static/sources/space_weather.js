// `noaa.space-weather` — NOAA SWPC solar wind and planetary Kp. Mirrors adapters/space_weather.rs.
import { Attrs, Emitter, Poller, clamp01, getJson, jsonF64, paramSecs } from './util.js';

export const PROPAGATED_URL = 'https://services.swpc.noaa.gov/products/geospace/propagated-solar-wind-1-hour.json';
export const PLASMA_URL = 'https://services.swpc.noaa.gov/json/rtsw/rtsw_wind_1m.json';
export const MAG_URL = 'https://services.swpc.noaa.gov/json/rtsw/rtsw_mag_1m.json';
export const KP_URL = 'https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json';

const f64opt = (v) => (v === null || v === undefined ? null : jsonF64(v));

/** Most recent row with a non-null value for every column, as [[col, value]]. */
export function latestRow(v, columns) {
  if (!Array.isArray(v) || !v.length) return null;
  const first = v[0];
  if (Array.isArray(first)) {
    const header = first.filter((h) => typeof h === 'string');
    const idx = columns.map((c) => header.indexOf(c));
    if (idx.some((i) => i < 0)) return null;
    for (let r = v.length - 1; r >= 1; r--) {
      const row = v[r];
      if (!Array.isArray(row)) return null;
      const vals = idx.map((i) => f64opt(row[i]));
      if (vals.every((x) => x !== null)) return columns.map((c, k) => [c, vals[k]]);
    }
    return null;
  }
  if (first && typeof first === 'object') {
    let best = null;
    for (const row of v) {
      if (!row || typeof row !== 'object' || Array.isArray(row)) return null;
      const vals = columns.map((c) => f64opt(row[c]));
      if (vals.some((x) => x === null)) continue;
      const tag = typeof row.time_tag === 'string' ? row.time_tag : '';
      if (!best || tag >= best[0]) best = [tag, vals];
    }
    return best ? columns.map((c, k) => [c, best[1][k]]) : null;
  }
  return null;
}

export function parsePropagated(v) {
  const out = latestRow(v, ['speed', 'density', 'temperature']);
  if (!out) return null;
  const mag = latestRow(v, ['bz', 'bt']);
  return mag ? out.concat(mag) : out;
}
export function parsePlasma(v) {
  const r = latestRow(v, ['speed', 'density', 'temperature']);
  if (r) return r;
  const p = latestRow(v, ['proton_speed', 'proton_density', 'proton_temperature']);
  return p ? p.map(([k, x]) => [k.replace(/^proton_/, ''), x]) : null;
}
export function parseMag(v) {
  const r = latestRow(v, ['bz_gsm', 'bt']);
  return r ? r.map(([k, x]) => [k === 'bz_gsm' ? 'bz' : k, x]) : null;
}
export function parseKp(v) {
  const r = latestRow(v, ['Kp']) || latestRow(v, ['kp']);
  return r ? r[0][1] : null;
}

async function fetchWind(signal) {
  let primaryErr;
  try {
    const v = await getJson(PROPAGATED_URL, { signal });
    const r = parsePropagated(v);
    if (r) return r;
    primaryErr = 'propagated: no complete row';
  } catch (e) { primaryErr = `propagated: ${e}`; }
  const [plasma, mag] = await Promise.allSettled([getJson(PLASMA_URL, { signal }), getJson(MAG_URL, { signal })]);
  let out = [];
  if (plasma.status === 'fulfilled') { const r = parsePlasma(plasma.value); if (r) out = out.concat(r); }
  if (mag.status === 'fulfilled') { const r = parseMag(mag.value); if (r) out = out.concat(r); }
  if (!out.length) throw `${primaryErr}; rtsw fallback failed too`;
  return out;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.interval = paramSecs(spec, 'interval', 120, 60);
    this.em = new Emitter(emit, log);
    this.prevKp = null;
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const [wind, kp] = await Promise.allSettled([fetchWind(signal), getJson(KP_URL, { signal })]);
    let vals = [];
    const errs = [];
    if (wind.status === 'fulfilled') vals = vals.concat(wind.value); else errs.push(String(wind.reason));
    let kpNow = null;
    if (kp.status === 'fulfilled') {
      const k = parseKp(kp.value);
      if (k === null) errs.push('kp: kp: no rows'); else { kpNow = k; vals.push(['kp', k], ['kp01', clamp01(k / 9)]); }
    } else errs.push(`kp: ${kp.reason}`);
    if (!vals.length) throw errs.join('; ');
    const sp = vals.find(([k]) => k === 'speed');
    if (sp) vals.push(['speed01', clamp01((sp[1] - 250) / 550)]);
    this.em.reading(vals);
    if (this.prevKp !== null && kpNow !== null && Math.abs(kpNow - this.prevKp) >= 0.3) {
      this.em.event('kp_change', clamp01(kpNow / 9), new Attrs().num('kp', kpNow).num('prev', this.prevKp).num('delta', kpNow - this.prevKp));
    }
    if (kpNow !== null) this.prevKp = kpNow;
    if (errs.length) this.em.log(`partial space weather: ${errs.join('; ')}`);
    return 'live';
  }
}
