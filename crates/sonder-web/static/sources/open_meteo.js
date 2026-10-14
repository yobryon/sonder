// `open-meteo.weather` — current weather for a lat/lon from Open-Meteo. Mirrors adapters/open_meteo.rs.
import { Attrs, Emitter, Poller, clamp01, getJson, paramF64, paramSecs } from './util.js';

export const FIELDS = ['temperature_2m', 'relative_humidity_2m', 'precipitation', 'cloud_cover', 'wind_speed_10m',
  'wind_direction_10m', 'wind_gusts_10m', 'pressure_msl', 'is_day', 'weather_code'];

export const url = (lat, lon) =>
  `https://api.open-meteo.com/v1/forecast?latitude=${lat}&longitude=${lon}&current=${FIELDS.join(',')}&wind_speed_unit=ms&timezone=UTC`;

/** `current` block → [[name, value]] plus normalized convenience channels. */
export function parseCurrent(v) {
  const cur = v?.current;
  if (!cur || typeof cur !== 'object') return [];
  const out = [];
  for (const [k, x] of Object.entries(cur)) {
    if (k === 'time' || k === 'interval') continue;
    if (typeof x === 'number' && Number.isFinite(x)) out.push([k, x]);
  }
  const get = (k) => { const e = out.find(([n]) => n === k); return e ? e[1] : null; };
  const extra = [
    ['cloud01', get('cloud_cover') === null ? null : clamp01(get('cloud_cover') / 100)],
    ['humidity01', get('relative_humidity_2m') === null ? null : clamp01(get('relative_humidity_2m') / 100)],
    ['wind01', get('wind_speed_10m') === null ? null : clamp01(get('wind_speed_10m') / 25)],
    ['temp01', get('temperature_2m') === null ? null : clamp01((get('temperature_2m') + 10) / 50)],
  ];
  for (const [k, x] of extra) if (x !== null) out.push([k, x]);
  return out;
}

export default class Adapter {
  constructor(spec, emit, log) {
    const lat = paramF64(spec, 'lat', 51.5), lon = paramF64(spec, 'lon', -0.12);
    if (!(lat >= -90 && lat <= 90)) throw new Error(`open-meteo.weather: param \`lat\`: expected -90..=90, got ${lat}`);
    if (!(lon >= -180 && lon <= 180)) throw new Error(`open-meteo.weather: param \`lon\`: expected -180..=180, got ${lon}`);
    this.url = url(lat, lon);
    this.interval = paramSecs(spec, 'interval', 300, 60);
    this.em = new Emitter(emit, log);
    this.prevGust = null;
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(this.url, { signal });
    const vals = parseCurrent(v);
    if (!vals.length) throw `no \`current\` block: ${typeof v?.reason === 'string' ? v.reason : '?'}`;
    const g = vals.find(([k]) => k === 'wind_gusts_10m');
    const gust = g ? g[1] : null;
    this.em.reading(vals);
    if (this.prevGust !== null && gust !== null && gust - this.prevGust > 3) {
      this.em.event('gust', clamp01((gust - this.prevGust) / 15), new Attrs().num('gust', gust).num('prev', this.prevGust));
    }
    if (gust !== null) this.prevGust = gust;
    return 'live';
  }
}
