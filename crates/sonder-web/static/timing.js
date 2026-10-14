// Pure timing math for the engine Worker (kept dependency-free so `node --test` can
// exercise it). The Worker owns the clock: it keeps the engine `lead` seconds ahead of
// wall time, rendering 20 ms blocks, and when the thread was asleep for long (hidden
// tab, laptop lid) it abandons all but `maxBehind` seconds of catch-up so a long sleep
// never turns into a burst of thousands of ticks.

export const LEAD_SEC = 0.25;
export const MAX_BEHIND_SEC = 1.0;
export const MAX_TICKS_PER_SLICE = 100;

/**
 * Decide what to do on one wake-up.
 * @param {object} a
 * @param {number} a.engineTime  seconds the engine has rendered so far
 * @param {number} a.elapsed     wall seconds since the engine started (already offset by earlier skips)
 * @param {number} a.tickSec     seconds per tick (frames_per_tick / sample_rate, 0.02)
 * @param {number} [a.lead]      how far ahead of wall time the engine should sit
 * @param {number} [a.maxBehind] cap on catch-up after a long sleep
 * @returns {{ticks:number, skip:number, nextMs:number}}
 *   ticks  – how many ticks to render now
 *   skip   – wall seconds to drop (add to the start timestamp) because we were too far behind
 *   nextMs – how long to sleep before the next wake-up
 */
export function planTicks({ engineTime, elapsed, tickSec, lead = LEAD_SEC, maxBehind = MAX_BEHIND_SEC }) {
  if (!(tickSec > 0)) throw new Error('tickSec must be > 0');
  let skip = 0;
  const behind = elapsed - engineTime;
  if (behind > maxBehind) { skip = behind - maxBehind; elapsed -= skip; }
  const target = elapsed + lead;                     // engine time we want to reach
  let ticks = Math.max(0, Math.ceil((target - engineTime) / tickSec - 1e-9));
  if (ticks > MAX_TICKS_PER_SLICE) ticks = MAX_TICKS_PER_SLICE;
  const after = engineTime + ticks * tickSec;
  // Wake when the engine lead has shrunk to half a tick, never later than 20 ms.
  const nextMs = Math.max(0, Math.min(20, (after - lead - elapsed + tickSec / 2) * 1000));
  return { ticks, skip, nextMs };
}
