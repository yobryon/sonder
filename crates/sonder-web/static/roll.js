// Piano-roll geometry and attribution helpers, pure so node can test them.
//
// The roll draws ~256 note rects per frame; hovering must not cost that again, so the frame
// loop stores the rects it drew (`noteRect`) and the pointer is tested against that list
// (`hitNote`) — a scan of a few hundred boxes, no canvas work.

export const PC_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];

/** "C#4" for 61; cents dropped. */
export function pitchName(midi) {
  const m = Math.round(midi);
  return `${PC_NAMES[((m % 12) + 12) % 12]}${Math.floor(m / 12) - 1}`;
}

/**
 * Where a note draws: `now` is engine time at the right edge, `W` the width, `top`/`rh` the
 * roll band. Returns null when the note is fully off-screen (left) or not yet visible.
 */
export function noteRect(n, { now, W, top, rh, pxPerSec, noteH, midiLo = 36, midiHi = 96 }) {
  const start = now - n.at;
  const dur = n.duration ?? (n.active ? start : 0.3);
  const x1 = W - start * pxPerSec;
  const w = Math.max(3, dur * pxPerSec);
  if (x1 + w < 0 || x1 > W) return null;
  const f = Math.min(1, Math.max(0, (n.midi - midiLo) / (midiHi - midiLo)));
  const y = top + (1 - f) * rh - noteH / 2;
  return { x: x1, y, w: Math.min(w, W - x1 + 2), h: noteH };
}

/**
 * The topmost (last drawn) rect under (x, y), with `pad` px of slack so thin notes are
 * hittable. Each entry is {x, y, w, h, ...payload}; returns the entry or null.
 */
export function hitNote(rects, x, y, pad = 2) {
  for (let i = rects.length - 1; i >= 0; i--) {
    const r = rects[i];
    if (x >= r.x - pad && x <= r.x + r.w + pad && y >= r.y - pad && y <= r.y + r.h + pad) return r;
  }
  return null;
}

/** Did `event` trigger `note`? Same layer, same event name, same arrival time. */
export function causedBy(note, event) {
  const c = note && note.cause;
  if (!c || !event) return false;
  return note.layer === event.layer && c.event === event.name && Math.abs((c.at ?? 0) - (event.at ?? 0)) < 1e-6;
}

/** Short attr summary "k=v k=v k=v" (first `n`). */
export function attrSummary(attrs, n = 3) {
  return Object.entries(attrs || {}).slice(0, n).map(([k, v]) => `${k}=${fmtAttrValue(v)}`).join(' ');
}

export function fmtAttrValue(v) {
  if (typeof v === 'number') {
    if (!isFinite(v)) return String(v);
    const a = Math.abs(v);
    if (Number.isInteger(v) || a >= 1000) return v.toFixed(0);
    return a >= 100 ? v.toFixed(1) : v.toFixed(2);
  }
  if (typeof v === 'boolean') return v ? 'yes' : 'no';
  const s = String(v ?? '');
  return s.length > 18 ? s.slice(0, 17) + '…' : s;
}

/** The one-liner for an event or a note cause: the engine's label, else name + attrs. */
export function causeText(c) {
  if (!c) return '';
  if (c.label) return c.label;
  const attrs = attrSummary(c.attrs);
  const name = c.event ?? c.name ?? '';
  return attrs ? `${name} · ${attrs}` : name;
}
