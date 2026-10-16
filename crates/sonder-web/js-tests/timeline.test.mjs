// Replay timeline math (time travel): clock formatting, scrub geometry, speed buttons,
// gzip detection and header peeking.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { gzipSync } from 'node:zlib';

const { fmtClock, fraction, seekTarget, activeSpeed, togglePause, wallLabel, replayVisible, isGzip, looksLikeLog, peekHeader, readLogFile, SPEEDS } =
  await import('../static/timeline.js');

test('clock formatting', () => {
  assert.equal(fmtClock(0), '0:00');
  assert.equal(fmtClock(65.9), '1:05');
  assert.equal(fmtClock(3600 + 7 * 60 + 3), '1:07:03');
  assert.equal(fmtClock(-4), '0:00');
  assert.equal(fmtClock(NaN), '0:00');
});

test('position fraction clamps and survives empty logs', () => {
  assert.equal(fraction(30, 120), 0.25);
  assert.equal(fraction(200, 120), 1);
  assert.equal(fraction(-1, 120), 0);
  assert.equal(fraction(5, 0), 0);
  assert.equal(fraction(NaN, 10), 0);
});

test('a click on the bar maps to log seconds', () => {
  assert.equal(seekTarget(0, 400, 600), 0);
  assert.equal(seekTarget(100, 400, 600), 150);
  assert.equal(seekTarget(400, 400, 600), 600);
  assert.equal(seekTarget(900, 400, 600), 600, 'clamped past the end');
  assert.equal(seekTarget(-5, 400, 600), 0);
  assert.equal(seekTarget(10, 0, 600), null);
  assert.equal(seekTarget(10, 400, 0), null);
});

test('speed buttons: nearest lights, paused lights none, pause toggles', () => {
  assert.deepEqual(SPEEDS, [0.5, 1, 2, 4]);
  assert.equal(activeSpeed(1), 1);
  assert.equal(activeSpeed(0.6), 0.5);
  assert.equal(activeSpeed(3), 4, 'log-nearest: 3 is closer to 4 than to 2 on a log scale? 3/2=1.5, 4/3=1.33 → 4');
  assert.equal(activeSpeed(8), 4);
  assert.equal(activeSpeed(0), null);
  assert.equal(togglePause(1), 0);
  assert.equal(togglePause(2, 2), 0);
  assert.equal(togglePause(0, 2), 2);
  assert.equal(togglePause(0), 1);
  assert.equal(togglePause(0, 0), 1);
});

test('wall label follows the epoch and position', () => {
  assert.equal(wallLabel(0, 10), '');
  const a = wallLabel(1_760_500_000, 0, 'en-US');
  const b = wallLabel(1_760_500_000, 3600, 'en-US');
  assert.ok(a.length > 8 && a !== b);
});

test('visibility follows snapshot.replay', () => {
  assert.equal(replayVisible(null), false);
  assert.equal(replayVisible({}), false);
  assert.equal(replayVisible({ replay: null }), false);
  assert.equal(replayVisible({ replay: { position: 1, length: 10, speed: 1 } }), true);
});

test('gzip detection, name sniffing and header peek', async () => {
  const header = { sonder: 1, scene: 'Night Freeway', seed: 7, epoch_unix: 1, created: '1970-01-01T00:00:01Z', sources: [] };
  const text = `${JSON.stringify(header)}\n{"t":0.02,"s":"traffic","d":{"type":"reading","values":{"count":3}}}\n`;
  const plain = new TextEncoder().encode(text);
  const gz = new Uint8Array(gzipSync(Buffer.from(text)));
  assert.equal(isGzip(plain), false);
  assert.equal(isGzip(gz), true);
  assert.equal(isGzip(new Uint8Array(1)), false);
  assert.ok(looksLikeLog('2026-10-15.sonder.gz'));
  assert.ok(looksLikeLog('x.sonder'));
  assert.ok(looksLikeLog('x.jsonl'));
  assert.ok(!looksLikeLog('x.mp3'));
  assert.deepEqual(peekHeader(plain), header);
  assert.equal(peekHeader(new TextEncoder().encode('nope\n')), null);
  assert.equal(peekHeader(new TextEncoder().encode('{"a":1}\n')), null);
  // readLogFile inflates gzip (Node has Blob, DecompressionStream and Response).
  const file = { arrayBuffer: async () => gz.buffer.slice(gz.byteOffset, gz.byteOffset + gz.byteLength) };
  const out = await readLogFile(file);
  assert.equal(new TextDecoder().decode(out), text);
  const same = await readLogFile({ arrayBuffer: async () => plain.buffer });
  assert.equal(new TextDecoder().decode(same), text);
});
