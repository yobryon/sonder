// `usgs.earthquakes` — USGS GeoJSON summary feeds, polled. Mirrors adapters/usgs.rs.
import { Attrs, Emitter, Poller, SeenSet, clamp01, getJson, jf, js, paramF64, paramSecs, paramStr, unixNow } from './util.js';

export const FEEDS = ['all_hour', 'all_day', 'all_week', '1.0_hour', '1.0_day', '1.0_week', '2.5_hour', '2.5_day', '2.5_week',
  '4.5_hour', '4.5_day', '4.5_week', 'significant_hour', 'significant_day', 'significant_week'];

export const feedUrl = (feed) => `https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/${feed}.geojson`;

export const quakeIntensity = (q) => clamp01((q.mag - 1.0) / 7.0);
export const quakeAttrs = (q) => new Attrs()
  .num('mag', q.mag).num('depth_km', q.depth_km).num('lat', q.lat).num('lon', q.lon)
  .text('place', q.place)
  .num('lon01', clamp01((q.lon + 180) / 360)).num('lat01', clamp01((q.lat + 90) / 180))
  .text('id', q.id).intoMap();

/** Parse a GeoJSON FeatureCollection into quakes, newest first (as the feed orders them). */
export function parseFeed(v) {
  const features = Array.isArray(v?.features) ? v.features : [];
  const out = [];
  for (const f of features) {
    const id = js(f, 'id');
    const props = f?.properties;
    const coords = f?.geometry?.coordinates;
    if (id === null || !props || !Array.isArray(coords)) continue;
    const lon = typeof coords[0] === 'number' ? coords[0] : null;
    const lat = typeof coords[1] === 'number' ? coords[1] : null;
    if (lon === null || lat === null) continue;
    out.push({
      id, mag: jf(props, 'mag') ?? 0, depth_km: typeof coords[2] === 'number' ? coords[2] : 0, lat, lon,
      place: js(props, 'place') ?? '', time_ms: jf(props, 'time') ?? 0,
    });
  }
  return out;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.spec = spec;
    const feed = paramStr(spec, 'feed', 'all_hour');
    if (!FEEDS.includes(feed)) throw new Error(`usgs.earthquakes: unknown feed \`${feed}\`; one of ${FEEDS.join(', ')}`);
    this.url = feedUrl(feed);
    this.minMag = paramF64(spec, 'min_magnitude', 0);
    this.interval = paramSecs(spec, 'interval', 60, 15);
    this.em = new Emitter(emit, log);
    this.seen = new SeenSet(5000);
    this.first = true;
  }
  start() {
    this.poller = new Poller(this.em, this.interval);
    this.poller.run((signal) => this.pollOnce(signal));
  }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(this.url, { signal });
    if (!v || v.features === undefined) throw 'unexpected payload (no features)';
    const quakes = parseFeed(v);
    const hourAgo = unixNow() * 1000 - 3600000;
    const countHour = quakes.filter((q) => q.time_ms >= hourAgo).length;
    const maxMag = quakes.reduce((m, q) => Math.max(m, q.mag), 0);
    let fresh = [];
    for (const q of quakes) if (this.seen.insert(q.id) && q.mag >= this.minMag) fresh.push(q);
    if (this.first) { fresh = fresh.slice(0, 3); this.first = false; }
    fresh.reverse();
    this.em.reading({ count_hour: countHour, max_mag: maxMag, count_feed: quakes.length });
    for (const q of fresh) this.em.event('quake', quakeIntensity(q), quakeAttrs(q));
    return fresh.length ? 'live' : 'idle';
  }
}
