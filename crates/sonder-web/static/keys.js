// Play-along: the computer keyboard (and Web MIDI inputs) as a piano for `human` layers.
//
// Everything goes out as a `human_note` UserCommand — {cmd:'human_note', key, velocity, on} —
// over whichever transport the page uses (WebSocket to the native server, or the wasm
// worker). The composer feeds it to every `role = "human"` layer; no source adapter needed.
// Hardware keyboards on the native engine can use the `human.midi` source instead.
//
// Play mode is toggled with `p` (or the strip's button) so the piano keys do not collide with
// the page shortcuts (digits, space, [ ] - = k m c n e ?). While it is on, key events are
// swallowed in the capture phase before app.js sees them. Layout (DAW style, by key *code*
// so it works on AZERTY/QWERTZ too):
//   bottom row  Z S X D C V G B H N J M ,   = C C# D D# E F F# G G# A A# B C   (low octave)
//   top row     Q 2 W 3 E R 5 T 6 Y 7 U I   = the octave above
//   ↑ / ↓ shift the octave; Esc or p leaves play mode.
//
// The pure parts (layout, MIDI parsing, snapshot queries) are exported for node tests;
// `initKeys` is the only thing that touches the DOM.

import { pitchName } from './roll.js';

/** KeyboardEvent.code → semitones above the piano's low C. */
export const KEY_OFFSETS = Object.freeze({
  KeyZ: 0, KeyS: 1, KeyX: 2, KeyD: 3, KeyC: 4, KeyV: 5, KeyG: 6, KeyB: 7, KeyH: 8, KeyN: 9, KeyJ: 10, KeyM: 11, Comma: 12,
  KeyQ: 12, Digit2: 13, KeyW: 14, Digit3: 15, KeyE: 16, KeyR: 17, Digit5: 18, KeyT: 19, Digit6: 20, KeyY: 21, Digit7: 22, KeyU: 23, KeyI: 24,
});

/** Number of piano keys the strip shows (two octaves plus the top C). */
export const STRIP_KEYS = 25;
/** Default velocity for computer keys (no pressure). */
export const KEY_VELOCITY = 0.75;
export const DEFAULT_OCTAVE = 4; // low C = C4 = 60
export const MIN_OCTAVE = 0;
export const MAX_OCTAVE = 8;

/** Semitone offset for a KeyboardEvent.code, or null when the key is not part of the piano. */
export const keyOffset = (code) => (Object.prototype.hasOwnProperty.call(KEY_OFFSETS, code) ? KEY_OFFSETS[code] : null);

/** MIDI number of the piano's low C for an octave (4 → 60). */
export const lowC = (octave) => 12 * (octave + 1);

/** MIDI number for a key code at an octave, clamped to 0..127; null if not a piano key. */
export function keyToMidi(code, octave = DEFAULT_OCTAVE) {
  const off = keyOffset(code);
  if (off === null) return null;
  return Math.min(127, Math.max(0, lowC(octave) + off));
}

export const clampOctave = (o) => Math.min(MAX_OCTAVE, Math.max(MIN_OCTAVE, o));

/** The `human_note` command for a key. */
export const humanNote = (key, velocity, on) => ({ cmd: 'human_note', key, velocity: on ? velocity : 0, on });

/** Is a pitch class a black key? */
export const isBlack = (midi) => [1, 3, 6, 8, 10].includes(((midi % 12) + 12) % 12);

/** Key codes for a midi number at an octave (so the strip can print the letter). */
export function codesFor(midi, octave = DEFAULT_OCTAVE) {
  const off = midi - lowC(octave);
  return Object.entries(KEY_OFFSETS).filter(([, o]) => o === off).map(([c]) => c);
}

/** Short label for a key code: "z", "2", ",". */
export function codeLabel(code) {
  if (code.startsWith('Key')) return code.slice(3).toLowerCase();
  if (code.startsWith('Digit')) return code.slice(5);
  if (code === 'Comma') return ',';
  return code;
}

/**
 * Decode one Web MIDI message into the same shape the Rust `human.midi` adapter produces:
 * {type:'note_on', channel, key, velocity} | {type:'note_off', channel, key} | {type:'cc', channel, number, value}
 * | {type:'pressure', channel, value} | null. NoteOn with velocity 0 is a NoteOff. Channels are 1-based.
 */
export function parseMidiMessage(bytes) {
  if (!bytes || bytes.length < 1) return null;
  const status = bytes[0];
  if (status < 0x80 || status >= 0xF0) return null;
  const channel = (status & 0x0F) + 1;
  const d1 = bytes.length > 1 ? bytes[1] & 0x7F : null;
  const d2 = bytes.length > 2 ? bytes[2] & 0x7F : null;
  switch (status & 0xF0) {
    case 0x80: return d1 === null ? null : { type: 'note_off', channel, key: d1 };
    case 0x90:
      if (d1 === null || d2 === null) return null;
      return d2 === 0 ? { type: 'note_off', channel, key: d1 } : { type: 'note_on', channel, key: d1, velocity: d2 / 127 };
    case 0xB0: return d1 === null || d2 === null ? null : { type: 'cc', channel, number: d1, value: d2 / 127 };
    case 0xD0: return d1 === null ? null : { type: 'pressure', channel, value: d1 / 127 };
    case 0xA0: return d1 === null || d2 === null ? null : { type: 'pressure', channel, key: d1, value: d2 / 127 };
    default: return null;
  }
}

/** The `human_note` command a MIDI message maps to (null for cc / pressure / unknown). */
export function midiToCommand(msg) {
  if (!msg) return null;
  if (msg.type === 'note_on') return humanNote(msg.key, msg.velocity, true);
  if (msg.type === 'note_off') return humanNote(msg.key, 0, false);
  return null;
}

/** The scene's `human` layers (from an EngineSnapshot). */
export const humanLayers = (snap) => ((snap && snap.layers) || []).filter((l) => l.role === 'human');

/** The latest note a person played (reason "human"), or null. */
export function lastHumanNote(snap) {
  const notes = (snap && snap.notes) || [];
  for (let i = notes.length - 1; i >= 0; i--) if (notes[i].reason === 'human') return notes[i];
  return null;
}

/** "snapped to C#4 · 3 held" style status for the strip. */
export function stripStatus(snap, heldCount) {
  const n = lastHumanNote(snap);
  const parts = [];
  if (n) parts.push(`→ ${pitchName(n.midi)}`);
  if (heldCount) parts.push(`${heldCount} held`);
  return parts.join(' · ');
}

/**
 * Wire the piano strip and the keyboard / MIDI listeners.
 *   root:    the strip container element (hidden until the scene has a human layer)
 *   send:    (cmd) => void — the page's command sender
 *   getSnap: () => latest EngineSnapshot | null
 * Returns { play(on), isPlaying(), octave(), destroy() } for tests and the page.
 */
export function initKeys({ root, send, getSnap, win = typeof window !== 'undefined' ? window : null, doc = typeof document !== 'undefined' ? document : null }) {
  if (!root || !win || !doc) return null;
  const st = { on: false, octave: DEFAULT_OCTAVE, pressed: new Map(), midi: null, visible: false };
  root.innerHTML = `
    <div class="khead">
      <span class="ktitle">you</span>
      <button class="btn kplay" title="play along with the keyboard (p)">play (p)</button>
      <button class="btn ghost kmidi" title="connect a MIDI keyboard (Web MIDI)">MIDI</button>
      <span class="koct mono dim" title="↑ / ↓ shift octave"></span>
      <span class="kstatus mono dim"></span>
    </div>
    <div class="kpiano"></div>`;
  const $ = (sel) => root.querySelector(sel);
  const piano = $('.kpiano'), status = $('.kstatus'), oct = $('.koct'), playBtn = $('.kplay'), midiBtn = $('.kmidi'), title = $('.ktitle');

  const noteOn = (midi, velocity = KEY_VELOCITY) => { send(humanNote(midi, velocity, true)); light(midi, true); };
  const noteOff = (midi) => { send(humanNote(midi, 0, false)); light(midi, false); };
  const light = (midi, on) => { const el = piano.querySelector(`[data-midi="${midi}"]`); if (el) el.classList.toggle('down', on); };
  const releaseAll = () => { for (const midi of new Set(st.pressed.values())) noteOff(midi); st.pressed.clear(); };

  function buildPiano() {
    piano.innerHTML = '';
    const base = lowC(st.octave);
    for (let i = 0; i < STRIP_KEYS; i++) {
      const midi = base + i;
      if (midi > 127) break;
      const k = doc.createElement('div');
      k.className = 'kkey' + (isBlack(midi) ? ' black' : '');
      k.dataset.midi = String(midi);
      const codes = codesFor(midi, st.octave);
      k.innerHTML = `<span class="klabel">${codes.map(codeLabel).join(' ')}</span>` + (midi % 12 === 0 ? `<span class="kname">${pitchName(midi)}</span>` : '');
      k.title = pitchName(midi);
      k.addEventListener('pointerdown', (e) => { e.preventDefault(); k.setPointerCapture?.(e.pointerId); noteOn(midi); });
      const up = () => noteOff(midi);
      k.addEventListener('pointerup', up);
      k.addEventListener('pointercancel', up);
      piano.appendChild(k);
    }
    oct.textContent = `${pitchName(base)}–${pitchName(Math.min(127, base + STRIP_KEYS - 1))}`;
  }

  function setOctave(o) {
    const next = clampOctave(o);
    if (next === st.octave) return;
    releaseAll();
    st.octave = next;
    buildPiano();
  }

  function play(on) {
    st.on = !!on;
    if (!st.on) releaseAll();
    playBtn.classList.toggle('on', st.on);
    playBtn.textContent = st.on ? 'playing · Esc' : 'play (p)';
    root.classList.toggle('playing', st.on);
  }

  // Capture-phase listener: in play mode the piano wins over the page shortcuts.
  const onKeyDown = (e) => {
    const tag = (e.target && e.target.tagName) || '';
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || e.metaKey || e.ctrlKey || e.altKey) return;
    if (!st.visible) return;
    if (!st.on) {
      if (e.key === 'p' || e.key === 'P') { play(true); e.preventDefault(); e.stopImmediatePropagation(); }
      return;
    }
    e.stopImmediatePropagation();
    e.preventDefault();
    if (e.key === 'Escape' || e.key === 'p' || e.key === 'P') { play(false); return; }
    if (e.key === 'ArrowUp') { setOctave(st.octave + 1); return; }
    if (e.key === 'ArrowDown') { setOctave(st.octave - 1); return; }
    if (e.repeat || st.pressed.has(e.code)) return;
    const midi = keyToMidi(e.code, st.octave);
    if (midi === null) return;
    st.pressed.set(e.code, midi);
    noteOn(midi);
  };
  const onKeyUp = (e) => {
    const midi = st.pressed.get(e.code);
    if (midi === undefined) return;
    st.pressed.delete(e.code);
    noteOff(midi);
    if (st.on) e.stopImmediatePropagation();
  };
  const onBlur = () => releaseAll();
  win.addEventListener('keydown', onKeyDown, true);
  win.addEventListener('keyup', onKeyUp, true);
  win.addEventListener('blur', onBlur);

  playBtn.addEventListener('click', () => play(!st.on));

  // Web MIDI: needs a user gesture; every input port feeds the same human layers.
  async function connectMidi() {
    if (!win.navigator || !win.navigator.requestMIDIAccess) { midiBtn.textContent = 'no Web MIDI'; midiBtn.disabled = true; return; }
    try {
      midiBtn.disabled = true;
      st.midi = await win.navigator.requestMIDIAccess({ sysex: false });
      const attach = () => {
        let n = 0;
        for (const input of st.midi.inputs.values()) {
          n++;
          input.onmidimessage = (ev) => {
            const msg = parseMidiMessage(ev.data);
            const cmd = midiToCommand(msg);
            if (cmd) { send(cmd); light(cmd.key, cmd.on); }
          };
        }
        midiBtn.textContent = n ? `MIDI · ${n} in` : 'MIDI · no inputs';
        midiBtn.classList.toggle('on', n > 0);
      };
      attach();
      st.midi.onstatechange = attach;
    } catch (e) {
      console.warn('Web MIDI unavailable', e);
      midiBtn.textContent = 'MIDI denied';
    } finally {
      midiBtn.disabled = false;
    }
  }
  midiBtn.addEventListener('click', connectMidi);

  // Follow the snapshot: show the strip only when the scene has a human layer.
  const timer = setInterval(() => {
    const snap = getSnap ? getSnap() : null;
    const humans = humanLayers(snap);
    const visible = humans.length > 0;
    if (visible !== st.visible) {
      st.visible = visible;
      root.hidden = !visible;
      if (!visible) play(false);
    }
    if (!visible) return;
    title.textContent = humans.map((l) => l.name).join(', ');
    const s = stripStatus(snap, st.pressed.size);
    if (status.textContent !== s) status.textContent = s;
  }, 100);

  buildPiano();
  play(false);
  return {
    play,
    isPlaying: () => st.on,
    octave: () => st.octave,
    setOctave,
    destroy() {
      clearInterval(timer);
      releaseAll();
      win.removeEventListener('keydown', onKeyDown, true);
      win.removeEventListener('keyup', onKeyUp, true);
      win.removeEventListener('blur', onBlur);
    },
  };
}
