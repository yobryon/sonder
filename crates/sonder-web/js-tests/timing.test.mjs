// The Worker's catch-up math: keep ~250 ms ahead, never spin after a long sleep.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { planTicks, LEAD_SEC, MAX_BEHIND_SEC, MAX_TICKS_PER_SLICE } from '../static/timing.js';

const TICK = 0.02;

test('cold start renders the whole lead at once', () => {
  const p = planTicks({ engineTime: 0, elapsed: 0, tickSec: TICK });
  assert.equal(p.ticks, Math.round(LEAD_SEC / TICK)); // 13 → 12.5 rounds up to 13
  assert.equal(p.skip, 0);
});

test('steady state: one tick per 20 ms of wall time, sleep ≤ 20 ms', () => {
  let engine = 0.26, t0 = 0;
  for (let step = 1; step <= 50; step++) {
    const elapsed = step * 0.02;
    const p = planTicks({ engineTime: engine, elapsed: elapsed - t0, tickSec: TICK });
    assert.ok(p.ticks <= 2, `step ${step} wanted ${p.ticks} ticks`);
    assert.ok(p.nextMs >= 0 && p.nextMs <= 20);
    engine += p.ticks * TICK;
    assert.ok(engine - (elapsed - t0) >= LEAD_SEC - TICK, 'engine fell behind the lead');
    assert.ok(engine - (elapsed - t0) <= LEAD_SEC + TICK, 'engine ran too far ahead');
  }
});

test('nothing to do when far enough ahead', () => {
  const p = planTicks({ engineTime: 1.0, elapsed: 0.6, tickSec: TICK });
  assert.equal(p.ticks, 0);
  assert.ok(p.nextMs > 0);
});

test('a long sleep is capped: at most maxBehind of catch-up, the rest skipped', () => {
  const p = planTicks({ engineTime: 10, elapsed: 70, tickSec: TICK }); // tab slept 60 s
  assert.ok(Math.abs(p.skip - (60 - MAX_BEHIND_SEC)) < 1e-9);
  // After the skip we are exactly maxBehind behind: render that plus the lead, bounded per slice.
  const want = Math.ceil((MAX_BEHIND_SEC + LEAD_SEC) / TICK);
  assert.equal(p.ticks, Math.min(want, MAX_TICKS_PER_SLICE));
  assert.ok(p.nextMs <= 20);
});

test('a slice never renders more than MAX_TICKS_PER_SLICE and comes straight back when capped', () => {
  const p = planTicks({ engineTime: 0, elapsed: 0.9, tickSec: 0.005, lead: 0.25, maxBehind: 1 }); // wants 230 ticks
  assert.equal(p.ticks, MAX_TICKS_PER_SLICE);
  assert.equal(p.skip, 0);
  assert.equal(p.nextMs, 0);
});

test('a short hiccup under maxBehind catches up fully without skipping', () => {
  const p = planTicks({ engineTime: 5, elapsed: 5.5, tickSec: TICK });
  assert.equal(p.skip, 0);
  assert.equal(p.ticks, Math.ceil((0.5 + LEAD_SEC) / TICK));
});

test('rejects a non-positive tick', () => {
  assert.throws(() => planTicks({ engineTime: 0, elapsed: 0, tickSec: 0 }));
});
