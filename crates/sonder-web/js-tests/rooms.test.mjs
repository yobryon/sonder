// Room protocol helpers: URLs, frame parsing, clock-offset math, sample unpacking.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { joinUrl, roomId, parseFrame, estimateOffset, pickOffset, sampleItems, helloSeed } = await import('../static/rooms.js');

test('seeds past 2^53 survive via seed_str', () => {
  const h = JSON.parse('{"type":"hello","seed":10933517196051543210,"seed_str":"10933517196051543210"}');
  assert.equal(helloSeed(h), '10933517196051543210');
  assert.equal(helloSeed({ seed: 7 }), '7');
  assert.equal(helloSeed({ seed_str: 'x', seed: 7 }), '7');
  assert.equal(helloSeed({}), null);
});

test('join URLs', () => {
  assert.equal(joinUrl('wss://relay.example/r/abc'), 'wss://relay.example/r/abc/join');
  assert.equal(joinUrl('ws://127.0.0.1:8788/r/test/'), 'ws://127.0.0.1:8788/r/test/join');
  assert.equal(joinUrl('https://relay.example/r/abc/host'), 'wss://relay.example/r/abc/join');
  assert.equal(joinUrl('relay.example/r/x_1-2'), 'wss://relay.example/r/x_1-2/join');
  assert.equal(joinUrl('wss://relay.example/nope'), null);
  assert.equal(joinUrl('wss://relay.example/r/bad id'), null);
  assert.equal(joinUrl(''), null);
  assert.equal(roomId('wss://relay.example/r/abc/join'), 'abc');
  assert.equal(roomId('garbage'), '');
});

test('frames parse defensively', () => {
  assert.deepEqual(parseFrame('{"type":"presence","listeners":2}'), { type: 'presence', listeners: 2 });
  assert.equal(parseFrame('{"listeners":2}'), null);
  assert.equal(parseFrame('not json'), null);
  assert.equal(parseFrame('[1]'), null);
});

test('offset: host time advances by half the round trip; bad pongs are ignored', () => {
  // Sent at 1000 ms, answered at 1100 ms: rtt 100 ms → host is at 50.05 s now; we are at 10 s.
  const s = estimateOffset({ type: 'pong', sent: 1000, host_time: 50.0, host_age_ms: 20, present: true }, 10.0, 1100);
  assert.equal(s.rtt, 100);
  assert.ok(Math.abs(s.offset - 40.05) < 1e-9, String(s.offset));
  assert.equal(estimateOffset({ type: 'pong', sent: 1000, host_time: 50.0, host_age_ms: -1 }, 10, 1100), null, 'no host snapshot yet');
  assert.equal(estimateOffset({ type: 'pong', sent: 1000, host_time: 50.0, present: false }, 10, 1100), null, 'host gone');
  assert.equal(estimateOffset({ type: 'pong', sent: 'x', host_time: 50.0 }, 10, 1100), null);
  assert.equal(estimateOffset(null, 10, 1100), null);
});

test('pickOffset takes the lowest-rtt recent sample', () => {
  const samples = [
    { offset: 40.9, rtt: 900 }, // old spike
    { offset: 40.1, rtt: 120 },
    { offset: 40.3, rtt: 400 },
    { offset: 40.0, rtt: 80 },
    { offset: 40.2, rtt: 150 },
    { offset: 40.4, rtt: 300 },
  ];
  assert.equal(pickOffset(samples), 40.0);
  assert.equal(pickOffset(samples, 2), 40.2);
  assert.equal(pickOffset([]), 0);
  assert.equal(pickOffset([{ offset: NaN, rtt: 1 }]), 0);
});

test('sample batches unpack to postSample arguments', () => {
  const items = sampleItems({ type: 'samples', items: [
    { t: 1.5, s: 'traffic', d: { type: 'reading', values: { count: 3 } } },
    { t: 1.6, s: 'quakes', d: { type: 'event', name: 'quake', intensity: 0.8, attrs: { mag: 5.1 } } },
    { t: 1.7, s: 'bad' },
    null,
  ] });
  assert.deepEqual(items, [
    ['traffic', '{"type":"reading","values":{"count":3}}'],
    ['quakes', '{"type":"event","name":"quake","intensity":0.8,"attrs":{"mag":5.1}}'],
  ]);
  assert.deepEqual(sampleItems({}), []);
});
