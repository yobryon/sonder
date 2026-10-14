// `iss.position` — where the ISS is right now. Mirrors adapters/iss.rs, but polls
// https://api.wheretheiss.at (open-notify is http-only and would be blocked on an https page).
import { Attrs, Emitter, Poller, clamp01, getJson, jf, paramSecs } from './util.js';

export const URL = 'https://api.wheretheiss.at/v1/satellites/25544';

/** Accepts both wheretheiss (`{latitude, longitude}`) and open-notify (`{iss_position:{...}}`) shapes. */
export function parsePosition(v) {
  const p = v?.iss_position && typeof v.iss_position === 'object' ? v.iss_position : v;
  const lat = jf(p, 'latitude'), lon = jf(p, 'longitude');
  return lat === null || lon === null ? null : [lat, lon];
}

export const readings = (lat, lon) => [
  ['lat', lat], ['lon', lon], ['lat01', clamp01((lat + 90) / 180)], ['lon01', clamp01((lon + 180) / 360)], ['northbound', 0],
];

export default class Adapter {
  constructor(spec, emit, log) {
    this.interval = paramSecs(spec, 'interval', 5, 2);
    this.em = new Emitter(emit, log);
    this.prev = null;
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(URL, { signal });
    const pos = parsePosition(v);
    if (!pos) throw 'unexpected payload';
    const [lat, lon] = pos;
    const vals = readings(lat, lon);
    if (this.prev) {
      const [plat, plon] = this.prev;
      vals.find(([k]) => k === 'northbound')[1] = lat > plat ? 1 : 0;
      const dlat = lat - plat;
      const dlon = ((((lon - plon + 540) % 360) + 360) % 360) - 180;
      vals.push(['speed_deg_s', Math.sqrt(dlat * dlat + dlon * dlon) / this.interval]);
      if ((plat < 0) !== (lat < 0)) {
        this.em.event('equator_cross', 1.0, new Attrs().num('lon', lon).num('lon01', clamp01((lon + 180) / 360)).bool('northbound', lat > plat));
      }
    }
    this.em.reading(vals);
    this.prev = [lat, lon];
    return 'live';
  }
}
