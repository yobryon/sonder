// `noaa.tides` — latest water level at a NOAA CO-OPS station. Mirrors adapters/tides.rs.
import { Emitter, Poller, clamp01, getJson, jf, js, nowSec, paramSecs } from './util.js';

export const url = (station) =>
  `https://api.tidesandcurrents.noaa.gov/api/prod/datagetter?product=water_level&date=latest&station=${station}&datum=MLLW&units=metric&time_zone=gmt&format=json`;

/** `{"data":[{"t":"2024-01-01 00:00","v":"1.234"}]}` → [level m, timestamp]; throws a message. */
export function parseLevel(v) {
  const err = v?.error && js(v.error, 'message');
  if (err) throw `NOAA: ${err}`;
  const data = v?.data;
  if (!Array.isArray(data)) throw 'no data array';
  if (!data.length) throw 'empty data';
  const last = data[data.length - 1];
  const level = jf(last, 'v');
  if (level === null) throw 'no `v` field';
  return [level, js(last, 't') ?? ''];
}

export const trend = (prev, cur, dt) => (dt <= 0 ? 0 : (cur - prev) / dt * 3600);

export function parseStation(spec) {
  const s = spec.params?.station;
  let station;
  if (s === undefined || s === null) station = '9414290';
  else if (typeof s === 'string') station = s.trim();
  else if (typeof s === 'number') station = String(s);
  else throw new Error('noaa.tides: param `station`: expected a station id');
  if (!station || !/^[A-Za-z0-9]+$/.test(station)) throw new Error('noaa.tides: param `station`: expected an alphanumeric station id');
  return station;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.url = url(parseStation(spec));
    this.interval = paramSecs(spec, 'interval', 120, 60);
    this.em = new Emitter(emit, log);
    this.prev = null; // [level, stamp, at]
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(this.url, { signal });
    const [level, stamp] = parseLevel(v);
    const now = nowSec();
    const vals = [['water_level', level], ['water_level01', clamp01((level + 1) / 4)]];
    let fresh = true;
    if (this.prev) {
      if (this.prev[1] === stamp) fresh = false; else vals.push(['trend', trend(this.prev[0], level, now - this.prev[2])]);
    }
    this.em.reading(vals);
    if (fresh) this.prev = [level, stamp, now];
    return fresh ? 'live' : 'idle';
  }
}
