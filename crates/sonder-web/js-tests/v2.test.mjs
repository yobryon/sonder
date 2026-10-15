// v2 UI math: URL scenes, captions, piano-roll hit-testing, Live Earth geometry.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { encodeScene, decodeScene, scenePayloadFromHash, sceneLink, b64urlEncode, b64urlDecode, hasDeflate } = await import('../static/scene-url.js');
const { captionLife, captionKey, newCaptions, captionAlpha, liveCaptions, isBig } = await import('../static/captions.js');
const { noteRect, hitNote, causedBy, pitchName, causeText, attrSummary } = await import('../static/roll.js');
const { project, unproject, eventLatLon, sunPosition, sunAltitude, nightPolygon } = await import('../static/geo.js');

const TOML = `name = "Night Freeway"
description = "Headlights over an overpass at 3am — ünïcödé ok"
viz = "freeway"

[[layer]]
name = "traffic"
role = "pad"
instrument = "pad.glass"
source = { kind = "synthetic.freeway", lanes = 4, flow = 0.4 }
`;

// ---------------------------------------------------------------- URL scenes
test('base64url round trip, no padding, url-safe alphabet', () => {
  const bytes = new Uint8Array([0, 1, 2, 250, 251, 252, 253, 254, 255, 62, 63]);
  const s = b64urlEncode(bytes);
  assert.doesNotMatch(s, /[+/=]/);
  assert.deepEqual([...b64urlDecode(s)], [...bytes]);
  for (let n = 0; n < 5; n++) assert.deepEqual([...b64urlDecode(b64urlEncode(new Uint8Array(n).fill(7)))], new Array(n).fill(7));
});

test('deflate round trip is smaller than the text', async () => {
  assert.ok(hasDeflate(), 'node provides CompressionStream');
  const enc = await encodeScene(TOML);
  assert.ok(!enc.startsWith('~'));
  assert.equal(await decodeScene(enc), TOML);
  // A realistic multi-layer scene compresses well below its own length (and the plain form).
  const big = TOML + Array.from({ length: 5 }, (_, i) => `\n[[layer]]\nname = "layer${i}"\nrole = "pluck"\ninstrument = "pluck.glass"\nsource = "traffic"\n`).join('');
  const encBig = await encodeScene(big);
  const plainBig = await encodeScene(big, { deflate: false });
  assert.ok(encBig.length < big.length * 0.7, `${encBig.length} vs ${big.length}`);
  assert.ok(encBig.length < plainBig.length);
  assert.equal(await decodeScene(encBig), big);
});

test('plain fallback round trip is marked and decodes without deflate', async () => {
  const enc = await encodeScene(TOML, { deflate: false });
  assert.ok(enc.startsWith('~'));
  assert.equal(await decodeScene(enc), TOML);
});

test('garbage payload throws rather than yielding a scene', async () => {
  await assert.rejects(decodeScene('!!!not base64!!!'));
  await assert.rejects(decodeScene(b64urlEncode(new Uint8Array([1, 2, 3, 4, 5]))));
});

test('hash parsing and link building', async () => {
  assert.equal(scenePayloadFromHash('#s=abc'), 'abc');
  assert.equal(scenePayloadFromHash('#x=1&s=a%2Bb&y=2'), 'a+b');
  assert.equal(scenePayloadFromHash('#other'), null);
  assert.equal(scenePayloadFromHash(''), null);
  const link = await sceneLink('https://user.github.io/sonder/?scene=x&seed=1#old', TOML);
  const u = new URL(link);
  assert.equal(u.search, '');
  assert.equal(u.pathname, '/sonder/');
  assert.equal(await decodeScene(scenePayloadFromHash(u.hash)), TOML);
});

// ------------------------------------------------------------------ captions
test('caption life scales with importance', () => {
  assert.equal(captionLife(0), 4);
  assert.equal(captionLife(1), 10);
  assert.equal(captionLife(0.5), 7);
  assert.equal(captionLife(5), 10);
  assert.equal(captionLife(undefined), 4);
  assert.ok(isBig({ importance: 0.8 }) && !isBig({ importance: 0.79 }));
});

test('captions dedupe by at+text across repeated snapshot windows', () => {
  const seen = new Set();
  const w1 = [{ at: 1, text: 'a 4.1 off Honshu', importance: 0.9 }, { at: 1, text: 'other', importance: 0.2 }, { at: 2, text: '', importance: 1 }];
  const a = newCaptions(w1, seen, 1000);
  assert.equal(a.length, 2, 'blank text skipped');
  assert.equal(a[0].key, captionKey(w1[0]));
  assert.equal(a[0].life, 9.4);
  assert.equal(newCaptions(w1, seen, 2000).length, 0);
  const w2 = [...w1, { at: 3, text: 'a 4.1 off Honshu', importance: 0.9 }]; // same text, new time → new
  assert.equal(newCaptions(w2, seen, 3000).length, 1);
});

test('caption fade: solid for 60 %, linear to zero, then gone', () => {
  const c = { wall: 0, life: 10 };
  assert.equal(captionAlpha(c, 0), 1);
  assert.equal(captionAlpha(c, 6000), 1);
  assert.ok(Math.abs(captionAlpha(c, 8000) - 0.5) < 1e-9);
  assert.equal(captionAlpha(c, 10000), 0);
  assert.equal(captionAlpha(c, 99999), 0);
  assert.equal(captionAlpha(c, -500), 1);
  const list = [{ key: 'old', wall: 0, life: 4, at: 0 }, { key: 'new', wall: 5000, life: 4, at: 5 }, { key: 'mid', wall: 4000, life: 10, at: 4 }];
  assert.deepEqual(liveCaptions(list, 6000).map((c) => c.key), ['new', 'mid']);
  assert.deepEqual(liveCaptions(list, 6000, 1).map((c) => c.key), ['new']);
});

// --------------------------------------------------------------- piano roll
const geom = { now: 100, W: 1200, top: 500, rh: 200, pxPerSec: 100, noteH: 6, midiLo: 36, midiHi: 96 };

test('note rects: position, clipping, off-screen', () => {
  const r = noteRect({ at: 99, midi: 66, duration: 0.5, active: false }, geom);
  assert.equal(r.x, 1100);
  assert.equal(r.w, 50);
  assert.ok(Math.abs(r.y - (500 + 100 - 3)) < 1e-9, 'midi 66 is halfway');
  assert.equal(noteRect({ at: 80, midi: 60, duration: 1, active: false }, geom), null, 'scrolled off the left');
  assert.equal(noteRect({ at: 101, midi: 60, duration: 1, active: false }, geom), null, 'in the future');
  const active = noteRect({ at: 98, midi: 60, duration: null, active: true }, geom);
  assert.equal(active.w, 200, 'active notes run to the playhead');
  const tiny = noteRect({ at: 99, midi: 60, duration: 0.001, active: false }, geom);
  assert.equal(tiny.w, 3, 'minimum width');
  assert.equal(noteRect({ at: 99, midi: 200, duration: 0.1 }, geom).y, 500 - 3, 'clamped to the top');
});

test('hit testing picks the topmost rect with padding', () => {
  const rects = [
    { x: 10, y: 10, w: 40, h: 6, key: 'a' },
    { x: 30, y: 12, w: 40, h: 6, key: 'b' },
  ];
  assert.equal(hitNote(rects, 12, 13).key, 'a');
  assert.equal(hitNote(rects, 35, 13).key, 'b', 'later-drawn wins where they overlap');
  assert.equal(hitNote(rects, 8.5, 13).key, 'a', 'within 2px pad');
  assert.equal(hitNote(rects, 5, 13), null);
  assert.equal(hitNote(rects, 12, 30), null);
  assert.equal(hitNote([], 12, 13), null);
});

test('attribution: note ↔ event matching and text', () => {
  const ev = { at: 12.34, layer: 2, name: 'trade', attrs: { size: 0.13, side: 'sell', venue: 'OKX', x: 1 }, label: '' };
  const note = { layer: 2, cause: { event: 'trade', at: 12.34, label: '', attrs: ev.attrs } };
  assert.ok(causedBy(note, ev));
  assert.ok(!causedBy({ ...note, layer: 1 }, ev));
  assert.ok(!causedBy({ ...note, cause: { ...note.cause, at: 12.35 } }, ev));
  assert.ok(!causedBy({ layer: 2 }, ev));
  assert.ok(!causedBy(note, null));
  assert.equal(attrSummary(ev.attrs), 'size=0.13 side=sell venue=OKX');
  assert.equal(causeText(note.cause), 'trade · size=0.13 side=sell venue=OKX');
  assert.equal(causeText({ ...note.cause, label: 'a 0.13 BTC sell on OKX' }), 'a 0.13 BTC sell on OKX');
  assert.equal(causeText({ ...ev, label: 'labelled' }), 'labelled', 'events use the same path');
  assert.equal(causeText(null), '');
  assert.equal(pitchName(60), 'C4');
  assert.equal(pitchName(61.4), 'C#4');
  assert.equal(pitchName(36), 'C2');
  assert.equal(pitchName(95), 'B6');
});

// ------------------------------------------------------------------- geo
test('projection and event positions', () => {
  assert.deepEqual(project(0, 0, 360, 180), [180, 90]);
  assert.deepEqual(project(-180, 90, 360, 180), [0, 0]);
  assert.deepEqual(unproject(360, 180, 360, 180), [180, -90]);
  assert.deepEqual(eventLatLon({ lat: 35.7, lon: 139.7 }), { lat: 35.7, lon: 139.7 });
  const n = eventLatLon({ lat01: 0.75, lon01: 0.25 });
  assert.ok(Math.abs(n.lat - 45) < 1e-9 && Math.abs(n.lon + 90) < 1e-9);
  assert.equal(eventLatLon({ lat: 'x', lon: 1 }), null);
  assert.equal(eventLatLon({ lat: 95, lon: 1 }), null, 'out of range is not a position');
  assert.equal(eventLatLon({ x: 0.5 }), null);
  assert.equal(eventLatLon(undefined), null);
});

test('sun declination: solstices and equinoxes', () => {
  const decl = (iso) => sunPosition(new Date(iso)).decl;
  assert.ok(Math.abs(decl('2026-06-21T08:00:00Z') - 23.44) < 0.1, `june ${decl('2026-06-21T08:00:00Z')}`);
  assert.ok(Math.abs(decl('2026-12-21T15:00:00Z') + 23.44) < 0.1, `december ${decl('2026-12-21T15:00:00Z')}`);
  assert.ok(Math.abs(decl('2026-03-20T14:46:00Z')) < 0.3, `march ${decl('2026-03-20T14:46:00Z')}`);
  assert.ok(Math.abs(decl('2026-09-23T00:05:00Z')) < 0.3, `september ${decl('2026-09-23T00:05:00Z')}`);
  for (const iso of ['2000-01-01T00:00:00Z', '2026-10-15T12:00:00Z', '2040-07-04T03:00:00Z']) {
    const d = decl(iso);
    assert.ok(d > -23.5 && d < 23.5, `${iso} ${d}`);
  }
});

test('subsolar longitude follows UTC noon (within the equation of time)', () => {
  // At 12:00 UTC the sun is over roughly the prime meridian (±4° for the equation of time).
  const s = sunPosition(new Date('2026-03-20T12:00:00Z'));
  assert.ok(Math.abs(s.lon) < 4, `noon ${s.lon}`);
  // Six hours later it is 90° further west.
  const s2 = sunPosition(new Date('2026-03-20T18:00:00Z'));
  assert.ok(Math.abs(s2.lon + 90) < 4, `18:00 ${s2.lon}`);
  // At midnight UTC it is near the antimeridian.
  const s3 = sunPosition(new Date('2026-03-20T00:00:00Z'));
  assert.ok(Math.abs(Math.abs(s3.lon) - 180) < 4, `midnight ${s3.lon}`);
});

test('terminator: day under the sun, night opposite, polygon closes over the dark pole', () => {
  const june = sunPosition(new Date('2026-06-21T12:00:00Z'));
  assert.ok(sunAltitude(june.decl, june.lon, june) > 89);
  assert.ok(sunAltitude(-june.decl, june.lon + 180, june) < -89);
  assert.ok(sunAltitude(80, 0, june) > 0, 'midnight sun in the Arctic in June');
  assert.ok(sunAltitude(-80, 0, june) < 0, 'polar night in the Antarctic in June');
  const poly = nightPolygon(june, 36);
  assert.equal(poly.length, 37 + 2);
  assert.equal(poly[37][1], -90, 'south pole is dark in June');
  // Every terminator point sits where the sun is on the horizon.
  for (const [lon, lat] of poly.slice(0, 37)) assert.ok(Math.abs(sunAltitude(lat, lon, june)) < 0.05, `${lon},${lat}`);
  const dec = sunPosition(new Date('2026-12-21T12:00:00Z'));
  assert.equal(nightPolygon(dec, 12)[13][1], 90, 'north pole is dark in December');
  // Near the equinox the terminator is a meridian pair; the polygon must not blow up.
  const eq = nightPolygon({ decl: 0, lon: 0 }, 12);
  assert.ok(eq.every(([, lat]) => Number.isFinite(lat) && Math.abs(lat) <= 90));
});
