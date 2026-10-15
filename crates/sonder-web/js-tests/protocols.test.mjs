// New browser adapters: generic attrsOf flattening + `each`, MQTT codec, Blitzortung decoder,
// audio feature math. Outputs must match crates/sonder-sources/src/adapters/{generic,mqtt,blitzortung,audio}.rs.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const S = '../static/sources/';
const here = new URL('.', import.meta.url);
const near = (a, b, eps = 1e-9) => assert.ok(Math.abs(a - b) < eps, `${a} !~ ${b}`);

test('generic attrsOf: nested scalars flattened with dotted keys, depth/array/string/cap limits, JSON order', async () => {
  const { attrsOf, MAX_ATTRS, ATTR_MAX_DEPTH, ATTR_MAX_ARRAY, ATTR_MAX_STR } = await import(S + 'generic.js');
  assert.equal(MAX_ATTRS, 48); assert.equal(ATTR_MAX_DEPTH, 4); assert.equal(ATTR_MAX_ARRAY, 8); assert.equal(ATTR_MAX_STR, 200);
  const a = attrsOf({ topic: 't', data: { p: '81736.3', v: 1.5, ok: true, nil: null, arr: [{ px: 1 }, 2, 'x'] }, z: 0 });
  assert.deepEqual(a, { topic: 't', 'data.p': '81736.3', 'data.v': 1.5, 'data.ok': true, 'data.arr[0].px': 1, 'data.arr[1]': 2, 'data.arr[2]': 'x', z: 0 });
  assert.deepEqual(Object.keys(a), ['topic', 'data.p', 'data.v', 'data.ok', 'data.arr[0].px', 'data.arr[1]', 'data.arr[2]', 'z']);
  // depth <= 4: a.b.c.d is 4 levels deep (kept), a.b.c.d.e is 5 (dropped)
  assert.deepEqual(attrsOf({ a: { b: { c: { d: 1, e: { f: 2 } } } } }), { 'a.b.c.d': 1 });
  // arrays: first 8 elements only
  assert.deepEqual(Object.keys(attrsOf({ n: Array.from({ length: 12 }, (_, i) => i) })).length, 8);
  // strings truncated to 200 chars, numbers/bools as-is, NaN/Infinity dropped
  const long = 'x'.repeat(500);
  assert.equal(attrsOf({ s: long }).s.length, 200);
  assert.deepEqual(attrsOf({ a: Infinity, b: NaN, c: -0.5, d: false }), { c: -0.5, d: false });
  // cap at 48 leaves
  const big = Object.fromEntries(Array.from({ length: 100 }, (_, i) => [`k${i}`, i]));
  assert.equal(Object.keys(attrsOf(big)).length, 48);
  // scalars
  assert.deepEqual(attrsOf(3), { value: 3 }); assert.deepEqual(attrsOf('t'), { text: 't' }); assert.deepEqual(attrsOf(true), { value: true });
  assert.deepEqual(attrsOf(null), {}); assert.deepEqual(attrsOf([1, 2]), { '[0]': 1, '[1]': 2 });
});

test('generic `each`: Bybit frame → one reading + event per element; non-array falls back', async () => {
  const { Converter, genericConfig } = await import(S + 'generic.js');
  const frame = { topic: 'publicTrade.BTCUSDT', type: 'snapshot', ts: 1700000000000,
    data: [{ T: 1700000000001, s: 'BTCUSDT', S: 'Sell', v: '0.008315', p: '81736.3', i: 'a1', BT: false },
      { T: 1700000000002, s: 'BTCUSDT', S: 'Buy', v: '0.1', p: '81737.0', i: 'a2', BT: false }] };
  const cfg = genericConfig({ kind: 'websocket', params: { each: '.data', events_on: 'every', event_name: 'trade' } }, 'every', 'message');
  assert.equal(cfg.each, '.data');
  const c = new Converter(cfg);
  const out = c.convert(frame);
  const events = out.filter((d) => d.type === 'event'), readings = out.filter((d) => d.type === 'reading');
  assert.equal(events.length, 2); assert.equal(readings.length, 2);
  assert.equal(events[0].name, 'trade');
  assert.equal(events[0].attrs.p, '81736.3'); assert.equal(events[0].attrs.v, '0.008315'); assert.equal(events[0].attrs.S, 'Sell');
  assert.equal(events[1].attrs.S, 'Buy'); assert.equal(events[1].attrs.BT, false);
  assert.deepEqual(readings[0].values, { BT: 0, T: 1700000000001, p: 81736.3, v: 0.008315 });
  assert.deepEqual(out.map((d) => d.type), ['reading', 'event', 'reading', 'event']);
  // `each` pointing at a non-array: whole object as before
  const whole = c.convert({ topic: 'x', data: { p: 1 } });
  assert.equal(whole.filter((d) => d.type === 'event').length, 1);
  assert.deepEqual(whole[0].values, { 'data.p': 1 });
  // filter is checked on the container before splitting
  const c2 = new Converter(genericConfig({ kind: 'websocket', params: { each: 'data', filter: 'topic==publicTrade.BTCUSDT' } }, 'every', 'message'));
  assert.equal(c2.convert(frame).length, 4);
  assert.deepEqual(c2.convert({ ...frame, topic: 'other' }), []);
  // data_path then each
  const c3 = new Converter(genericConfig({ kind: 'websocket', params: { data_path: '.msg', each: '.items' } }, 'every', 'message'));
  assert.equal(c3.convert({ msg: { items: [{ a: 1 }, { a: 2 }, { a: 3 }] } }).filter((d) => d.type === 'event').length, 3);
  assert.throws(() => genericConfig({ kind: 'websocket', params: { each: 'a[oops' } }, 'every', 'message'));
  // fields = "none": events only, never a reading (also as ["none"]); [] and "auto" stay auto
  for (const fields of ['none', ['none'], 'NONE']) {
    const cn = new Converter(genericConfig({ kind: 'websocket', params: { fields, each: '.data', events_on: 'every' } }, 'every', 'message'));
    const o = cn.convert(frame);
    assert.deepEqual(o.map((d) => d.type), ['event', 'event']);
    assert.equal(o[0].attrs.p, '81736.3');
    assert.deepEqual(cn.convert({ a: 1 }).map((d) => d.type), ['event']);
  }
  assert.equal(genericConfig({ kind: 'http-json', params: { fields: [] } }, 'change', 'update').fields, 'auto');
  assert.equal(genericConfig({ kind: 'http-json', params: { fields: 'auto' } }, 'change', 'update').fields, 'auto');
  assert.equal(genericConfig({ kind: 'http-json', params: { fields: 'none' } }, 'change', 'update').fields, 'none');
  // with fields none and the default `change` policy nothing is emitted (no reading can change)
  assert.deepEqual(new Converter(genericConfig({ kind: 'http-json', params: { fields: 'none' } }, 'change', 'update')).convert({ a: 1 }), []);
  // the generic adapters accept `each`
  const { default: Ws } = await import(S + 'websocket.js');
  assert.equal(new Ws({ kind: 'websocket', params: { url: 'wss://x/y', each: '.data' } }, () => {}).conv.cfg.each, '.data');
  const { default: Http } = await import(S + 'http_json.js');
  assert.equal(new Http({ kind: 'http-json', params: { url: 'https://x/y', each: 'rows' } }, () => {}).conv.cfg.each, 'rows');
  const { default: Sse } = await import(S + 'sse.js');
  assert.equal(new Sse({ kind: 'sse', params: { url: 'https://x/y', each: 'rows' } }, () => {}).conv.cfg.each, 'rows');
});

test('mqtt codec: varint, CONNECT/SUBSCRIBE/PUBLISH/PING round-trip, split frames', async () => {
  const m = await import(S + 'mqtt.js');
  assert.deepEqual(m.encodeVarint(0), [0]); assert.deepEqual(m.encodeVarint(127), [127]);
  assert.deepEqual(m.encodeVarint(128), [0x80, 1]); assert.deepEqual(m.encodeVarint(16383), [0xff, 0x7f]);
  assert.deepEqual(m.encodeVarint(2097152), [0x80, 0x80, 0x80, 1]);
  for (const n of [0, 1, 127, 128, 300, 16383, 16384, 2097151, 268435455]) assert.equal(m.decodeVarint(new Uint8Array(m.encodeVarint(n))).value, n);
  assert.equal(m.decodeVarint(new Uint8Array([0x80])), null);
  // CONNECT per MQTT 3.1.1 §3.1 with username+password, keepalive 60, clean session
  const c = m.encodeConnect({ clientId: 'abc', username: 'wildcard', password: 'pw', keepalive: 60 });
  assert.deepEqual(Array.from(c.subarray(0, 12)), [0x10, c.length - 2, 0, 4, 77, 81, 84, 84, 4, 0xc2, 0, 60]);
  assert.deepEqual(Array.from(c.subarray(12)), [0, 3, 97, 98, 99, 0, 8, ...Buffer.from('wildcard'), 0, 2, 112, 119]);
  const c0 = m.encodeConnect({ clientId: 'x' });
  assert.equal(c0[9], 0x02);
  assert.deepEqual(Array.from(m.encodeSubscribe(1, ['#', 'a/b'])), [0x82, 12, 0, 1, 0, 1, 35, 0, 0, 3, 97, 47, 98, 0]);
  assert.deepEqual(Array.from(m.encodePingreq()), [0xc0, 0]); assert.deepEqual(Array.from(m.encodeDisconnect()), [0xe0, 0]);
  // CONNACK / SUBACK decode
  assert.deepEqual(m.decodePacket(new Uint8Array([0x20, 2, 0, 0])), { type: 2, flags: 0, len: 4, sessionPresent: false, code: 0 });
  assert.equal(m.decodePacket(new Uint8Array([0x20, 2, 0, 5])).code, 5);
  assert.deepEqual(m.decodePacket(new Uint8Array([0x90, 3, 0, 1, 0])).codes, [0]);
  // PUBLISH round trip, QoS 0 and QoS 1, unicode topic
  const payload = JSON.stringify({ t: 21.5, h: 40 });
  const p0 = m.decodePacket(m.encodePublish('sensors/températur', payload));
  assert.equal(p0.type, 3); assert.equal(p0.topic, 'sensors/températur'); assert.equal(p0.qos, 0); assert.equal(new TextDecoder().decode(p0.payload), payload);
  const p1 = m.decodePacket(m.encodePublish('a', new Uint8Array([1, 2, 3]), { qos: 1, packetId: 513, retain: true }));
  assert.equal(p1.qos, 1); assert.equal(p1.packetId, 513); assert.ok(p1.retain); assert.deepEqual(Array.from(p1.payload), [1, 2, 3]);
  assert.deepEqual(Array.from(m.encodePuback(513)), [0x40, 2, 2, 1]);
  // large remaining length (2-byte varint) and a reader fed in arbitrary chunks
  const big = m.encodePublish('big', 'x'.repeat(1000));
  assert.equal(big[1] & 0x80, 0x80);
  const all = new Uint8Array([...big, ...m.encodePingreq(), ...m.encodePublish('t', '1')]);
  const r = new m.PacketReader();
  const got = [];
  for (let i = 0; i < all.length; i += 7) got.push(...r.push(all.subarray(i, Math.min(all.length, i + 7))));
  assert.deepEqual(got.map((p) => p.type), [3, 12, 3]); assert.equal(got[0].payload.length, 1000); assert.equal(got[2].topic, 't');
  assert.equal(r.buf.length, 0);
});

test('mqtt: message attrs/intensity, url resolution, stats', async () => {
  const m = await import(S + 'mqtt.js');
  const enc = (s) => new TextEncoder().encode(s);
  assert.deepEqual(m.messageAttrs('a/b', enc('21.5')), { topic: 'a/b', len: 4, value: 21.5 });
  assert.deepEqual(m.messageAttrs('a', enc(' -3e2 ')), { topic: 'a', len: 6, value: -300 });
  const jsonPayload = '{"t":1,"s":"x","n":{"a":true,"b":null},"arr":[1,2]}';
  assert.deepEqual(m.messageAttrs('j', enc(jsonPayload)),
    { topic: 'j', len: jsonPayload.length, 'data.t': 1, 'data.s': 'x', 'data.n.a': true, 'data.arr[0]': 1, 'data.arr[1]': 2 });
  const many = JSON.stringify(Object.fromEntries(Array.from({ length: 30 }, (_, i) => [`k${i}`, i])));
  assert.equal(Object.keys(m.messageAttrs('j', enc(many))).length, 2 + 16);
  assert.deepEqual(m.messageAttrs('bin', new Uint8Array([0xff, 0xfe, 0])), { topic: 'bin', len: 3 });
  assert.deepEqual(m.messageAttrs('arr', enc('[1,2]')), { topic: 'arr', len: 5 });
  assert.deepEqual(m.messageAttrs('txt', enc('hello')), { topic: 'txt', len: 5 });
  near(m.messageIntensity(0), 0.05); near(m.messageIntensity(256), 0.5); near(m.messageIntensity(5000), 1);
  assert.deepEqual(m.resolveUrl({ kind: 'mqtt', params: {} }, 'https:'), { url: m.DEFAULT_URL });
  assert.match(m.resolveUrl({ kind: 'mqtt', params: { url: 'mqtt://test.mosquitto.org:1883' } }, 'http:').error, /wss:\/\/test\.mosquitto\.org:8081\/mqtt/);
  assert.match(m.resolveUrl({ kind: 'mqtt', params: { url: 'ws://test.mosquitto.org:8080/mqtt' } }, 'https:').error, /https page.*wss:\/\/test\.mosquitto\.org:8081\/mqtt/);
  assert.deepEqual(m.resolveUrl({ kind: 'mqtt', params: { url: 'ws://test.mosquitto.org:8080/mqtt' } }, 'http:'), { url: 'ws://test.mosquitto.org:8080/mqtt' });
  assert.ok(m.resolveUrl({ kind: 'mqtt', params: { url: 'https://x' } }, 'https:').error);
  const a = new m.default({ name: 'b', kind: 'mqtt', params: { topics: 'a/#, b', username: 'wildcard', keepalive: 30 } }, () => {});
  assert.deepEqual(a.topics, ['a/#', 'b']); assert.equal(a.keepalive, 30); assert.equal(a.password, null); assert.ok(a.clientId.startsWith('sonder-'));
  const emitted = [];
  const fail = new m.default({ name: 'b', kind: 'mqtt', params: { url: 'mqtt://h' } }, (j) => emitted.push(JSON.parse(j)));
  fail.start(); assert.equal(emitted[0].state, 'failed');
  const s = new m.Stats();
  s.push(0, 'a', 10); s.push(0.5, 'b', 20); s.push(0.9, 'a', 30);
  assert.deepEqual(s.readings(1), { rate: 3, topics: 2, bytes: 60 });
  assert.deepEqual(s.readings(2), { rate: 0, topics: 2, bytes: 0 });
  assert.deepEqual(s.readings(62), { rate: 0, topics: 0, bytes: 0 });
});

test('blitzortung: decode a real captured frame, strike attrs, bbox, stats/viz', async () => {
  const b = await import(S + 'blitzortung.js');
  const { frame } = JSON.parse(readFileSync(new URL('./fixtures/blitzortung-frame.json', here), 'utf8'));
  const text = b.decodeFrame(frame);
  const v = JSON.parse(text);
  assert.ok('lat' in v && 'lon' in v && 'time' in v, 'decoded JSON has lat, lon, time');
  near(v.lat, 35.065388, 1e-6); near(v.lon, -98.696562, 1e-6); assert.equal(v.sig.length, 40); assert.equal(v.delay, 2.8);
  // decoder also handles plain ASCII (no dictionary codes) and short inputs
  assert.equal(b.decodeFrame('{"a":1}'), '{"a":1}'); assert.equal(b.decodeFrame(''), '');
  const s = b.parseStrike(v);
  assert.equal(s.stations, 40); assert.equal(s.delayMs, 2800); assert.equal(s.pol, 0);
  assert.equal(b.strikeIntensity(s), 1);
  near(b.strikeIntensity({ stations: 10 }), 0.25); near(b.strikeIntensity({ stations: 1 }), 0.1);
  const a = b.strikeAttrs(s);
  assert.deepEqual(Object.keys(a).sort(), ['delay', 'lat', 'lat01', 'lon', 'lon01', 'polarity', 'stations']);
  near(a.lon01, (-98.696562 + 180) / 360); near(a.lat01, (35.065388 + 90) / 180); assert.equal(a.delay, 2800);
  assert.equal(b.parseStrike({ lat: 1, lon: 2, mds: 7 }).stations, 7);
  assert.equal(b.parseStrike({ lat: 1, lon: 2, sig: [], mds: 7 }).stations, 7);
  assert.equal(b.parseStrike({ lon: 2 }), null); assert.equal(b.parseStrike({ lat: 95, lon: 2 }), null);
  assert.deepEqual(b.parseBbox({ kind: 'b', params: { bbox: [30, -100, 40, -90] } }), [30, -100, 40, -90]);
  assert.deepEqual(b.parseBbox({ kind: 'b', params: { bbox: '30,-100,40,-90' } }), [30, -100, 40, -90]);
  assert.throws(() => b.parseBbox({ kind: 'b', params: { bbox: [1, 2, 3] } }));
  assert.throws(() => b.parseBbox({ kind: 'b', params: { bbox: [40, -100, 30, -90] } }));
  assert.ok(b.inBbox(s, [30, -100, 40, -90])); assert.ok(!b.inBbox(s, [40, -100, 50, -90])); assert.ok(b.inBbox(s, null));
  const st = new b.StrikeStats();
  st.push(0, { lat: 35, lon: -98 }); st.push(1, { lat: 36, lon: -97 }); st.push(4, { lat: 50, lon: 10 });
  assert.deepEqual(st.readings(5), { rate: 0.3, active: 2 });
  const viz = st.viz(5.5);
  assert.equal(viz.strikes.length, 2); near(viz.strikes[0].age_s, 4.5, 1e-6); near(viz.strikes[0].lon01, 83 / 360);
  assert.deepEqual(st.readings(70), { rate: 0, active: 0 });
  const ad = new b.default({ name: 'l', kind: 'blitzortung.lightning', params: { max_rate: 5 } }, () => {});
  assert.equal(ad.maxRate, 5); assert.equal(ad.bbox, null); assert.equal(b.HOSTS.length, 3);
});

test('audio: feature math on synthetic buffers, onset + loud detectors, HLS check', async () => {
  const au = await import(S + 'audio.js');
  const sr = 48000, n = 2048, bins = 1024;
  // silence
  const silence = au.features(new Float32Array(n), new Float32Array(bins).fill(-Infinity), sr, null);
  assert.equal(silence.rms, 0); assert.equal(silence.peak, 0); assert.equal(silence.centroid, 0); assert.equal(silence.flux, 0);
  assert.equal(silence.low, 0); assert.equal(silence.mid, 0); assert.equal(silence.high, 0);
  // full-scale square wave at ~1 kHz: rms 0 dBFS → 1, peak 1, zcr ≈ 2·f/sr
  const sq = new Float32Array(n); for (let i = 0; i < n; i++) sq[i] = Math.sin(2 * Math.PI * 1000 * i / sr) >= 0 ? 1 : -1;
  const f1 = au.features(sq, new Float32Array(bins).fill(-100), sr, null);
  near(f1.rms, 1, 1e-6); near(f1.peak, 1); near(f1.zcr, 2 * 1000 / sr, 0.003);
  // −20 dBFS sine → rms 2/3
  const sine = new Float32Array(n); const amp = 0.1 * Math.SQRT2; for (let i = 0; i < n; i++) sine[i] = amp * Math.sin(2 * Math.PI * 440 * i / sr);
  near(au.features(sine, new Float32Array(bins).fill(-100), sr, null).rms, 2 / 3, 0.01);
  // spectrum: single 0 dB bin at 1 kHz → centroid at that bin's frequency, mid band = 1, others 0
  const binHz = sr / 2 / bins;
  const spec = new Float32Array(bins).fill(-100); const k = Math.round(1000 / binHz); spec[k] = 0;
  const f2 = au.features(new Float32Array(n), spec, sr, null);
  near(f2.centroid, k * binHz, 1e-6); assert.equal(f2.low, 0); assert.equal(f2.high, 0);
  assert.equal(f2.mid, 1);
  // two −3 dB bins in the low band sum to 0 dB → 1; one −60 dB bin → 0, −30 dB → 0.5
  const lo = new Float32Array(bins).fill(-100); lo[2] = -10 * Math.log10(2); lo[3] = lo[2];
  near(au.features(new Float32Array(n), lo, sr, null).low, 1, 1e-6);
  const hi = new Float32Array(bins).fill(-100); hi[900] = -30; near(au.features(new Float32Array(n), hi, sr, null).high, 0.5, 1e-6);
  // flux: energy appears where there was none → positive; unchanged → 0; decreasing → 0
  const f3 = au.features(new Float32Array(n), spec, sr, new Float32Array(bins));
  near(f3.flux, 1); assert.equal(au.features(new Float32Array(n), spec, sr, f3.mags).flux, 0);
  // half the mass is new → 0.5
  const half = new Float32Array(bins).fill(-100); half[k] = 0; half[k + 5] = 0;
  near(au.features(new Float32Array(n), half, sr, f3.mags).flux, 0.5, 1e-6);
  assert.equal(au.features(new Float32Array(n), new Float32Array(bins).fill(-100), sr, f3.mags).flux, 0);
  // two bins: centroid is the magnitude-weighted mean
  const two = new Float32Array(bins).fill(-100); two[10] = 0; two[30] = 0;
  near(au.features(new Float32Array(n), two, sr, null).centroid, 20 * binHz, 1e-6);
  near(au.db01(-60), 0); near(au.db01(-30), 0.5); near(au.db01(6), 1);
  // onset detector: steady noise floor, a spike fires once, refractory blocks a second spike
  const od = new au.OnsetDetector({ k: 4, window: 40, refractoryMs: 100 });
  let t = 0; const fired = [];
  for (let i = 0; i < 40; i++, t += 50) if (od.push(0.05 + (i % 2) * 0.01, t) !== null) fired.push(t);
  assert.deepEqual(fired, []);
  const hit = od.push(0.8, t); assert.ok(hit !== null && hit > 0.99, `intensity ${hit}`); t += 50;
  assert.equal(od.push(0.8, t), null, 'refractory'); t += 50;
  assert.equal(od.push(0.05, t), null); t += 500;
  const hit2 = od.push(0.4, t); assert.ok(hit2 !== null && hit2 < 0.6 && hit2 > 0.4, `normalized by window max: ${hit2}`);
  // loud: rising through 0.6 once, re-armed below 0.5
  const ld = new au.LoudDetector();
  assert.deepEqual([0.2, 0.7, 0.8, 0.55, 0.9, 0.4, 0.61].map((r) => ld.push(r)), [false, true, false, false, false, false, true]);
  // HLS detection + spec validation
  assert.ok(au.hlsUnsupported('https://x/live.m3u8', () => '')); assert.ok(!au.hlsUnsupported('https://x/live.m3u8', () => 'maybe'));
  assert.ok(!au.hlsUnsupported('https://ice1.somafm.com/groovesalad-128-mp3', () => ''));
  assert.throws(() => new au.default({ kind: 'audio.features', params: {} }, () => {}));
  assert.throws(() => new au.default({ kind: 'audio.features', params: { url: 'rtsp://x' } }, () => {}));
  const a = new au.default({ kind: 'audio.features', params: { mic: true } }, () => {}); assert.equal(a.label, '🎤 mic');
  assert.equal(new au.default({ kind: 'audio.features', params: { url: 'https://ice1.somafm.com/groovesalad-128-mp3' } }, () => {}).label, '🔊 stream');
  assert.equal(au.HLS_MESSAGE, 'HLS needs a server-side ffmpeg bridge (sonder play … audio.features) or a direct mp3/ogg stream');
});

test('registry: new kinds registered, audio on the main thread', async () => {
  const { ADAPTERS, isMainThreadKind, isAudioKind, loadAdapter } = await import(S + 'index.js');
  assert.ok(ADAPTERS.mqtt && ADAPTERS['blitzortung.lightning'] && ADAPTERS['audio.features']);
  assert.ok(isMainThreadKind('audio.features')); assert.ok(!isMainThreadKind('mqtt')); assert.ok(isAudioKind('audio.features'));
  for (const k of ['mqtt', 'blitzortung.lightning', 'audio.features']) assert.equal(typeof (await loadAdapter(k)), 'function');
});
