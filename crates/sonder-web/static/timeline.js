// Replay timeline (time travel). Shown only while `snapshot.replay` is present: a strip
// with the log's wall-clock date, position / length, a scrub bar (click to seek), speed
// buttons (½ 1 2 4) and pause. Sends `seek` / `set_speed` UserCommands through `send`,
// so it works the same against the wasm Worker and the native WebSocket.
//
// The math is kept in pure functions so `node --test` can exercise it; the DOM lives in
// `createTimeline`, which app.js calls once and feeds from `ingest`.

export const SPEEDS = [0.5, 1, 2, 4];

/** `mm:ss` or `h:mm:ss`. */
export function fmtClock(sec) {
  const t = Number.isFinite(sec) && sec > 0 ? Math.floor(sec) : 0;
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}` : `${m}:${String(s).padStart(2, '0')}`;
}

/** Position as a 0..1 fraction of the length (0 for an empty log). */
export function fraction(position, length) {
  if (!(length > 0) || !Number.isFinite(position)) return 0;
  return Math.min(1, Math.max(0, position / length));
}

/**
 * Log seconds for a click at `x` within a bar of width `width` (clamped to the bar).
 * Returns null when the bar has no size or the log no length.
 */
export function seekTarget(x, width, length) {
  if (!(width > 0) || !(length > 0) || !Number.isFinite(x)) return null;
  return Math.min(1, Math.max(0, x / width)) * length;
}

/** The speed button to light for a replay speed (nearest; 0 = paused lights none). */
export function activeSpeed(speed) {
  if (!(speed > 0)) return null;
  let best = SPEEDS[0];
  for (const s of SPEEDS) if (Math.abs(Math.log(s / speed)) < Math.abs(Math.log(best / speed))) best = s;
  return best;
}

/** Toggling pause: 0 → the remembered speed (1 if none), else 0. */
export function togglePause(speed, remembered = 1) {
  return speed > 0 ? 0 : (remembered > 0 ? remembered : 1);
}

/** Wall-clock label for a log position: the log's epoch plus the position, local time. */
export function wallLabel(epochUnix, position, locale = undefined) {
  if (!(epochUnix > 0)) return '';
  const d = new Date((epochUnix + (position || 0)) * 1000);
  if (Number.isNaN(d.getTime())) return '';
  return d.toLocaleString(locale, { dateStyle: 'medium', timeStyle: 'medium' });
}

/** Is a replay strip warranted for this snapshot? */
export function replayVisible(snap) {
  return !!(snap && snap.replay && Number.isFinite(snap.replay.length));
}

/**
 * Build the strip inside `root` (a container element). `send(cmd)` posts UserCommands.
 * Returns `{ update(replay), hide() }`; `update(null)` hides it.
 */
export function createTimeline(root, { send, doc = document } = {}) {
  root.innerHTML = `
    <span class="tl-tag" title="playing a recorded data log">⟲ replay</span>
    <span class="tl-title mono"></span>
    <span class="tl-wall dim"></span>
    <div class="tl-bar" title="click to seek"><i class="tl-fill"></i><i class="tl-head"></i></div>
    <span class="tl-time mono"><b class="tl-pos">0:00</b> / <span class="tl-len">0:00</span></span>
    <span class="tl-speeds"></span>
    <button class="btn ghost tl-pause" title="pause / resume the log (speed 0)">❚❚</button>`;
  const $ = (sel) => root.querySelector(sel);
  const bar = $('.tl-bar'), fill = $('.tl-fill'), head = $('.tl-head');
  const speeds = $('.tl-speeds');
  let last = null;        // last replay state
  let remembered = 1;     // speed to resume at after a pause
  for (const s of SPEEDS) {
    const b = doc.createElement('button');
    b.className = 'btn ghost tl-speed';
    b.dataset.speed = String(s);
    b.textContent = s === 0.5 ? '½' : `${s}×`;
    b.title = `play at ${s}×`;
    b.addEventListener('click', () => { remembered = s; send({ cmd: 'set_speed', speed: s }); });
    speeds.appendChild(b);
  }
  $('.tl-pause').addEventListener('click', () => {
    if (!last) return;
    const next = togglePause(last.speed, remembered);
    if (last.speed > 0) remembered = last.speed;
    send({ cmd: 'set_speed', speed: next });
  });
  bar.addEventListener('click', (e) => {
    if (!last) return;
    const r = bar.getBoundingClientRect();
    const to = seekTarget(e.clientX - r.left, r.width, last.length);
    if (to !== null) send({ cmd: 'seek', to });
  });
  const setText = (el, t) => { if (el.textContent !== t) el.textContent = t; };
  return {
    update(replay) {
      if (!replay) { this.hide(); return; }
      root.hidden = false;
      last = replay;
      const f = fraction(replay.position, replay.length);
      fill.style.width = `${(f * 100).toFixed(2)}%`;
      head.style.left = `${(f * 100).toFixed(2)}%`;
      setText($('.tl-pos'), fmtClock(replay.position));
      setText($('.tl-len'), fmtClock(replay.length));
      setText($('.tl-title'), replay.title || '');
      setText($('.tl-wall'), wallLabel(replay.epoch_unix, replay.position));
      const active = activeSpeed(replay.speed);
      for (const b of speeds.children) b.classList.toggle('on', Number(b.dataset.speed) === active);
      const paused = !(replay.speed > 0);
      $('.tl-pause').textContent = paused ? '▶' : '❚❚';
      $('.tl-pause').classList.toggle('on', paused);
    },
    hide() { root.hidden = true; last = null; },
  };
}

/**
 * Read a dropped / picked log file into plain JSONL bytes: gzip (by magic bytes) is
 * inflated here with DecompressionStream, so the wasm engine only ever sees text.
 */
export async function readLogFile(file) {
  const buf = new Uint8Array(await file.arrayBuffer());
  if (!isGzip(buf)) return buf;
  if (typeof DecompressionStream !== 'function') throw new Error('this browser cannot inflate .gz logs; unzip it first');
  const stream = new Blob([buf]).stream().pipeThrough(new DecompressionStream('gzip'));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

export function isGzip(bytes) {
  return bytes && bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b;
}

/** Does this look like a Sonder log by name? (`.sonder`, `.sonder.gz`, `.jsonl`) */
export function looksLikeLog(name) {
  return /\.(sonder(\.gz)?|jsonl)$/i.test(name || '');
}

/** The header line of a log's bytes (already inflated), or null. */
export function peekHeader(bytes) {
  try {
    const n = Math.min(bytes.length, 65536);
    const text = new TextDecoder().decode(bytes.subarray(0, n));
    const line = text.split('\n', 1)[0];
    const h = JSON.parse(line);
    return h && typeof h === 'object' && 'sonder' in h ? h : null;
  } catch (_) {
    return null;
  }
}
