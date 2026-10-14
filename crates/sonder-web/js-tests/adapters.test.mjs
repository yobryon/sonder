// Payload mappers must emit exactly what the native Rust adapters emit. Fixtures are
// copied from the #[cfg(test)] modules in crates/sonder-sources/src/adapters/*.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const S = '../static/sources/';
const near = (a, b, eps = 1e-9) => assert.ok(Math.abs(a - b) < eps, `${a} !~ ${b}`);

/** Run an adapter's pollOnce against a canned fetch; returns the emitted sample objects. */
async function collect(Adapter, spec, responses, fn = 'pollOnce') {
  const emitted = [];
  const a = new Adapter(spec, (json) => emitted.push(JSON.parse(json)), () => {});
  const origFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    const key = Object.keys(responses).find((k) => url.startsWith(k) || url.includes(k));
    if (!key) throw new TypeError(`unexpected fetch ${url}`);
    const body = responses[key];
    return new Response(typeof body === 'string' ? body : JSON.stringify(body), { status: 200 });
  };
  try { const r = await a[fn](new AbortController().signal); return { emitted, result: r, adapter: a }; }
  finally { globalThis.fetch = origFetch; }
}

test('usgs: parse_feed, intensity, attrs, first-poll seeding, de-dup', async () => {
  const { default: Adapter, parseFeed, quakeIntensity, quakeAttrs } = await import(S + 'usgs.js');
  const fixture = {
    type: 'FeatureCollection', metadata: { generated: 1700000000000, count: 2 },
    features: [
      { type: 'Feature', id: 'nc73000001', properties: { mag: 2.7, place: '10km NE of Somewhere, CA', time: 1700000000000, type: 'earthquake' }, geometry: { type: 'Point', coordinates: [-122.1, 37.9, 8.2] } },
      { type: 'Feature', id: 'us7000abcd', properties: { mag: 5.1, place: 'Kermadec Islands', time: 1699999000000 }, geometry: { type: 'Point', coordinates: [-177.5, -30.2, 35.0] } },
      { type: 'Feature', id: 'broken', properties: {}, geometry: null },
    ],
  };
  const q = parseFeed(fixture);
  assert.equal(q.length, 2);
  assert.equal(q[0].id, 'nc73000001');
  assert.equal(q[0].depth_km, 8.2);
  near(quakeIntensity(q[1]), 4.1 / 7);
  const a = quakeAttrs(q[1]);
  near(a.lon01, 2.5 / 360);
  assert.equal(a.place, 'Kermadec Islands');
  assert.deepEqual(Object.keys(a).sort(), ['depth_km', 'id', 'lat', 'lat01', 'lon', 'lon01', 'mag', 'place']);

  assert.throws(() => new Adapter({ kind: 'usgs.earthquakes', params: { feed: 'nope' } }, () => {}));
  const spec = { name: 'quakes', kind: 'usgs.earthquakes', params: { feed: '2.5_day', min_magnitude: 1.0 } };
  const { emitted, result, adapter } = await collect(Adapter, spec, { 'earthquake.usgs.gov': fixture });
  assert.equal(result, 'live');
  assert.deepEqual(emitted[0], { type: 'reading', values: { count_hour: 0, max_mag: 5.1, count_feed: 2 } });
  // oldest first
  assert.equal(emitted[1].type, 'event'); assert.equal(emitted[1].name, 'quake'); assert.equal(emitted[1].attrs.id, 'us7000abcd');
  assert.equal(emitted[2].attrs.id, 'nc73000001');
  near(emitted[2].intensity, 1.7 / 7);
  // second poll: nothing new → idle, reading only
  const em2 = [];
  adapter.em.send = (j) => em2.push(JSON.parse(j));
  globalThis.fetch = async () => new Response(JSON.stringify(fixture));
  assert.equal(await adapter.pollOnce(new AbortController().signal), 'idle');
  assert.equal(em2.length, 1);
});

test('open-meteo: parse_current + normalized channels + gust event', async () => {
  const { default: Adapter, parseCurrent } = await import(S + 'open_meteo.js');
  const v = { latitude: 51.5, current_units: { temperature_2m: '°C' },
    current: { time: '2024-01-01T12:00', interval: 900, temperature_2m: 7.3, relative_humidity_2m: 81, precipitation: 0.0, cloud_cover: 100, wind_speed_10m: 5.2, wind_direction_10m: 210, wind_gusts_10m: 9.8, pressure_msl: 1003.4, is_day: 1, weather_code: 3 } };
  const m = Object.fromEntries(parseCurrent(v));
  assert.equal(m.temperature_2m, 7.3); assert.equal(m.cloud01, 1); assert.equal(m.is_day, 1);
  assert.ok(!('time' in m) && !('interval' in m));
  near(m.wind01, 5.2 / 25); near(m.temp01, 17.3 / 50); near(m.humidity01, 0.81);
  assert.deepEqual(parseCurrent({ error: true }), []);
  const spec = { name: 'sky', kind: 'open-meteo.weather', params: { lat: 37.77, lon: -122.42 } };
  const r1 = await collect(Adapter, spec, { 'api.open-meteo.com': v });
  assert.equal(r1.emitted.length, 1); assert.equal(r1.emitted[0].type, 'reading');
  const v2 = structuredClone(v); v2.current.wind_gusts_10m = 15.0;
  const em = []; r1.adapter.em.send = (j) => em.push(JSON.parse(j));
  globalThis.fetch = async () => new Response(JSON.stringify(v2));
  await r1.adapter.pollOnce(new AbortController().signal);
  assert.equal(em[1].name, 'gust'); near(em[1].intensity, 5.2 / 15); assert.deepEqual(em[1].attrs, { gust: 15, prev: 9.8 });
});

test('wikipedia: parse_edit, attrs and filters', async () => {
  const { parseEdit, editAttrs, wikiConfig, accepts } = await import(S + 'wikipedia.js');
  const rc = { $schema: '/mediawiki/recentchange/1.0.0', type: 'edit', namespace: 0, title: 'Rust (programming language)', user: 'Ferris', bot: false, minor: true,
    length: { old: 1000, new: 1500 }, wiki: 'enwiki', server_name: 'en.wikipedia.org' };
  const e = parseEdit(rc);
  assert.equal(e.bytes, 500); near(e.intensity, 0.25); assert.equal(e.wiki, 'enwiki'); assert.equal(e.minor, true);
  const a = editAttrs(e);
  assert.equal(a.user, 'Ferris'); assert.equal(a.bytes, 500); assert.equal(a.bot, false); assert.equal(a.type, 'edit'); assert.equal(a.namespace, 0);
  assert.ok(!accepts(wikiConfig({ kind: 'w', params: { wikis: ['dewiki'] } }), rc));
  const cfg = wikiConfig({ kind: 'w', params: { wikis: 'enwiki' } });
  assert.ok(accepts(cfg, rc));
  assert.ok(!accepts(cfg, { ...rc, bot: true }));
  assert.ok(!accepts(cfg, { ...rc, type: 'log' }));
  assert.throws(() => wikiConfig({ kind: 'w', params: { types: 'bogus' } }));
  assert.ok(accepts(wikiConfig({ kind: 'w', params: { include_bots: true, types: ['edit', 'log'] } }), { ...rc, bot: true, type: 'log' }));
});

test('coinbase: parse_match, intensity, stats, subscribe', async () => {
  const { default: Adapter, parseMatch, tradeIntensity, tradeAttrs, TradeStats, subscribeMsg } = await import(S + 'coinbase.js');
  const v = JSON.parse('{"type":"match","trade_id":12345,"maker_order_id":"a","taker_order_id":"b","side":"sell","size":"0.025","price":"64321.5","product_id":"BTC-USD","sequence":1,"time":"2024-01-01T00:00:00.000000Z"}');
  const t = parseMatch(v);
  assert.equal(t.product, 'BTC-USD'); near(t.usd, 1608.0375, 1e-6); near(tradeIntensity(t), Math.log10(1609.0375) / 5);
  assert.deepEqual(tradeAttrs(t), { product: 'BTC-USD', side: 'sell', price: 64321.5, size: 0.025, usd: t.usd, buy: false });
  assert.equal(parseMatch({ type: 'heartbeat' }), null);
  const s = new TradeStats();
  s.push(0, { product: 'BTC-USD', side: 'buy', price: 100, size: 1, usd: 100 });
  s.push(5, { product: 'BTC-USD', side: 'sell', price: 110, size: 1, usd: 110 });
  const r = Object.fromEntries(s.readings(6));
  assert.equal(r.price, 110); near(r.price_change, 10); assert.equal(r.buy_ratio, 0.5); near(r.rate, 0.2); near(r.usd_rate, 21);
  assert.deepEqual(subscribeMsg(['BTC-USD']), { type: 'subscribe', product_ids: ['BTC-USD'], channels: ['matches', 'heartbeat'] });
  assert.throws(() => new Adapter({ kind: 'coinbase.trades', params: { products: 'btc' } }, () => {}));
  assert.deepEqual(new Adapter({ kind: 'coinbase.trades', params: { products: 'btc-usd, eth-usd' } }, () => {}).products, ['BTC-USD', 'ETH-USD']);
});

test('opensky: parse_states, summarize, intensity, bbox', async () => {
  const { default: Adapter, parseStates, summarize, aircraftAttrs, aircraftIntensity, parseBbox, DEFAULT_BBOX } = await import(S + 'opensky.js');
  const fixture = { time: 1700000000, states: [
    ['4ca1fa', 'RYR1234 ', 'Ireland', 1700000000, 1700000000, -0.45, 51.47, 10972.8, false, 231.5, 270.1, 0.0, null, 11277.6, '4521', false, 0],
    ['406b2c', 'BAW12   ', 'United Kingdom', 1700000000, 1700000000, -0.2, 51.5, null, true, 5.1, 90.0, null, null, null, null, false, 0],
    ['abc123', null, 'x', 1, 1, 0.1, 51.0, 3000.0, false, 120.0, 10.0, 5.2, null, 3100.0, null, false, 0],
  ] };
  const list = parseStates(fixture);
  assert.equal(list.length, 3); assert.equal(list[0].callsign, 'RYR1234'); assert.equal(list[1].alt, null); assert.ok(list[1].on_ground);
  const s = Object.fromEntries(summarize(list));
  assert.equal(s.count, 3); assert.equal(s.airborne, 2); near(s.mean_alt, (10972.8 + 3000) / 2, 1e-6); assert.equal(s.climbing, 0.5);
  const a = aircraftAttrs(list[0], DEFAULT_BBOX);
  near(a.lat01, (51.47 - 50.5) / 1.5); assert.equal(a.on_ground, false); assert.ok(!('vertical_rate' in aircraftAttrs(list[1], DEFAULT_BBOX)));
  assert.ok(aircraftIntensity(list[0]) > aircraftIntensity(list[2]));
  assert.throws(() => parseBbox({ params: { bbox: [1, 2, 3] } }));
  assert.throws(() => parseBbox({ params: { bbox: [3, 2, 1, 4] } }));
  assert.deepEqual(parseBbox({ params: { bbox: [30, -10, 40, 10] } }), [30, -10, 40, 10]);
  // first poll: readings only (no events); second poll: new aircraft → events
  const spec = { name: 'aircraft', kind: 'opensky.flights', params: {} };
  const r = await collect(Adapter, spec, { 'opensky-network.org': fixture });
  assert.equal(r.emitted.length, 1);
  const em = []; r.adapter.em.send = (j) => em.push(JSON.parse(j));
  globalThis.fetch = async () => new Response(JSON.stringify({ time: 1, states: [...fixture.states, ['new001', 'X', 'y', 1, 1, 0, 51, 9000, false, 200, 0, 0, null, 9000, null, false, 0]] }));
  await r.adapter.pollOnce(new AbortController().signal);
  assert.equal(em.filter((d) => d.type === 'event').length, 1);
  assert.equal(em[1].attrs.icao24, 'new001');
});

test('space weather: array-of-arrays and array-of-objects shapes', async () => {
  const { parsePlasma, parseKp, parsePropagated, parseMag } = await import(S + 'space_weather.js');
  const plasma = [['time_tag', 'density', 'speed', 'temperature'], ['2024-01-01 00:00:00.000', '3.21', '412.5', '61000'], ['2024-01-01 00:01:00.000', null, '415.0', '62000'], ['2024-01-01 00:02:00.000', '3.40', '418.2', null]];
  const r = Object.fromEntries(parsePlasma(plasma));
  assert.equal(r.speed, 412.5); assert.equal(r.density, 3.21);
  assert.equal(parseKp([['time_tag', 'Kp', 'a_running', 'station_count'], ['2024-01-01 00:00:00.000', '2.33', 9, 8], ['2024-01-01 03:00:00.000', '3.67', 15, 8]]), 3.67);
  assert.equal(parsePlasma([['time_tag', 'x']]), null);
  assert.equal(parsePlasma({ not: 'array' }), null);
  const prop = [['time_tag', 'speed', 'density', 'temperature', 'bx', 'by', 'bz', 'bt', 'vx', 'vy', 'vz', 'propagated_time_tag'],
    ['2026-10-13T07:17:00Z', 468.5, 9.72, 234878.0, 7.25, -3.58, 4.44, 9.28, -468.1, -6.1, -18.4, '2026-10-13T08:05:47Z'],
    ['2026-10-13T07:18:00Z', 470.9, 9.75, 238286.0, 6.85, -2.65, 5.15, 9.11, -469.8, -8.1, -31.0, '2026-10-13T08:06:36Z']];
  const p = Object.fromEntries(parsePropagated(prop));
  assert.equal(p.speed, 470.9); assert.equal(p.bz, 5.15); assert.equal(p.bt, 9.11);
  const wind = [{ time_tag: '2026-10-13T08:11:03', proton_speed: 462.95, proton_temperature: 184207, proton_density: 8.62 },
    { time_tag: '2026-10-13T08:12:03', proton_speed: 465.0, proton_temperature: null, proton_density: 8.7 },
    { time_tag: '2026-10-13T08:10:03', proton_speed: 460.0, proton_temperature: 180000, proton_density: 8.5 }];
  const w = Object.fromEntries(parsePlasma(wind));
  assert.equal(w.speed, 462.95); assert.equal(w.density, 8.62);
  assert.equal(Object.fromEntries(parseMag([{ time_tag: '2026-10-13T08:13:01', bt: 8.9, bz_gsm: 4.95 }])).bz, 4.95);
  assert.equal(parseKp([{ time_tag: '2026-10-13T00:00:00', Kp: 3.33, a_running: 18 }, { time_tag: '2026-10-13T03:00:00', Kp: 2.0, a_running: 18 }]), 2.0);
});

test('space weather adapter: readings incl. kp01/speed01 and kp_change event', async () => {
  const { default: Adapter, PROPAGATED_URL, KP_URL } = await import(S + 'space_weather.js');
  const prop = [['time_tag', 'speed', 'density', 'temperature', 'bz', 'bt'], ['t', 525, 5, 100000, 1, 2]];
  const kp = [{ time_tag: 'a', Kp: 3 }];
  const r = await collect(Adapter, { name: 'sw', kind: 'noaa.space-weather', params: {} }, { [PROPAGATED_URL]: prop, [KP_URL]: kp });
  assert.deepEqual(r.emitted[0], { type: 'reading', values: { speed: 525, density: 5, temperature: 100000, bz: 1, bt: 2, kp: 3, kp01: 3 / 9, speed01: 0.5 } });
  const em = []; r.adapter.em.send = (j) => em.push(JSON.parse(j));
  globalThis.fetch = async (u) => new Response(JSON.stringify(u.startsWith(KP_URL) ? [{ time_tag: 'b', Kp: 4 }] : prop));
  await r.adapter.pollOnce(new AbortController().signal);
  assert.equal(em[1].name, 'kp_change'); near(em[1].intensity, 4 / 9); assert.deepEqual(em[1].attrs, { kp: 4, prev: 3, delta: 1 });
});

test('tides: parse_level, trend, station param, idle on repeated stamp', async () => {
  const { default: Adapter, parseLevel, trend, parseStation } = await import(S + 'tides.js');
  const v = { metadata: { id: '9414290', name: 'San Francisco' }, data: [{ t: '2024-01-01 00:00', v: '1.234', s: '0.010', f: '0,0,0,0', q: 'p' }] };
  assert.deepEqual(parseLevel(v), [1.234, '2024-01-01 00:00']);
  assert.throws(() => parseLevel({ error: { message: 'No data was found' } }), /NOAA: No data was found/);
  near(trend(1.0, 1.1, 360), 1.0);
  assert.equal(parseStation({ params: { station: 8518750 } }), '8518750');
  assert.throws(() => parseStation({ params: { station: '../x' } }));
  const r = await collect(Adapter, { name: 'tide', kind: 'noaa.tides', params: { station: '9414290' } }, { 'tidesandcurrents': v });
  assert.equal(r.result, 'live');
  assert.deepEqual(r.emitted[0], { type: 'reading', values: { water_level: 1.234, water_level01: 2.234 / 4 } });
  globalThis.fetch = async () => new Response(JSON.stringify(v));
  assert.equal(await r.adapter.pollOnce(new AbortController().signal), 'idle');
});

test('hackernews: parse_item, intensities, attrs', async () => {
  const { parseItem, itemIntensity, itemAttrs } = await import(S + 'hackernews.js');
  const story = { by: 'pg', descendants: 71, id: 8863, kids: [8952], score: 111, time: 1175714200, title: 'My YC app: Dropbox', type: 'story', url: 'http://x' };
  const s = parseItem(story);
  assert.equal(itemIntensity(s), 0.8); assert.equal(itemAttrs(s).title, 'My YC app: Dropbox');
  assert.deepEqual(itemAttrs(s), { id: 8863, type: 'story', score: 111, by: 'pg', title: 'My YC app: Dropbox', text_len: 0, descendants: 71 });
  const c = parseItem({ by: 'norvig', id: 2921983, parent: 2921506, text: 'Aw shucks, guys ... <p>Hello', time: 1314211127, type: 'comment' });
  assert.equal(itemIntensity(c), 0.3); assert.ok(c.text_len > 10);
  assert.equal(parseItem({ id: 1, deleted: true, type: 'comment' }), null);
  assert.equal(parseItem(null), null);
});

test('github: parse_events + intensities', async () => {
  const { parseEvents, intensityFor, eventAttrs } = await import(S + 'github.js');
  const e = parseEvents([
    { id: '3', type: 'PushEvent', actor: { login: 'octocat' }, repo: { name: 'octo/hello' }, payload: { size: 1 } },
    { id: '2', type: 'PullRequestEvent', actor: { login: 'a' }, repo: { name: 'x/y' }, payload: { action: 'opened' } },
    { type: 'NoId' },
  ]);
  assert.equal(e.length, 2); assert.equal(intensityFor(e[0].ty), 0.4); assert.equal(e[1].action, 'opened'); assert.equal(intensityFor(e[1].ty), 0.7);
  assert.deepEqual(eventAttrs(e[0]), { type: 'PushEvent', repo: 'octo/hello', actor: 'octocat', action: '', id: '3' });
});

test('iss: both payload shapes, readings, equator crossing', async () => {
  const { default: Adapter, parsePosition, readings } = await import(S + 'iss.js');
  const [lat, lon] = parsePosition({ message: 'success', timestamp: 1700000000, iss_position: { latitude: '-12.3456', longitude: '100.1' } });
  assert.equal(lat, -12.3456);
  const r = Object.fromEntries(readings(lat, lon));
  near(r.lon01, 280.1 / 360); assert.equal(r.northbound, 0);
  assert.equal(parsePosition({ message: 'error' }), null);
  assert.deepEqual(parsePosition({ name: 'iss', id: 25544, latitude: 10.5, longitude: -20, velocity: 27000 }), [10.5, -20]);
  const a = await collect(Adapter, { name: 'station', kind: 'iss.position', params: {} }, { 'wheretheiss': { latitude: -1, longitude: 10 } });
  assert.deepEqual(a.emitted[0].values, { lat: -1, lon: 10, lat01: 89 / 180, lon01: 190 / 360, northbound: 0 });
  const em = []; a.adapter.em.send = (j) => em.push(JSON.parse(j));
  globalThis.fetch = async () => new Response(JSON.stringify({ latitude: 1, longitude: 10.3 }));
  await a.adapter.pollOnce(new AbortController().signal);
  assert.equal(em[0].name, 'equator_cross'); assert.equal(em[0].attrs.northbound, true);
  assert.equal(em[1].values.northbound, 1); near(em[1].values.speed_deg_s, Math.hypot(2, 0.3) / 5);
});

test('generic converter: auto fields, change policy, explicit paths, filter, data_path, names', async () => {
  const { Converter, genericConfig, channelName, parsePath, extract, extractF64, flattenNumeric } = await import(S + 'generic.js');
  const cfg = (eventsOn) => ({ fields: 'auto', eventsOn, eventName: 'update', dataPath: null, filter: null });
  const c = new Converter(cfg('change'));
  assert.equal(c.convert({ a: 1, b: { c: 2 }, s: 'x' }).length, 1);
  assert.equal(c.convert({ a: 1, b: { c: 2 }, s: 'y' }).length, 1);
  const d3 = c.convert({ a: 2, b: { c: 2 }, s: 'y' });
  assert.equal(d3.length, 2);
  assert.deepEqual(d3[1], { type: 'event', name: 'update', intensity: 1, attrs: { a: 2, s: 'y' } });
  const c2 = new Converter({ fields: ['.payload.x', 'payload.arr[1]'], eventsOn: 'every', eventName: 'tick', dataPath: '.msg', filter: ['.type', 'data'] });
  const d = c2.convert({ msg: { type: 'data', payload: { x: 5, arr: [1, 2] } } });
  assert.equal(d.length, 2);
  assert.deepEqual(d[0], { type: 'reading', values: { 'payload.arr[1]': 2, 'payload.x': 5 } });
  assert.deepEqual(c2.convert({ msg: { type: 'heartbeat' } }), []);
  const g = genericConfig({ kind: 'http-json', params: { fields: ['a', '.b[0]'], events_on: 'every', filter: 'type==match' } }, 'change', 'update');
  assert.equal(g.eventsOn, 'every'); assert.deepEqual(g.filter, ['type', 'match']); assert.equal(g.fields.length, 2);
  assert.throws(() => genericConfig({ kind: 'http-json', params: { events_on: 'sometimes' } }, 'change', 'update'));
  assert.equal(channelName('.a.b[0]'), 'a.b[0]');
  assert.equal(channelName('["weird key"].x'), 'weird key.x');
  const v = { a: { b: [{ c: 3 }, { c: 4 }], 'weird key': { x: '5' } }, top: true };
  assert.equal(extract(v, '.a.b[0].c'), 3); assert.equal(extract(v, 'a.b.1.c'), 4); assert.equal(extractF64(v, "a['weird key'].x"), 5);
  assert.equal(extractF64(v, 'top'), 1); assert.equal(extract(v, '.a.nope'), undefined); assert.equal(parsePath('a.b[oops'), null);
  const f = Object.fromEntries(flattenNumeric({ a: 1, b: { c: '2.5', d: true, e: 'text', f: null }, arr: [1, { x: 2 }] }));
  assert.deepEqual(f, { a: 1, 'b.c': 2.5, 'b.d': 1, 'arr[0]': 1, 'arr[1].x': 2 });
  assert.equal(flattenNumeric({ x: 1 }, 'root')[0][0], 'root.x');
});

test('http-json / sse / websocket specs: url + headers validation', async () => {
  const { default: Http, parseHeaders } = await import(S + 'http_json.js');
  const { default: Sse } = await import(S + 'sse.js');
  const { default: Ws } = await import(S + 'websocket.js');
  assert.throws(() => new Http({ kind: 'http-json', params: {} }, () => {}));
  assert.throws(() => new Http({ kind: 'http-json', params: { url: 'ftp://x' } }, () => {}));
  const spec = { kind: 'http-json', params: { url: 'https://x/y', headers: { 'X-Token': 'abc', N: 1 } } };
  assert.deepEqual(parseHeaders(spec), { 'X-Token': 'abc', N: '1' });
  assert.ok(new Http(spec, () => {}));
  assert.ok(new Sse({ kind: 'sse', params: { url: 'https://stream.example/x', filter: 'type==edit', sse_event: 'message' } }, () => {}));
  assert.throws(() => new Sse({ kind: 'sse', params: {} }, () => {}));
  assert.ok(new Ws({ kind: 'websocket', params: { url: 'wss://x/y', subscribe: { type: 'subscribe' } } }, () => {}));
  assert.throws(() => new Ws({ kind: 'websocket', params: { url: 'https://x' } }, () => {}));
});

test('util: SSE parser, backoff bounds, Emitter status de-dup, SeenSet cap', async () => {
  const { SseParser, retryDelaySec, Emitter, SeenSet, RateWindow } = await import(S + 'util.js');
  const p = new SseParser();
  const evs = p.push('event: message\nid: [{"topic":"x"}]\ndata: {"a":1}\ndata: {"b":2}\n\n: comment\r\ndata: x\r\n\r\n');
  assert.equal(evs.length, 2); assert.equal(evs[0].event, 'message'); assert.equal(evs[0].data, '{"a":1}\n{"b":2}'); assert.equal(evs[1].data, 'x');
  for (let i = 0; i < 20; i++) { const d = retryDelaySec(0); assert.ok(d >= 0.75 && d <= 1.25); }
  assert.ok(retryDelaySec(12) <= 360 && retryDelaySec(12) >= 225);
  const out = [];
  const em = new Emitter((j) => out.push(JSON.parse(j)));
  em.status('connecting', 'connecting'); em.status('connecting', 'again'); em.status('live', 'live'); em.statusForce('live', 'still');
  assert.deepEqual(out.map((o) => o.message), ['connecting', 'live', 'still']);
  em.reading({ a: 1, b: NaN, c: Infinity }); assert.deepEqual(out.at(-1).values, { a: 1 });
  em.reading({ b: NaN }); assert.equal(out.length, 4);
  em.event('e', NaN, { x: 1 }); assert.equal(out.at(-1).intensity, 0);
  const s = new SeenSet(2); assert.ok(s.insert('a')); assert.ok(!s.insert('a')); s.insert('b'); s.insert('c'); assert.ok(!s.has('a'));
  const w = new RateWindow(); w.push(0); w.push(5); near(w.rate(6, 10), 0.2); assert.equal(w.count(16, 10), 0);
});

test('browser.input pure helpers', async () => {
  const { speedOf, scrollFraction } = await import(S + 'browser_input.js');
  assert.equal(speedOf(2000), 1);
  assert.equal(scrollFraction(0, 1000, 1000), 0);
  assert.equal(scrollFraction(500, 2000, 1000), 0.5);
  assert.equal(scrollFraction(5000, 2000, 1000), 1);
});

test('camera luma + capture size', async () => {
  const { rgbaToGray, captureSize } = await import(S + 'camera.js');
  assert.deepEqual(Array.from(rgbaToGray(new Uint8ClampedArray([255, 255, 255, 255, 0, 0, 0, 255, 255, 0, 0, 255]), new Uint8Array(3))), [255, 0, 76]);
  assert.deepEqual(captureSize(640, 480), [160, 120]);
  assert.deepEqual(captureSize(100, 50), [100, 50]);
});

test('poller: repeated CORS failures become `failed` but polling continues', async () => {
  const { Poller, Emitter, FAILED_AFTER } = await import(S + 'util.js');
  const out = [];
  const em = new Emitter((j) => out.push(JSON.parse(j)));
  const p = new Poller(em, 0.001);
  p.sleep = () => Promise.resolve();
  let n = 0;
  const run = p.run(async () => { if (++n > FAILED_AFTER + 1) { p.stop(); } throw 'request failed (network or CORS blocked by x)'; });
  await run;
  const states = out.map((o) => o.state);
  assert.equal(states[0], 'connecting');
  assert.ok(states.slice(1, FAILED_AFTER).every((s) => s === 'reconnecting'));
  assert.equal(states[FAILED_AFTER], 'failed');
  assert.ok(n > FAILED_AFTER, 'kept polling after failed');
});
