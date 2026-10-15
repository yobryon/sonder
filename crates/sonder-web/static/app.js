// Sonder web UI. Vanilla ES module, no build step, no network beyond this host.
//
// Data in:  EngineSnapshot JSON at ~20-25 Hz (see sonder-core/src/snapshot.rs)
// Data out: UserCommand JSON  {"cmd":"toggle_mute","layer":0}
// Audio:    interleaved stereo f32 chunks → AudioWorklet (worklet.js)
//
// Where it all comes from is transport.js's business: `ws` mode talks to the native
// Sonder server (/ws, /audio); `worker` mode runs the wasm engine in worker.js with
// JS source adapters, no server at all. Everything drawn here comes from the snapshot,
// so both modes look identical.

'use strict';

import { connect } from './transport.js';
import { decodeScene, scenePayloadFromHash, sceneLink } from './scene-url.js';
import { newCaptions, liveCaptions, isBig } from './captions.js';
import { noteRect, hitNote, causedBy, pitchName, causeText, attrSummary } from './roll.js';
import { project, eventLatLon, sunPosition, nightPolygon } from './geo.js';
import { createEditor, sceneTitle } from './editor.js';
// The source-adapter registry is only needed in worker mode and is not served by the
// native server, so it is imported lazily (a 404 would otherwise sink this whole module).
let registry = null;
const loadRegistry = async () => (registry ||= await import('./sources/index.js'));

const $ = (sel) => document.querySelector(sel);
const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);
const lerp = (a, b, t) => a + (b - a) * t;

// ---------------------------------------------------------------------------
// Colors: one hue per layer from the golden angle, so neighbours differ.
// ---------------------------------------------------------------------------
const hue = (i) => (i * 137.508 + 28) % 360;
const col = (i, a = 1, l = 62, s = 70) => `hsla(${hue(i).toFixed(1)},${s}%,${l}%,${a})`;

// ---------------------------------------------------------------------------
// Client state
// ---------------------------------------------------------------------------
const HIST = 600; // ~30 s at 20 Hz
const state = {
  snap: null,
  recvAt: 0,
  tick: 0,
  hist: new Map(),        // layerId -> Map(signal -> {buf, i, n})
  events: [],             // deduped, oldest first, with .wall (Date.now at receipt)
  seenEvents: new Map(),  // dedupe key -> how many events with that key we've ingested
  notes: new Map(),       // key -> NoteSnapshot (latest copy)
  rings: [],              // transient flashes on the stage {x,y,layer,t0,intensity}
  pings: [],              // map pings {x,y,layer,t0,intensity}
  laneY: new Map(),       // freeway: lane -> smoothed y
  layerSig: '',
  sourceSig: '',
  tickerDirty: true,
  captions: [],           // live captions {at,text,importance,wall,life,key}
  seenCaptions: new Set(),
  geoSeen: false,         // any event with lat/lon so far → map mode is sensible
  noteRects: [],          // rects drawn by the last roll frame, for hover hit-testing
  hover: null,            // {kind:'note'|'ping', ...} under the pointer
  pinned: null,           // note key whose tooltip is pinned (click)
  hlEvent: null,          // ticker event whose notes are highlighted (click)
  pointer: { x: 0, y: 0, inside: false },
};

function engineNow() {
  if (!state.snap) return 0;
  const dt = (performance.now() - state.recvAt) / 1000;
  return state.snap.time + clamp(dt, 0, 0.5);
}

// ---------------------------------------------------------------------------
// Snapshot ingest
// ---------------------------------------------------------------------------
function histFor(layerId, name) {
  let m = state.hist.get(layerId);
  if (!m) { m = new Map(); state.hist.set(layerId, m); }
  let h = m.get(name);
  if (!h) { h = { buf: new Float32Array(HIST), i: 0, n: 0 }; m.set(name, h); }
  return h;
}

function pushHist(h, v) {
  h.buf[h.i] = v;
  h.i = (h.i + 1) % HIST;
  if (h.n < HIST) h.n++;
}

function eventPosition(e, snap) {
  const a = e.attrs || {};
  const num = (k) => (typeof a[k] === 'number' ? a[k] : null);
  const ll = eventLatLon(a);
  if (ll) return { x: (ll.lon + 180) / 360, y: (90 - ll.lat) / 180 };
  const x = num('x');
  let y = num('y');
  if (y === null && num('lane') !== null) {
    const ly = state.laneY.get(num('lane'));
    y = ly !== undefined ? ly : (num('lane') + 0.5) / Math.max(1, state.laneY.size || 4);
  }
  if (x !== null || y !== null) return { x: x ?? 0.5, y: y ?? 0.5 };
  // Generic: place in the layer's band at the right edge.
  const n = Math.max(1, (snap.layers || []).length);
  const idx = (snap.layers || []).findIndex((l) => l.id === e.layer);
  return { x: 0.96, y: (Math.max(0, idx) + 0.5) / n };
}

const eventKey = (e) => `${e.at}|${e.layer}|${e.name}`;
const noteKey = (n) => `${n.at}|${n.layer}|${n.midi}`;

// Count events per dedupe key.
function countEvents(events) {
  const m = new Map();
  for (const e of events) { const k = eventKey(e); m.set(k, (m.get(k) || 0) + 1); }
  return m;
}

// Events in `events` (a snapshot's recent-events window) not yet ingested, in
// order. Events share `at` within an engine tick, so the key is not unique: we
// compare per-key counts against `seen` (updated in place) instead of a set, so
// two cars leaving in the same tick both show up.
function newEvents(events, seen) {
  const out = [];
  const here = new Map();
  for (const e of events) {
    const k = eventKey(e);
    const c = (here.get(k) || 0) + 1;
    here.set(k, c);
    if (c <= (seen.get(k) || 0)) continue;
    seen.set(k, c);
    out.push(e);
  }
  return out;
}

function ingest(snap) {
  state.snap = snap;
  state.recvAt = performance.now();
  state.tick++;

  // Learn lane geometry from the freeway viz before placing events.
  for (const v of Object.values(snap.viz || {})) {
    if (v && Array.isArray(v.cars)) {
      for (const c of v.cars) {
        if (typeof c.lane !== 'number' || typeof c.y !== 'number') continue;
        const prev = state.laneY.get(c.lane);
        state.laneY.set(c.lane, prev === undefined ? c.y : lerp(prev, c.y, 0.1));
      }
    }
  }

  for (const l of snap.layers || []) {
    for (const [k, v] of Object.entries(l.signals || {})) pushHist(histFor(l.id, k), v);
  }

  const wall = Date.now();
  for (const e of newEvents(snap.events || [], state.seenEvents)) {
    state.events.push({ ...e, wall });
    state.tickerDirty = true;
    const p = eventPosition(e, snap);
    const ring = { x: p.x, y: p.y, layer: e.layer, t0: performance.now(), intensity: clamp(e.intensity ?? 0.5, 0, 1.5), event: e };
    state.rings.push(ring);
    if (eventLatLon(e.attrs)) { state.pings.push(ring); state.geoSeen = true; }
  }
  if (state.pings.length > 400) state.pings.splice(0, state.pings.length - 400);
  pushCaptions(newCaptions(snap.captions, state.seenCaptions, wall));
  if (state.seenCaptions.size > 2000) state.seenCaptions = new Set(state.captions.map((c) => c.key));
  if (state.events.length > 200) state.events.splice(0, state.events.length - 200);
  if (state.seenEvents.size > 4000) state.seenEvents = countEvents(state.events);

  const inSnap = new Set();
  for (const n of snap.notes || []) {
    const k = noteKey(n);
    inSnap.add(k);
    state.notes.set(k, n);
  }
  // A note that left the snapshot window while we still had it as active has
  // ended (or the engine restarted); otherwise it would be drawn forever.
  for (const n of state.notes.values()) if (n.active && !inSnap.has(noteKey(n))) n.active = false;
  const horizon = snap.time - 20;
  for (const [k, n] of state.notes) if (n.at < horizon && !n.active) state.notes.delete(k);

  updateHeader(snap);
  if (state.tick % 2 === 0) updateLayers(snap);
  if (state.tick % 4 === 0) updateSources(snap);
  if (state.tickerDirty) { updateTicker(); state.tickerDirty = false; }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------
const PC_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B'];
const textCache = new Map();
function setText(el, text) {
  if (textCache.get(el) === text) return;
  textCache.set(el, text);
  el.textContent = text;
}

function updateHeader(s) {
  setText($('#scene'), s.scene_name || 'untitled');
  $('#paused').hidden = !s.paused;
  const t = s.transport || {};
  setText($('#pos'), `${(t.bar ?? 0) + 1}.${(t.beat ?? 0) + 1}`);
  setText($('#bpm'), `${(t.bpm ?? 0).toFixed(1)} bpm`);
  const h = s.harmony || {};
  setText($('#chord'), h.chord_name || '—');
  setText($('#key'), `${h.key_name || PC_NAMES[h.tonic ?? 0]} ${h.mode || ''}`);

  const degSig = `${h.tonic}|${(h.scale || []).join()}|${(h.chord || []).join()}`;
  const degEl = $('#degrees');
  if (degEl.dataset.sig !== degSig) {
    degEl.dataset.sig = degSig;
    const scale = new Set(h.scale || []);
    const chord = new Set(h.chord || []);
    degEl.innerHTML = '';
    for (let i = 0; i < 12; i++) {
      const pc = ((h.tonic ?? 0) + i) % 12;
      const d = document.createElement('i');
      d.title = PC_NAMES[pc];
      if (scale.has(pc)) d.classList.add('in-scale');
      if (chord.has(pc)) d.classList.add('in-chord');
      if (i === 0) d.classList.add('is-tonic');
      degEl.appendChild(d);
    }
  }
  const m = s.mood || {};
  $('#m-energy').style.width = `${clamp(m.energy ?? 0, 0, 1) * 100}%`;
  $('#m-tension').style.width = `${clamp(m.tension ?? 0, 0, 1) * 100}%`;
  $('#m-brightness').style.width = `${clamp(m.brightness ?? 0, 0, 1) * 100}%`;
  setText($('#cnote'), s.conductor_note || ' ');
  const f = s.form || {};
  const formEl = $('#form');
  const hasForm = !!(f.section || f.motif || f.note);
  formEl.hidden = !hasForm;
  if (hasForm) {
    setText($('#section'), f.section || '—');
    $('#arc').style.width = `${clamp(f.arc ?? 0, 0, 1) * 100}%`;
    const bars = f.bars_in_section ? `bar ${(f.bar_in_section ?? 0) + 1}/${f.bars_in_section}` : '';
    setText($('#bars'), bars);
    const motifEl = $('#motif');
    motifEl.hidden = !f.motif;
    setText(motifEl, f.motif ? `motif ${f.motif}` : '');
  }
  const fn = $('#fnote');
  fn.hidden = !f.note;
  setText(fn, f.note || '');
}

// ---------------------------------------------------------------------------
// Captions strip: DOM items with a CSS fade whose duration is the caption's life.
// ---------------------------------------------------------------------------
function pushCaptions(list) {
  if (!list.length) return;
  const root = $('#captions');
  state.captions.push(...list);
  for (const c of list) {
    const el = document.createElement('div');
    el.className = `cap${isBig(c) ? ' big' : ''}`;
    el.style.setProperty('--life', `${c.life}s`);
    if (c.layer !== null && c.layer !== undefined) el.style.borderColor = col(c.layer, 0.9);
    el.textContent = c.text;
    el.dataset.key = c.key;
    root.prepend(el);
    setTimeout(() => el.remove(), c.life * 1000 + 100);
  }
  // Keep the strip short: the newest few only.
  const live = new Set(liveCaptions(state.captions, Date.now()).map((c) => c.key));
  for (const el of [...root.children]) if (!live.has(el.dataset.key)) el.remove();
  state.captions = state.captions.filter((c) => live.has(c.key));
}

// ---------------------------------------------------------------------------
// Layers panel
// ---------------------------------------------------------------------------
const fmt = (v) => {
  if (typeof v !== 'number' || !isFinite(v)) return String(v ?? '');
  const a = Math.abs(v);
  if (a >= 1000) return v.toFixed(0);
  if (a >= 100) return v.toFixed(1);
  if (Number.isInteger(v)) return String(v);
  return v.toFixed(2);
};

function layerSignature(s) {
  return (s.layers || []).map((l) => `${l.id}:${l.name}:${Object.keys(l.signals || {}).join(',')}:${Object.keys(l.params || {}).join(',')}`).join('|');
}

function buildLayers(s) {
  const root = $('#layer-list');
  root.innerHTML = '';
  if (!(s.layers || []).length) { root.innerHTML = '<div class="empty">no layers</div>'; return; }
  for (const l of s.layers) {
    const i = l.id;
    const row = document.createElement('div');
    row.className = 'layer';
    row.dataset.id = i;
    row.style.color = col(i);
    const sigs = Object.keys(l.signals || {}).map((k) =>
      `<div class="sig" data-sig="${k}"><label title="${k}">${k}</label><canvas></canvas><span class="val mono">0</span></div>`).join('');
    const params = Object.keys(l.params || {}).map((k) =>
      `<div class="param" data-param="${k}"><label title="${k}">${k}</label><div class="bar"><i></i></div><span class="v mono">0</span></div>`).join('');
    row.innerHTML = `
      <div class="lhead">
        <span class="sw" style="background:${col(i)}"></span>
        <span class="lname">${l.name}</span>
        <span class="lrole">${l.role} · ${l.instrument || '—'}</span>
        <span class="lnotes mono" title="active notes">0</span>
        <button class="tog m" title="mute (${i + 1})">M</button>
        <button class="tog s" title="solo (⇧${i + 1})">S</button>
      </div>
      <div class="lmeter"><i class="rms"></i><i class="peak"></i></div>
      <div class="lsigs">${sigs}</div>
      <div class="lparams">${params}</div>`;
    row.querySelector('.tog.m').addEventListener('click', () => send({ cmd: 'toggle_mute', layer: i }));
    row.querySelector('.tog.s').addEventListener('click', () => send({ cmd: 'toggle_solo', layer: i }));
    root.appendChild(row);
  }
}

function drawSparkline(canvas, h, color) {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 100, ht = canvas.clientHeight || 18;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(ht * dpr)) {
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(ht * dpr);
  }
  const g = canvas.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, ht);
  if (h.n < 2) return;
  g.beginPath();
  const start = (h.i - h.n + HIST) % HIST;
  for (let k = 0; k < h.n; k++) {
    const v = clamp(h.buf[(start + k) % HIST], 0, 1);
    const x = w - ((h.n - 1 - k) / (HIST - 1)) * w;
    const y = ht - 1 - v * (ht - 2);
    if (k === 0) g.moveTo(x, y); else g.lineTo(x, y);
  }
  g.strokeStyle = color; g.lineWidth = 1.2; g.stroke();
  g.lineTo(w, ht); g.lineTo(w - ((h.n - 1) / (HIST - 1)) * w, ht); g.closePath();
  g.fillStyle = color.replace(/[\d.]+\)$/, '0.12)');
  g.fill();
}

function updateLayers(s) {
  const sig = layerSignature(s);
  if (sig !== state.layerSig) { state.layerSig = sig; buildLayers(s); }
  const rows = $('#layer-list').children;
  for (const l of s.layers || []) {
    const row = [...rows].find((r) => +r.dataset.id === l.id);
    if (!row) continue;
    row.classList.toggle('muted', !!l.muted);
    row.querySelector('.tog.m').classList.toggle('on', !!l.muted);
    row.querySelector('.tog.s').classList.toggle('on', !!l.soloed);
    setText(row.querySelector('.lnotes'), String(l.active_notes ?? 0));
    const m = l.meter || {};
    row.querySelector('.rms').style.width = `${clamp(Math.sqrt(m.rms || 0), 0, 1) * 100}%`;
    row.querySelector('.peak').style.left = `${clamp(Math.sqrt(m.peak || 0), 0, 1) * 100}%`;
    for (const el of row.querySelectorAll('.sig')) {
      const k = el.dataset.sig;
      const h = state.hist.get(l.id)?.get(k);
      if (h) drawSparkline(el.querySelector('canvas'), h, col(l.id, 0.9));
      setText(el.querySelector('.val'), fmt(l.signals?.[k] ?? 0));
    }
    for (const el of row.querySelectorAll('.param')) {
      const k = el.dataset.param;
      const v = l.params?.[k] ?? 0;
      el.querySelector('.bar i').style.width = `${clamp(v, 0, 1) * 100}%`;
      setText(el.querySelector('.v'), fmt(v));
    }
  }
}

// ---------------------------------------------------------------------------
// Sources panel
// ---------------------------------------------------------------------------
function updateSources(s) {
  const root = $('#source-list');
  const sig = (s.sources || []).map((x) => `${x.name}:${x.kind}`).join('|');
  if (sig !== state.sourceSig) {
    state.sourceSig = sig;
    root.innerHTML = '';
    if (!(s.sources || []).length) root.innerHTML = '<div class="empty">no sources</div>';
    for (const src of s.sources || []) {
      const el = document.createElement('div');
      el.className = 'source';
      el.dataset.name = src.name;
      el.innerHTML = `<span class="sdot"></span>
        <div class="shead"><span class="sname">${src.name}</span><span class="skind">${src.kind}</span><span class="srate mono"></span></div>
        <div class="smsg"></div><div class="sread mono"></div>`;
      root.appendChild(el);
    }
  }
  for (const src of s.sources || []) {
    const el = [...root.children].find((r) => r.dataset.name === src.name);
    if (!el) continue;
    const dot = el.querySelector('.sdot');
    dot.className = `sdot ${src.state || ''}`;
    dot.title = src.state || '';
    setText(el.querySelector('.srate'), `${fmt(src.rate ?? 0)}/s`);
    setText(el.querySelector('.smsg'), src.message || '');
    const readings = Object.entries(src.readings || {}).slice(0, 8)
      .map(([k, v]) => `<span><b>${k}</b> ${fmt(v)}</span>`).join('');
    const rd = el.querySelector('.sread');
    if (rd.dataset.v !== readings) { rd.dataset.v = readings; rd.innerHTML = readings; }
  }
}

// ---------------------------------------------------------------------------
// Event ticker
// ---------------------------------------------------------------------------
function hhmmss(ms) {
  const d = new Date(ms);
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map((n) => String(n).padStart(2, '0')).join(':');
}

function fmtAttr(v) {
  if (typeof v === 'number') return fmt(v);
  if (typeof v === 'boolean') return v ? 'yes' : 'no';
  const s = String(v);
  return s.length > 14 ? s.slice(0, 13) + '…' : s;
}

const esc = (v) => String(v).replace(/[&<>"]/g, (ch) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ch]));

// The event a pinned/hovered note points at, if it is in our buffer.
function eventForNote(n) {
  if (!n || !n.cause) return null;
  for (let i = state.events.length - 1; i >= 0; i--) if (causedBy(n, state.events[i])) return state.events[i];
  return null;
}

function updateTicker() {
  const list = $('#event-list');
  const recent = state.events.slice(-12).reverse();
  const layers = state.snap?.layers || [];
  const pinnedEv = eventForNote(state.pinned ? state.notes.get(state.pinned) : null);
  list.innerHTML = recent.map((e, i) => {
    const l = layers.find((x) => x.id === e.layer);
    const attrs = e.label ? e.label : Object.entries(e.attrs || {}).slice(0, 3).map(([k, v]) => `${k}=${fmtAttr(v)}`).join(' ');
    const c = col(e.layer);
    const idx = state.events.length - 1 - i;
    const cls = [state.hlEvent === e ? 'sel' : '', pinnedEv === e ? 'hl' : ''].filter(Boolean).join(' ');
    return `<li class="${cls}" data-idx="${idx}" title="click: highlight the notes this event caused"><span>${hhmmss(e.wall)}</span><span class="edot" style="background:${c}"></span>` +
      `<span class="ename">${esc(l ? l.name : 'L' + e.layer)} · ${esc(e.name)}</span>` +
      `<span class="ebar"><i style="width:${clamp(e.intensity ?? 0, 0, 1) * 100}%;background:${c}"></i></span>` +
      `<span class="eattrs${e.label ? ' label' : ''}" title="${esc(attrs)}">${esc(attrs)}</span></li>`;
  }).join('') || '<li class="empty">waiting for events…</li>';
}

$('#event-list').addEventListener('click', (ev) => {
  const li = ev.target.closest('li[data-idx]');
  if (!li) return;
  const e = state.events[+li.dataset.idx];
  state.hlEvent = state.hlEvent === e ? null : e || null;
  state.tickerDirty = true;
  updateTicker(); state.tickerDirty = false;
});

// ---------------------------------------------------------------------------
// Stage: canvases
// ---------------------------------------------------------------------------
const vizCanvas = $('#viz');
const ovCanvas = $('#overlay');
const vg = vizCanvas.getContext('2d');
const og = ovCanvas.getContext('2d');
let W = 0, H = 0;

function resize() {
  const stage = $('#stage');
  const dpr = window.devicePixelRatio || 1;
  const w = stage.clientWidth, h = stage.clientHeight;
  if (w === W && h === H && vizCanvas.width === Math.round(w * dpr)) return;
  W = w; H = h;
  $('#captions').style.bottom = `${clamp(H * 0.26, 80, 220) + 10}px`;
  for (const c of [vizCanvas, ovCanvas]) { c.width = Math.round(w * dpr); c.height = Math.round(h * dpr); }
  vg.setTransform(dpr, 0, 0, dpr, 0, 0);
  og.setTransform(dpr, 0, 0, dpr, 0, 0);
}

function pickViz(s) {
  const kinds = ['freeway', 'camera', 'map', 'generic'];
  if (kinds.includes(s.viz_kind)) {
    // Trust the hint, except fall back when the hinted data is missing.
    if (s.viz_kind === 'freeway' && !findViz(s, 'cars')) return findViz(s, 'blobs') ? 'camera' : 'generic';
    if (s.viz_kind === 'camera' && !findViz(s, 'blobs')) return findViz(s, 'cars') ? 'freeway' : 'generic';
    return s.viz_kind;
  }
  if (findViz(s, 'cars')) return 'freeway';
  if (findViz(s, 'blobs')) return 'camera';
  if (state.geoSeen || findViz(s, 'strikes') || (s.events || []).some((e) => eventLatLon(e.attrs))) return 'map';
  return 'generic';
}

function findViz(s, key) {
  for (const [name, v] of Object.entries(s.viz || {})) if (v && Array.isArray(v[key])) return { name, v };
  return null;
}

// --- freeway -------------------------------------------------------------
function drawFreeway(s, Hv) {
  const f = findViz(s, 'cars');
  // Lane guides (faint, dashed) on the overlay.
  og.save();
  og.setLineDash([14, 22]);
  og.lineWidth = 1;
  og.strokeStyle = 'rgba(255,255,255,0.045)';
  for (const y of state.laneY.values()) {
    og.beginPath(); og.moveTo(0, y * Hv); og.lineTo(W, y * Hv); og.stroke();
  }
  og.restore();
  if (!f) return;
  // Headlights on the persistent canvas: trails come from the per-frame fade.
  vg.save();
  vg.globalCompositeOperation = 'lighter';
  for (const c of f.v.cars) {
    const x = (c.x ?? 0.5) * W, y = (c.y ?? 0.5) * Hv;
    const r = (6 + clamp(c.size ?? 0.5, 0, 2) * 22) * Math.min(1.5, W / 900);
    const a = 0.35 + 0.65 * clamp(c.bright ?? 0.8, 0, 1);
    const grad = vg.createRadialGradient(x, y, 0, x, y, r);
    grad.addColorStop(0, `rgba(255,246,222,${a})`);
    grad.addColorStop(0.25, `rgba(255,214,150,${a * 0.6})`);
    grad.addColorStop(1, 'rgba(255,170,90,0)');
    vg.fillStyle = grad;
    vg.beginPath(); vg.arc(x, y, r, 0, Math.PI * 2); vg.fill();
    // Hot core.
    vg.fillStyle = `rgba(255,255,255,${a})`;
    vg.beginPath(); vg.arc(x, y, Math.max(1.2, r * 0.14), 0, Math.PI * 2); vg.fill();
  }
  vg.restore();
}

// --- camera --------------------------------------------------------------
function drawCamera(s, Hv) {
  const f = findViz(s, 'blobs');
  if (!f) return;
  const line = typeof f.v.line === 'number' ? f.v.line : null;
  if (line !== null) {
    og.save(); og.setLineDash([6, 8]); og.strokeStyle = 'rgba(232,200,138,0.5)'; og.lineWidth = 1;
    og.beginPath(); og.moveTo(0, line * Hv); og.lineTo(W, line * Hv); og.stroke(); og.restore();
  }
  vg.save();
  vg.globalCompositeOperation = 'lighter';
  for (const b of f.v.blobs) {
    const x = (b.x ?? 0.5) * W, y = (b.y ?? 0.5) * Hv;
    const w = Math.max(6, (b.w ?? 0.03) * W), h = Math.max(6, (b.h ?? 0.03) * Hv);
    const a = 0.3 + 0.7 * clamp(b.b ?? b.brightness ?? 0.7, 0, 1);
    const grad = vg.createRadialGradient(x, y, 0, x, y, Math.max(w, h));
    grad.addColorStop(0, `rgba(210,230,255,${a * 0.6})`);
    grad.addColorStop(1, 'rgba(120,160,255,0)');
    vg.fillStyle = grad;
    vg.fillRect(x - w, y - h, w * 2, h * 2);
  }
  vg.restore();
  og.save();
  og.font = '10px ' + getComputedStyle(document.body).getPropertyValue('--mono');
  for (const b of f.v.blobs) {
    const x = (b.x ?? 0.5) * W, y = (b.y ?? 0.5) * Hv;
    const w = Math.max(6, (b.w ?? 0.03) * W), h = Math.max(6, (b.h ?? 0.03) * Hv);
    og.strokeStyle = 'rgba(210,230,255,0.55)'; og.lineWidth = 1;
    og.strokeRect(x - w / 2, y - h / 2, w, h);
    if (b.id !== undefined) { og.fillStyle = 'rgba(210,230,255,0.7)'; og.fillText(String(b.id), x - w / 2, y - h / 2 - 3); }
  }
  og.restore();
}

// --- map: Live Earth ------------------------------------------------------
// Equirectangular world: embedded low-res coastlines (Natural Earth 110m, simplified),
// graticule, the night side from the current UTC time, every geo-tagged event as an
// expanding ring in its layer colour, lightning `strikes` as flashes.
let coast = null;        // {lines: [[lon,lat,lon,lat,…], …]} once fetched
let coastCache = null;   // offscreen canvas of coast + graticule at the current size
let coastFetching = false;
function loadCoast() {
  if (coast || coastFetching) return;
  coastFetching = true;
  fetch(new URL('./coastlines.json', import.meta.url)).then((r) => (r.ok ? r.json() : null))
    .then((j) => { coast = j && Array.isArray(j.lines) ? j : { lines: [] }; coastCache = null; })
    .catch(() => { coast = { lines: [] }; });
}

function coastLayer(Hv) {
  const dpr = window.devicePixelRatio || 1;
  if (coastCache && coastCache.W === W && coastCache.Hv === Hv && coastCache.dpr === dpr) return coastCache.c;
  const c = document.createElement('canvas');
  c.width = Math.round(W * dpr); c.height = Math.round(Hv * dpr);
  const g = c.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  // Graticule every 30°, equator & prime meridian brighter.
  g.lineWidth = 1;
  for (let lon = -180; lon <= 180; lon += 30) {
    const [x] = project(lon, 0, W, Hv);
    g.strokeStyle = lon === 0 ? 'rgba(255,255,255,0.11)' : 'rgba(255,255,255,0.045)';
    g.beginPath(); g.moveTo(x, 0); g.lineTo(x, Hv); g.stroke();
  }
  for (let lat = -60; lat <= 60; lat += 30) {
    const [, y] = project(0, lat, W, Hv);
    g.strokeStyle = lat === 0 ? 'rgba(255,255,255,0.11)' : 'rgba(255,255,255,0.045)';
    g.beginPath(); g.moveTo(0, y); g.lineTo(W, y); g.stroke();
  }
  // Coastlines.
  g.strokeStyle = 'rgba(150,180,210,0.42)'; g.lineWidth = Math.max(0.8, Math.min(1.4, W / 1100)); g.lineJoin = 'round';
  g.beginPath();
  for (const line of (coast && coast.lines) || []) {
    for (let i = 0; i + 1 < line.length; i += 2) {
      const [x, y] = project(line[i], line[i + 1], W, Hv);
      if (i === 0) g.moveTo(x, y); else g.lineTo(x, y);
    }
  }
  g.stroke();
  coastCache = { c, W, Hv, dpr };
  return c;
}

let lastSun = { at: 0, sun: null };
function drawMap(s, Hv, now) {
  loadCoast();
  og.drawImage(coastLayer(Hv), 0, 0, W, Hv);

  // Night side, recomputed once a minute.
  if (!lastSun.sun || now - lastSun.at > 60000) lastSun = { at: now, sun: sunPosition(new Date()) };
  const poly = nightPolygon(lastSun.sun, 120);
  og.save();
  og.beginPath();
  poly.forEach(([lon, lat], i) => { const [x, y] = project(lon, lat, W, Hv); if (i === 0) og.moveTo(x, y); else og.lineTo(x, y); });
  og.closePath();
  og.fillStyle = 'rgba(2,4,14,0.42)';
  og.fill();
  og.strokeStyle = 'rgba(232,200,138,0.18)'; og.lineWidth = 1;
  og.beginPath();
  for (let i = 0; i < poly.length - 2; i++) { const [x, y] = project(poly[i][0], poly[i][1], W, Hv); if (i === 0) og.moveTo(x, y); else og.lineTo(x, y); }
  og.stroke();
  // Subsolar point.
  const [sx, sy] = project(lastSun.sun.lon, lastSun.sun.decl, W, Hv);
  og.fillStyle = 'rgba(232,200,138,0.5)';
  og.beginPath(); og.arc(sx, sy, 2.5, 0, Math.PI * 2); og.fill();
  og.restore();

  // Lightning: viz.*.strikes [{lat01, lon01, age_s}] as fading flashes on the glow canvas.
  for (const v of Object.values(s.viz || {})) {
    if (!v || !Array.isArray(v.strikes)) continue;
    vg.save(); vg.globalCompositeOperation = 'lighter';
    for (const st of v.strikes) {
      const ll = eventLatLon(st);
      if (!ll) continue;
      const a = clamp(1 - (st.age_s ?? 0) / 5, 0, 1);
      if (a <= 0) continue;
      const [x, y] = project(ll.lon, ll.lat, W, Hv);
      const r = 2 + 5 * a;
      const grad = vg.createRadialGradient(x, y, 0, x, y, r);
      grad.addColorStop(0, `rgba(255,255,255,${0.1 * a})`);
      grad.addColorStop(0.3, `rgba(220,230,255,${0.08 * a})`);
      grad.addColorStop(1, 'rgba(180,200,255,0)');
      vg.fillStyle = grad;
      vg.beginPath(); vg.arc(x, y, r, 0, Math.PI * 2); vg.fill();
    }
    vg.restore();
  }

  const LIFE = 10000;
  state.pings = state.pings.filter((p) => now - p.t0 < LIFE);
  og.save();
  for (const p of state.pings) {
    const age = (now - p.t0) / LIFE;
    const x = p.x * W, y = p.y * Hv;
    const rMax = 10 + p.intensity * 70;
    const r = 3 + Math.pow(age, 0.5) * rMax;
    const hot = state.hover && state.hover.kind === 'ping' && state.hover.ping === p;
    og.strokeStyle = col(p.layer, (1 - age) * (hot ? 1 : 0.8)); og.lineWidth = hot ? 2.5 : 1.5;
    og.beginPath(); og.arc(x, y, r, 0, Math.PI * 2); og.stroke();
    og.fillStyle = col(p.layer, hot ? 1 : (1 - age) * 0.9, 75);
    og.beginPath(); og.arc(x, y, hot ? 4 : 2.5, 0, Math.PI * 2); og.fill();
  }
  og.restore();
}

/** Nearest live ping within `r` px of a stage point, for hover labels. */
function pingAt(x, y, Hv, r = 14) {
  let best = null, bd = r * r;
  for (const p of state.pings) {
    const dx = p.x * W - x, dy = p.y * Hv - y, d = dx * dx + dy * dy;
    if (d < bd) { bd = d; best = p; }
  }
  return best;
}

// --- generic signal field -------------------------------------------------
function drawGeneric(s, Hv) {
  const layers = s.layers || [];
  if (!layers.length) return;
  const band = Hv / layers.length;
  og.save();
  og.font = '10px ' + getComputedStyle(document.body).getPropertyValue('--sans');
  layers.forEach((l, li) => {
    const top = li * band;
    og.fillStyle = 'rgba(255,255,255,0.04)';
    og.fillRect(0, top + band - 1, W, 1);
    og.fillStyle = col(l.id, 0.7);
    og.fillText(l.name, 10, top + 14);
    const sigs = [...(state.hist.get(l.id) || new Map()).entries()];
    sigs.forEach(([name, h], si) => {
      if (h.n < 2) return;
      const start = (h.i - h.n + HIST) % HIST;
      og.beginPath();
      for (let k = 0; k < h.n; k++) {
        const v = clamp(h.buf[(start + k) % HIST], 0, 1);
        const x = W - ((h.n - 1 - k) / (HIST - 1)) * W;
        const y = top + band - 6 - v * (band - 22);
        if (k === 0) og.moveTo(x, y); else og.lineTo(x, y);
      }
      og.strokeStyle = col(l.id, 0.85 - si * 0.2, 62 + si * 8);
      og.lineWidth = si === 0 ? 1.6 : 1;
      og.stroke();
      og.fillStyle = col(l.id, 0.6);
      og.fillText(name, 10 + 60 * (si + 1) + og.measureText(l.name).width - 50, top + 14);
    });
  });
  og.restore();
}

// --- shared: rings (event flashes + note ripples) ------------------------
function drawRings(Hv, now) {
  const LIFE = 1100;
  state.rings = state.rings.filter((r) => now - r.t0 < LIFE);
  og.save();
  for (const r of state.rings) {
    const age = (now - r.t0) / LIFE;
    const x = r.x * W, y = r.y * Hv;
    const rad = 4 + age * (30 + 50 * r.intensity);
    og.lineWidth = 2 * (1 - age);
    og.strokeStyle = col(r.layer, (1 - age) * 0.9);
    og.beginPath(); og.arc(x, y, rad, 0, Math.PI * 2); og.stroke();
    if (age > 0.15) { // trailing note ripple
      const a2 = (age - 0.15) / 0.85;
      og.lineWidth = 1;
      og.strokeStyle = col(r.layer, (1 - a2) * 0.5, 80);
      og.beginPath(); og.arc(x, y, 2 + a2 * (20 + 30 * r.intensity), 0, Math.PI * 2); og.stroke();
    }
  }
  og.restore();
}

// --- piano roll + spectrum ------------------------------------------------
const ROLL_WINDOW = 12; // seconds visible
const MIDI_LO = 36, MIDI_HI = 96;

function drawRollAndSpectrum(s, top, rh, now) {
  // Backdrop
  og.save();
  const bg = og.createLinearGradient(0, top, 0, top + rh);
  bg.addColorStop(0, 'rgba(5,6,8,0)'); bg.addColorStop(0.3, 'rgba(5,6,8,0.75)'); bg.addColorStop(1, 'rgba(5,6,8,0.9)');
  og.fillStyle = bg; og.fillRect(0, top, W, rh);

  // Spectrum, ghosted behind.
  const spec = s.spectrum || [];
  if (spec.length) {
    const bw = W / spec.length;
    for (let i = 0; i < spec.length; i++) {
      const v = clamp(spec[i], 0, 1);
      const h = v * rh * 0.92;
      og.fillStyle = `hsla(${200 + (i / spec.length) * 40},40%,70%,${0.05 + v * 0.1})`;
      og.fillRect(i * bw + 1, top + rh - h, bw - 2, h);
    }
  }

  // Octave guides.
  og.strokeStyle = 'rgba(255,255,255,0.05)'; og.lineWidth = 1;
  for (let m = MIDI_LO; m <= MIDI_HI; m += 12) {
    const y = top + (1 - (m - MIDI_LO) / (MIDI_HI - MIDI_LO)) * rh;
    og.beginPath(); og.moveTo(0, y); og.lineTo(W, y); og.stroke();
  }

  // Notes scroll right → left; newest at the right edge. Every rect drawn is remembered
  // so the pointer can be hit-tested against the list instead of redrawing.
  const pxPerSec = W / ROLL_WINDOW;
  const noteH = Math.max(2.5, rh / (MIDI_HI - MIDI_LO) * 1.6);
  const geom = { now, W, top, rh, pxPerSec, noteH, midiLo: MIDI_LO, midiHi: MIDI_HI };
  const rects = state.noteRects;
  rects.length = 0;
  const hl = state.hlEvent;
  const hoverKey = state.hover && state.hover.kind === 'note' ? state.hover.key : null;
  for (const [k, n] of state.notes) {
    const r = noteRect(n, geom);
    if (!r) { if (!n.active && now - n.at > ROLL_WINDOW) state.notes.delete(k); continue; }
    const vel = clamp(n.velocity ?? 0.7, 0, 1);
    og.fillStyle = col(n.layer, 0.45 + 0.55 * vel, 45 + vel * 30);
    if (n.active) { og.shadowBlur = 14; og.shadowColor = col(n.layer, 0.9, 70); } else { og.shadowBlur = 0; }
    og.fillRect(r.x, r.y, r.w, r.h);
    r.key = k; r.note = n;
    rects.push(r);
    if ((hl && causedBy(n, hl)) || k === state.pinned || k === hoverKey) {
      og.shadowBlur = 0;
      og.strokeStyle = k === hoverKey || k === state.pinned ? 'rgba(255,255,255,0.95)' : 'rgba(232,200,138,0.95)';
      og.lineWidth = 1.5;
      og.strokeRect(r.x - 1, r.y - 1, r.w + 2, r.h + 2);
    }
  }
  og.shadowBlur = 0;
  // Playhead.
  og.fillStyle = 'rgba(232,200,138,0.35)';
  og.fillRect(W - 1, top, 1, rh);
  og.restore();
}

// --- frame loop -----------------------------------------------------------
let vizKindShown = '';
function frame() {
  resize();
  const now = performance.now();
  // Persistence-of-vision fade on the headlight canvas.
  vg.save();
  vg.globalCompositeOperation = 'destination-out';
  vg.fillStyle = 'rgba(0,0,0,0.16)';
  vg.fillRect(0, 0, W, H);
  vg.restore();
  og.clearRect(0, 0, W, H);

  const s = state.snap;
  if (s) {
    const rh = clamp(H * 0.26, 80, 220);
    const Hv = H - rh;
    const kind = pickViz(s);
    if (kind !== vizKindShown) { vizKindShown = kind; setText($('#vizlabel'), kind); }
    if (kind === 'freeway') drawFreeway(s, Hv);
    else if (kind === 'camera') drawCamera(s, Hv);
    else if (kind === 'map') drawMap(s, Hv, now);
    else drawGeneric(s, Hv);
    drawRings(Hv, now);
    drawRollAndSpectrum(s, Hv, rh, engineNow());
    updateHover(Hv);
  }
  requestAnimationFrame(frame);
}

// ---------------------------------------------------------------------------
// Attribution: hover a note → what made it; click pins; map pings show their caption.
// ---------------------------------------------------------------------------
const tip = $('#tip');
let tipSig = '';
function showTip(html, x, y, pinned) {
  if (tipSig !== html) { tipSig = html; tip.innerHTML = html; }
  tip.hidden = false;
  tip.classList.toggle('pinned', !!pinned);
  const tw = tip.offsetWidth, th = tip.offsetHeight;
  tip.style.left = `${clamp(x + 14, 4, Math.max(4, W - tw - 4))}px`;
  tip.style.top = `${clamp(y - th - 10, 4, Math.max(4, H - th - 4))}px`;
}
function hideTip() { if (!tip.hidden) { tip.hidden = true; tipSig = ''; } }

function noteTipHtml(n) {
  const layers = state.snap?.layers || [];
  const l = layers.find((x) => x.id === n.layer);
  const rows = [
    `<b style="color:${col(n.layer, 1, 70)}">${esc(l ? l.name : 'L' + n.layer)}</b> <span class="mono">${esc(pitchName(n.midi))}</span> <span class="dim">vel ${(n.velocity ?? 0).toFixed(2)}</span>`,
  ];
  if (n.reason) rows.push(`<span class="dim">${esc(n.reason)}</span>`);
  if (n.cause) {
    const txt = causeText(n.cause) || n.cause.event;
    rows.push(`<span class="cause">↳ ${esc(txt)}</span>`);
    if (!n.cause.label && !attrSummary(n.cause.attrs)) rows.push(`<span class="dim">event ${esc(n.cause.event)}</span>`);
  } else if (!n.reason) rows.push('<span class="dim">conductor</span>');
  return rows.join('<br>');
}

function updateHover(Hv) {
  const pt = state.pointer;
  let hover = null;
  if (pt.inside) {
    if (pt.y >= Hv) {
      const r = hitNote(state.noteRects, pt.x, pt.y);
      if (r) hover = { kind: 'note', key: r.key, note: r.note, rect: r };
    } else if (vizKindShown === 'map') {
      const p = pingAt(pt.x, pt.y, Hv);
      if (p) hover = { kind: 'ping', ping: p };
    }
  }
  state.hover = hover;
  $('#stage').classList.toggle('hit', !!hover);

  if (state.pinned) {
    const r = state.noteRects.find((x) => x.key === state.pinned);
    if (!r) { state.pinned = null; state.tickerDirty = true; }
    else { showTip(noteTipHtml(r.note), r.x + r.w / 2, r.y, true); return; }
  }
  if (!hover) { hideTip(); return; }
  if (hover.kind === 'note') showTip(noteTipHtml(hover.note), pt.x, pt.y, false);
  else {
    const e = hover.ping.event;
    const layers = state.snap?.layers || [];
    const l = layers.find((x) => x.id === e.layer);
    showTip(`<b style="color:${col(e.layer, 1, 70)}">${esc(l ? l.name : 'L' + e.layer)}</b> · ${esc(e.name)}<br><span class="cause">${esc(causeText(e) || e.name)}</span>`, pt.x, pt.y, false);
  }
}

{
  const stage = $('#stage');
  const pos = (ev) => { const b = stage.getBoundingClientRect(); return { x: ev.clientX - b.left, y: ev.clientY - b.top }; };
  stage.addEventListener('pointermove', (ev) => { const p = pos(ev); state.pointer.x = p.x; state.pointer.y = p.y; state.pointer.inside = true; });
  stage.addEventListener('pointerleave', () => { state.pointer.inside = false; });
  stage.addEventListener('click', (ev) => {
    if (ev.target.closest('#captions, #tip, button, .campreview')) return;
    const p = pos(ev);
    const r = hitNote(state.noteRects, p.x, p.y);
    state.pinned = r && r.key !== state.pinned ? r.key : null;
    state.tickerDirty = true;
    const li = state.pinned ? eventForNote(r.note) : null;
    if (li) { updateTicker(); state.tickerDirty = false; $('#event-list li.hl')?.scrollIntoView({ block: 'nearest' }); }
  });
}

// ---------------------------------------------------------------------------
// Transport (ws ↔ worker), scene picker, main-thread sources
// ---------------------------------------------------------------------------
const params = new URLSearchParams(location.search);
const MOCK = params.get('mock') === '1';
let transport = null;
let mainAdapters = [];     // browser.input / camera adapters running on this thread
let cameraAdapter = null;
let audioAdapter = null;   // audio.features (mic / stream) adapter, toggled by #mic
let currentScene = null;   // {name, title, toml, ...} in worker mode
let currentSeed = params.get('seed');

function setConn(stateName) {
  $('#dot').className = `dot ${stateName}`;
  setText($('#conn-label'), stateName);
}

function banner(text, bad = false) {
  const el = $('#banner');
  el.hidden = !text;
  el.className = `banner${bad ? ' bad' : ''}`;
  setText(el, text || '');
}

function send(cmd) { if (transport) transport.send(cmd); }

// Forget everything learned from the previous engine (scene switch / restart).
function resetClient() {
  state.snap = null; state.hist = new Map(); state.events = []; state.seenEvents = new Map(); state.notes = new Map();
  state.rings = []; state.pings = []; state.laneY = new Map(); state.layerSig = ''; state.sourceSig = ''; state.tickerDirty = true;
  state.captions = []; state.seenCaptions = new Set(); state.geoSeen = false; state.noteRects = [];
  state.hover = null; state.pinned = null; state.hlEvent = null;
  textCache.clear();
  hideTip();
  vg.clearRect(0, 0, W, H); og.clearRect(0, 0, W, H);
  $('#layer-list').innerHTML = ''; $('#source-list').innerHTML = ''; $('#event-list').innerHTML = ''; $('#captions').innerHTML = '';
}

function stopMainAdapters() {
  for (const a of mainAdapters) { try { a.stop(); } catch (_) { /* ignore */ } }
  mainAdapters = [];
  cameraAdapter = null;
  $('#camera').hidden = true;
  $('#camera').classList.remove('on');
  audioAdapter = null;
  $('#mic').hidden = true;
  $('#mic').classList.remove('on');
}

// Sources that need the DOM or user media run here and forward samples to the worker.
async function startMainAdapters(specs) {
  stopMainAdapters();
  const { loadAdapter, isCameraKind, isAudioKind } = await loadRegistry();
  for (const spec of specs) {
    const emit = (json) => transport.postSample(spec.name, json);
    const log = (m) => console.debug(`[${spec.name}]`, m);
    try {
      const Adapter = await loadAdapter(spec.kind);
      if (!Adapter) { emit(JSON.stringify({ type: 'status', state: 'failed', message: `no browser adapter for ${spec.kind}` })); continue; }
      const audio = isAudioKind(spec.kind);
      const ctx = { stage: $('#stage'), postFrame: (gray, w, h) => transport.postFrame(spec.name, gray, w, h), onChange: (on) => $(audio ? '#mic' : '#camera').classList.toggle('on', on) };
      const a = new Adapter(spec, emit, log, ctx);
      mainAdapters.push(a);
      if (audio) { audioAdapter = a; $('#mic').textContent = a.label; $('#mic').hidden = false; }
      a.start();
      if (isCameraKind(spec.kind)) { cameraAdapter = a; $('#camera').hidden = false; }
    } catch (e) {
      emit(JSON.stringify({ type: 'status', state: 'failed', message: e && e.message ? e.message : String(e) }));
    }
  }
}

$('#camera').addEventListener('click', () => {
  if (!cameraAdapter) return;
  if (cameraAdapter.enabled) cameraAdapter.disable(); else cameraAdapter.enable();
});
$('#mic').addEventListener('click', () => {
  if (!audioAdapter) return;
  if (audioAdapter.enabled) audioAdapter.disable(); else audioAdapter.enable();
});

// The product is about listening to the world: default to a live scene that works from
// any browser, and fall back to the founding (simulated) one.
const DEFAULT_SCENES = ['social-firehose', 'night-freeway'];

function pickScene(scenes, wanted) {
  return scenes.find((s) => s.name === wanted)
    || DEFAULT_SCENES.map((n) => scenes.find((s) => s.name === n)).find(Boolean)
    || scenes[0];
}

function buildScenePicker(scenes) {
  const sel = $('#scene-pick');
  sel.innerHTML = '';
  // Live feeds first; simulated/offline scenes in their own group.
  const groups = [
    ['live — real data streams', scenes.filter((s) => s.tags?.includes('live'))],
    ['offline — simulated or local', scenes.filter((s) => !s.tags?.includes('live'))],
  ];
  for (const [label, list] of groups) {
    if (!list.length) continue;
    const g = document.createElement('optgroup');
    g.label = label;
    for (const s of list) {
      const o = document.createElement('option');
      o.value = s.name;
      o.textContent = s.title || s.name;
      o.title = `${s.description || ''}${s.tags?.length ? `  [${s.tags.join(', ')}]` : ''}`;
      g.appendChild(o);
    }
    sel.appendChild(g);
  }
  sel.hidden = false;
  $('#scene').hidden = true;
  sel.onchange = () => {
    if (sel.value === '__custom') return;
    const s = transport.scenes.find((x) => x.name === sel.value);
    if (s) { sel.querySelector('option[value="__custom"]')?.remove(); startScene(s, params.get('seed')); }
  };
}

// For scenes tagged `camera` whose traffic is simulated, offer a webcam variant: the
// same scene with `synthetic.freeway` swapped for a camera source (fed by push_frame).
async function addWebcamVariants(scenes) {
  if (!navigator.mediaDevices?.getUserMedia) return scenes;
  const out = [...scenes];
  for (const s of scenes) {
    if (!s.tags?.includes('camera') || !/kind\s*=\s*"synthetic\.freeway"/.test(s.toml)) continue;
    const toml = s.toml.replace(/kind\s*=\s*"synthetic\.freeway"[^}\n]*/, 'kind = "camera.mjpeg", url = "webcam"').replace(/^name\s*=\s*"([^"]*)"/m, 'name = "$1 (webcam)"');
    const errors = await transport.validate(toml);
    if (errors.length) { console.debug('webcam variant rejected', errors); continue; }
    out.splice(out.indexOf(s) + 1, 0, { ...s, name: `${s.name}-webcam`, title: `${s.title || s.name} (webcam)`, description: `${s.description || ''} Uses your webcam.`, toml });
  }
  return out;
}

function startScene(scene, seed) {
  currentScene = scene;
  currentSeed = seed ?? String(Math.floor(Math.random() * 2 ** 48));
  resetClient();
  stopMainAdapters();
  setConn('connecting');
  banner('');
  transport.start(scene.toml, currentSeed, audio.srcRate);
  // Drop whatever the old engine had queued in the worklet so latency doesn't stack.
  if (audio.node) audio.node.port.postMessage({ type: 'config', srcRate: audio.srcRate, targetMs: 200, maxMs: 500 });
  const pick = $('#scene-pick');
  if (scene.custom) {
    let o = pick.querySelector('option[value="__custom"]');
    if (!o) { o = document.createElement('option'); o.value = '__custom'; pick.prepend(o); }
    o.textContent = `✎ ${scene.title || sceneTitle(scene.toml)}`;
  }
  pick.value = scene.custom ? '__custom' : scene.name;
  const u = new URL(location.href);
  if (scene.custom) u.searchParams.delete('scene'); else u.searchParams.set('scene', scene.name);
  if (!scene.keepHash) u.hash = ''; // a stale #s= would describe a different scene than the one playing
  history.replaceState(null, '', u);
  if (editor) editor.setToml(scene.toml, scene.custom ? '' : scene.name);
}

// --- scene editor, share links, URL scenes ---------------------------------
let editor = null;
const customScene = (toml, extra = {}) => ({ name: '__custom', title: sceneTitle(toml), toml, custom: true, tags: [], ...extra });

async function validateToml(toml) {
  if (transport && transport.mode === 'ws') {
    const r = await fetch('api/validate', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ toml }) });
    if (!r.ok) throw new Error(`server said ${r.status}`);
    return (await r.json()).errors || [];
  }
  if (transport && transport.validate) return transport.validate(toml);
  throw new Error('no engine to validate with yet');
}

async function applyToml(toml) {
  if (!transport) throw new Error('not connected');
  if (transport.mode === 'ws') { send({ cmd: 'load_scene', toml }); return; }
  startScene(customScene(toml), params.get('seed'));
}

function currentToml() {
  if (editor && editor.visible && editor.getToml().trim()) return editor.getToml();
  return currentScene ? currentScene.toml : (editor ? editor.getToml() : '');
}

async function shareToml(toml) {
  const url = await sceneLink(location.href, toml);
  history.replaceState(null, '', url);
  try { await navigator.clipboard.writeText(url); } catch (_) { /* no clipboard permission: the link is shown anyway */ }
  return url;
}

async function shareCurrent() {
  const toml = currentToml();
  if (!toml.trim()) { banner('nothing to share yet'); return; }
  try {
    const url = await shareToml(toml);
    editor.show();
    editor.showLink(url);
    editor.flash('link copied to the clipboard');
  } catch (e) { banner(`share failed: ${e.message || e}`, true); }
}

function setupEditor() {
  editor = createEditor($('#editor'), {
    validate: validateToml,
    apply: applyToml,
    share: shareToml,
    builtins: () => (transport && transport.scenes) || [],
  });
  $('#edit').addEventListener('click', () => editor.toggle());
  $('#share').addEventListener('click', shareCurrent);
}

/** The scene carried in `#s=` (decoded TOML) or null. */
async function urlScene() {
  const payload = scenePayloadFromHash(location.hash);
  if (!payload) return null;
  try { return await decodeScene(payload); } catch (e) { banner(`could not read the scene in this link: ${e.message || e}`, true); return null; }
}

async function boot() {
  transport = await connect({ mode: params.get('mode') || undefined, engine: MOCK ? './engine-mock.js' : './pkg/sonder_wasm.js' });
  transport.on('status', setConn);
  transport.on('snapshot', ingest);
  transport.on('audio', onAudio);
  transport.on('audio-config', ({ sampleRate }) => {
    audio.srcRate = sampleRate;
    if (audio.node) audio.node.port.postMessage({ type: 'config', srcRate: sampleRate, targetMs: 200, maxMs: 500 });
  });
  transport.on('audio-error', (msg) => { setText($('#buf'), msg); $('#buf').className = 'buf mono bad'; });

  setupEditor();
  const linked = await urlScene();

  if (transport.mode === 'ws') {
    if (transport.probe && !state.snap) ingest(transport.probe);
    if (linked) {
      // A link with a scene in it asks the native engine to hot-reload into that scene.
      editor.setToml(linked, '');
      const errs = await validateToml(linked).catch(() => []);
      if (errs.length) { editor.show(); banner('the linked scene has errors; see the editor', true); }
      else {
        let sent = false;
        transport.on('status', (st) => { if (st === 'live' && !sent) { sent = true; send({ cmd: 'load_scene', toml: linked }); } });
      }
    }
    return;
  }

  $('#mode').hidden = false;
  $('#mode').textContent = MOCK ? 'no server — mock engine' : 'no server — running in your browser';
  transport.on('error', ({ message }) => { banner(`engine error: ${message}`, true); setConn('closed'); });
  transport.on('ready', async ({ scenes, version }) => {
    console.info(`sonder wasm ${version}: ${scenes.length} scenes`);
    transport.scenes = await addWebcamVariants(scenes);
    buildScenePicker(transport.scenes);
    if (linked) {
      const errs = await transport.validate(linked);
      if (errs.length) {
        editor.setToml(linked, ''); editor.show();
        banner('the linked scene has errors; see the editor', true);
        startScene(pickScene(transport.scenes, params.get('scene')), params.get('seed'));
      } else startScene(customScene(linked, { keepHash: true }), params.get('seed'));
    } else startScene(pickScene(transport.scenes, params.get('scene')), params.get('seed'));
  });
  transport.on('started', async ({ seed, mainSpecs, specs }) => {
    const { ADAPTERS } = await loadRegistry();
    const seedEl = $('#seed');
    seedEl.hidden = false;
    setText(seedEl, `seed ${seed}`);
    const u = new URL(location.href); u.searchParams.set('seed', seed); history.replaceState(null, '', u);
    startMainAdapters(mainSpecs || []);
    const missing = (specs || []).filter((s) => !s.builtin && !ADAPTERS[s.kind]).map((s) => `${s.name} (${s.kind})`);
    if (missing.length) banner(`not available in the browser: ${missing.join(', ')} — those layers play on the conductor alone`);
  });
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------
window.addEventListener('keydown', (e) => {
  const tag = (e.target && e.target.tagName) || '';
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || e.metaKey || e.ctrlKey || e.altKey) return;
  const digit = /^Digit([1-9])$/.exec(e.code);
  if (digit) {
    send({ cmd: e.shiftKey ? 'toggle_solo' : 'toggle_mute', layer: +digit[1] - 1 });
    e.preventDefault();
    return;
  }
  switch (e.key) {
    case ' ': send({ cmd: 'toggle_pause' }); e.preventDefault(); break;
    case '[': send({ cmd: 'nudge_tempo', delta: -2 }); break;
    case ']': send({ cmd: 'nudge_tempo', delta: 2 }); break;
    case '-': send({ cmd: 'nudge_density', delta: -0.1 }); break;
    case '=': send({ cmd: 'nudge_density', delta: 0.1 }); break;
    case 'k': send({ cmd: 'transpose', semitones: 1 }); break;
    case 'K': send({ cmd: 'transpose', semitones: -1 }); break;
    case 'm': send({ cmd: 'next_mode', backwards: false }); break;
    case 'M': send({ cmd: 'next_mode', backwards: true }); break;
    case 'c': send({ cmd: 'next_chord' }); break;
    case 'n': send({ cmd: 'next_section' }); break;
    case 'e': if (editor) editor.toggle(); break;
    case '?': toggleLegend(); break;
    case 'Escape': $('#legend').hidden = true; if (editor) editor.hide(); state.pinned = null; state.hlEvent = null; state.tickerDirty = true; break;
    default: return;
  }
});

function toggleLegend() { const l = $('#legend'); l.hidden = !l.hidden; }
$('#help').addEventListener('click', toggleLegend);

// ---------------------------------------------------------------------------
// Audio playback
// ---------------------------------------------------------------------------
const audio = {
  ctx: null, node: null, gain: null, on: false,
  srcRate: 48000, channels: 2,
  vuL: 0, vuR: 0, bufferedMs: 0, underruns: 0, playing: false,
};

// A rendered chunk (interleaved stereo f32) from either transport → VU + worklet.
function onAudio(f32, sampleRate) {
  if (!audio.node) return; // stopped while frames were still in flight
  if (sampleRate && sampleRate !== audio.srcRate) {
    audio.srcRate = sampleRate;
    audio.node.port.postMessage({ type: 'config', srcRate: sampleRate, targetMs: 200, maxMs: 500 });
  }
  let pl = 0, pr = 0;
  for (let i = 0; i < f32.length; i += 2) {
    const al = Math.abs(f32[i]), ar = Math.abs(f32[i + 1]);
    if (al > pl) pl = al;
    if (ar > pr) pr = ar;
  }
  audio.vuL = Math.max(pl, audio.vuL * 0.8);
  audio.vuR = Math.max(pr, audio.vuR * 0.8);
  audio.node.port.postMessage({ type: 'pcm', data: f32 }, [f32.buffer]);
}

async function startAudio() {
  const btn = $('#listen');
  btn.disabled = true;
  try {
    let ctx;
    try { ctx = new AudioContext({ sampleRate: audio.srcRate, latencyHint: 'interactive' }); }
    catch (_) { ctx = new AudioContext({ latencyHint: 'interactive' }); }
    await ctx.resume();
    await ctx.audioWorklet.addModule(new URL('./worklet.js', import.meta.url));
    const node = new AudioWorkletNode(ctx, 'sonder-player', { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    const gain = ctx.createGain();
    gain.gain.value = +$('#vol').value;
    node.connect(gain).connect(ctx.destination);
    node.port.onmessage = (e) => {
      const m = e.data;
      if (m && m.type === 'stats') { audio.bufferedMs = m.bufferedMs; audio.underruns = m.underruns; audio.playing = m.playing; }
    };
    node.port.postMessage({ type: 'config', srcRate: audio.srcRate, targetMs: 200, maxMs: 500 });
    audio.ctx = ctx; audio.node = node; audio.gain = gain;
    // In worker mode the engine may have died (error) or not started yet: (re)start it.
    if (transport && transport.mode === 'worker' && !transport.started && currentScene) startScene(currentScene, currentSeed);
    transport.startAudio();
    audio.on = true;
    btn.textContent = '■ Stop';
    btn.classList.add('on');
  } catch (e) {
    console.error('audio start failed', e);
    setText($('#buf'), 'audio unavailable');
    $('#buf').className = 'buf mono bad';
  } finally {
    btn.disabled = false;
  }
}

async function stopAudio() {
  audio.on = false;
  if (transport) transport.stopAudio();
  if (audio.ctx) { try { await audio.ctx.close(); } catch (_) { /* ignore */ } }
  audio.ctx = null; audio.node = null; audio.gain = null;
  audio.vuL = audio.vuR = 0; audio.bufferedMs = 0; audio.playing = false;
  const btn = $('#listen');
  btn.textContent = '▶ Listen';
  btn.classList.remove('on');
  setText($('#buf'), '—');
  $('#buf').className = 'buf mono dim';
}

$('#listen').addEventListener('click', () => (audio.on ? stopAudio() : startAudio()));
$('#vol').addEventListener('input', (e) => { if (audio.gain) audio.gain.gain.value = +e.target.value; });

// VU meter + buffer health, drawn at ~15 Hz.
const vuCanvas = $('#vu');
function drawVu() {
  const g = vuCanvas.getContext('2d');
  const w = vuCanvas.width, h = vuCanvas.height;
  g.clearRect(0, 0, w, h);
  const bar = (v, y) => {
    const db = 20 * Math.log10(Math.max(1e-4, v));
    const frac = clamp((db + 48) / 48, 0, 1);
    g.fillStyle = frac > 0.92 ? '#e2635a' : frac > 0.75 ? '#e2b04a' : '#5fd39a';
    g.fillRect(0, y, frac * w, h / 2 - 2);
  };
  if (audio.on) { bar(audio.vuL, 1); bar(audio.vuR, h / 2 + 1); audio.vuL *= 0.9; audio.vuR *= 0.9; }
  if (audio.on) {
    const b = audio.bufferedMs;
    const el = $('#buf');
    setText(el, `${b.toFixed(0)} ms${audio.underruns ? ` · ${audio.underruns} drop${audio.underruns === 1 ? '' : 's'}` : ''}`);
    el.className = 'buf mono ' + (!audio.playing ? 'warn' : b < 80 || b > 450 ? 'warn' : 'ok');
  }
}
setInterval(drawVu, 66);

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------
window.addEventListener('resize', resize);
requestAnimationFrame(frame);
boot().catch((e) => { console.error(e); banner(`failed to start: ${e.message || e}`, true); setConn('closed'); });
