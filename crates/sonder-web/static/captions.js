// Caption strip math: which captions are new, how long each lives, how faded it is.
// Pure functions over `EngineSnapshot.captions` entries ({at, layer, text, importance}).

const clamp01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v);

/** Seconds a caption stays on screen: 4 s for trivia, 10 s for the big moments. */
export const captionLife = (importance) => 4 + 6 * clamp01(importance ?? 0);

/** Dedupe key: the engine repeats its bounded window every snapshot. */
export const captionKey = (c) => `${c.at}|${c.text}`;

/** Big type for the moments that deserve it. */
export const isBig = (c) => (c.importance ?? 0) >= 0.8;

/**
 * Captions in `incoming` not yet in `seen` (a Set of keys, updated in place), oldest first,
 * each stamped with `wall` (ms) as its arrival time. Blank texts are skipped.
 */
export function newCaptions(incoming, seen, wall) {
  const out = [];
  for (const c of incoming || []) {
    if (!c || !c.text) continue;
    const k = captionKey(c);
    if (seen.has(k)) continue;
    seen.add(k);
    out.push({ ...c, key: k, wall, life: captionLife(c.importance) });
  }
  return out;
}

/** Opacity 0..1 at `wall` ms: solid for the first 60 % of its life, then a linear fade. */
export function captionAlpha(c, wall) {
  const age = (wall - c.wall) / 1000;
  if (age < 0) return 1;
  const fadeFrom = c.life * 0.6;
  if (age <= fadeFrom) return 1;
  if (age >= c.life) return 0;
  return 1 - (age - fadeFrom) / (c.life - fadeFrom);
}

/** Drop dead captions and return the live ones newest first (capped). */
export function liveCaptions(list, wall, max = 6) {
  const alive = list.filter((c) => captionAlpha(c, wall) > 0);
  alive.sort((a, b) => b.wall - a.wall || b.at - a.at);
  return alive.slice(0, max);
}
