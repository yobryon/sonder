// Sonder web UI. Vanilla ES module, no build step, no network beyond this host.
//
// Data in:  /ws  → EngineSnapshot JSON at ~20 Hz (see sonder-core/src/snapshot.rs)
// Data out: /ws  ← UserCommand JSON  {"cmd":"toggle_mute","layer":0}
// Audio:    /audio → header JSON, then i16 stereo PCM frames → AudioWorklet

'use strict';

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
  seenEvents: new Set(),
  notes: new Map(),       // key -> NoteSnapshot (latest copy)
  rings: [],              // transient flashes on the stage {x,y,layer,t0,intensity}
  pings: [],              // map pings {x,y,layer,t0,intensity}
  laneY: new Map(),       // freeway: lane -> smoothed y
  layerSig: '',
  sourceSig: '',
  tickerDirty: true,
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
  if (num('lon01') !== null && num('lat01') !== null) return { x: num('lon01'), y: 1 - num('lat01') };
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
  for (const e of snap.events || []) {
    const key = `${e.at}|${e.layer}|${e.name}`;
    if (state.seenEvents.has(key)) continue;
    state.seenEvents.add(key);
    state.events.push({ ...e, wall });
    state.tickerDirty = true;
    const p = eventPosition(e, snap);
    const ring = { x: p.x, y: p.y, layer: e.layer, t0: performance.now(), intensity: clamp(e.intensity ?? 0.5, 0, 1.5) };
    state.rings.push(ring);
    if (e.attrs && typeof e.attrs.lat01 === 'number') state.pings.push(ring);
  }
  if (state.events.length > 200) state.events.splice(0, state.events.length - 200);
  if (state.seenEvents.size > 4000) {
    state.seenEvents = new Set(state.events.map((e) => `${e.at}|${e.layer}|${e.name}`));
  }

  for (const n of snap.notes || []) state.notes.set(`${n.at}|${n.layer}|${n.midi}`, n);
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
  setText($('#note'), s.conductor_note || ' ');
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

function updateTicker() {
  const list = $('#event-list');
  const recent = state.events.slice(-12).reverse();
  const layers = state.snap?.layers || [];
  list.innerHTML = recent.map((e) => {
    const l = layers.find((x) => x.id === e.layer);
    const attrs = Object.entries(e.attrs || {}).slice(0, 3).map(([k, v]) => `${k}=${fmtAttr(v)}`).join(' ');
    const c = col(e.layer);
    return `<li><span>${hhmmss(e.wall)}</span><span class="edot" style="background:${c}"></span>` +
      `<span class="ename">${l ? l.name : 'L' + e.layer} · ${e.name}</span>` +
      `<span class="ebar"><i style="width:${clamp(e.intensity ?? 0, 0, 1) * 100}%;background:${c}"></i></span>` +
      `<span class="eattrs" title="${attrs}">${attrs}</span></li>`;
  }).join('') || '<li class="empty">waiting for events…</li>';
}

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
  if ((s.events || []).some((e) => e.attrs && typeof e.attrs.lat01 === 'number')) return 'map';
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

// --- map -----------------------------------------------------------------
function drawMap(_s, Hv, now) {
  og.save();
  og.strokeStyle = 'rgba(255,255,255,0.05)'; og.lineWidth = 1;
  for (let i = 0; i <= 12; i++) { const x = (i / 12) * W; og.beginPath(); og.moveTo(x, 0); og.lineTo(x, Hv); og.stroke(); }
  for (let i = 0; i <= 6; i++) { const y = (i / 6) * Hv; og.beginPath(); og.moveTo(0, y); og.lineTo(W, y); og.stroke(); }
  // Equator and prime meridian a touch brighter.
  og.strokeStyle = 'rgba(255,255,255,0.1)';
  og.beginPath(); og.moveTo(0, Hv / 2); og.lineTo(W, Hv / 2); og.stroke();
  og.beginPath(); og.moveTo(W / 2, 0); og.lineTo(W / 2, Hv); og.stroke();
  og.restore();

  const LIFE = 10000;
  state.pings = state.pings.filter((p) => now - p.t0 < LIFE);
  og.save();
  for (const p of state.pings) {
    const age = (now - p.t0) / LIFE;
    const x = p.x * W, y = p.y * Hv;
    const rMax = 10 + p.intensity * 70;
    const r = 3 + Math.pow(age, 0.5) * rMax;
    og.strokeStyle = col(p.layer, (1 - age) * 0.8); og.lineWidth = 1.5;
    og.beginPath(); og.arc(x, y, r, 0, Math.PI * 2); og.stroke();
    og.fillStyle = col(p.layer, (1 - age) * 0.9, 75);
    og.beginPath(); og.arc(x, y, 2.5, 0, Math.PI * 2); og.fill();
  }
  og.restore();
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

  // Notes scroll right → left; newest at the right edge.
  const pxPerSec = W / ROLL_WINDOW;
  const noteH = Math.max(2.5, rh / (MIDI_HI - MIDI_LO) * 1.6);
  for (const [k, n] of state.notes) {
    const start = now - n.at;                 // seconds ago the note began
    const dur = n.duration ?? (n.active ? start : 0.3);
    const x1 = W - start * pxPerSec;
    const w = Math.max(3, dur * pxPerSec);
    if (x1 + w < 0) { if (!n.active) state.notes.delete(k); continue; }
    if (x1 > W) continue;
    const y = top + (1 - clamp((n.midi - MIDI_LO) / (MIDI_HI - MIDI_LO), 0, 1)) * rh - noteH / 2;
    const vel = clamp(n.velocity ?? 0.7, 0, 1);
    og.fillStyle = col(n.layer, 0.45 + 0.55 * vel, 45 + vel * 30);
    if (n.active) { og.shadowBlur = 14; og.shadowColor = col(n.layer, 0.9, 70); } else { og.shadowBlur = 0; }
    og.fillRect(x1, y, Math.min(w, W - x1 + 2), noteH);
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
  }
  requestAnimationFrame(frame);
}

// ---------------------------------------------------------------------------
// Snapshot WebSocket with reconnect
// ---------------------------------------------------------------------------
let ws = null;
let backoff = 500;
const wsProto = location.protocol === 'https:' ? 'wss:' : 'ws:';

function setConn(stateName) {
  $('#dot').className = `dot ${stateName}`;
  setText($('#conn-label'), stateName);
}

function connect() {
  setConn('connecting');
  ws = new WebSocket(`${wsProto}//${location.host}/ws`);
  ws.onopen = () => { setConn('live'); backoff = 500; };
  ws.onmessage = (ev) => {
    try { ingest(JSON.parse(ev.data)); } catch (e) { console.warn('bad snapshot', e); }
  };
  ws.onclose = () => {
    setConn('closed');
    ws = null;
    setTimeout(connect, backoff);
    backoff = Math.min(5000, backoff * 1.7);
  };
  ws.onerror = () => { /* onclose follows */ };
}

function send(cmd) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(cmd));
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------
window.addEventListener('keydown', (e) => {
  const tag = (e.target && e.target.tagName) || '';
  if (tag === 'INPUT' || tag === 'TEXTAREA' || e.metaKey || e.ctrlKey || e.altKey) return;
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
    case '?': toggleLegend(); break;
    case 'Escape': $('#legend').hidden = true; break;
    default: return;
  }
});

function toggleLegend() { const l = $('#legend'); l.hidden = !l.hidden; }
$('#help').addEventListener('click', toggleLegend);

// ---------------------------------------------------------------------------
// Audio playback
// ---------------------------------------------------------------------------
const audio = {
  ctx: null, node: null, gain: null, ws: null, on: false,
  srcRate: 48000, channels: 2,
  vuL: 0, vuR: 0, bufferedMs: 0, underruns: 0, playing: false,
};

async function startAudio() {
  const btn = $('#listen');
  btn.disabled = true;
  try {
    let ctx;
    try { ctx = new AudioContext({ sampleRate: audio.srcRate, latencyHint: 'interactive' }); }
    catch (_) { ctx = new AudioContext({ latencyHint: 'interactive' }); }
    await ctx.resume();
    await ctx.audioWorklet.addModule('/worklet.js');
    const node = new AudioWorkletNode(ctx, 'sonder-player', { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    const gain = ctx.createGain();
    gain.gain.value = +$('#vol').value;
    node.connect(gain).connect(ctx.destination);
    node.port.onmessage = (e) => {
      const m = e.data;
      if (m && m.type === 'stats') { audio.bufferedMs = m.bufferedMs; audio.underruns = m.underruns; audio.playing = m.playing; }
    };
    audio.ctx = ctx; audio.node = node; audio.gain = gain;
    openAudioSocket();
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

function openAudioSocket() {
  const sock = new WebSocket(`${wsProto}//${location.host}/audio`);
  sock.binaryType = 'arraybuffer';
  sock.onmessage = (ev) => {
    if (typeof ev.data === 'string') {
      let h;
      try { h = JSON.parse(ev.data); } catch (_) { return; }
      if (h.error) { setText($('#buf'), h.error); $('#buf').className = 'buf mono bad'; return; }
      audio.srcRate = h.sample_rate || audio.srcRate;
      audio.channels = h.channels || 2;
      audio.node.port.postMessage({ type: 'config', srcRate: audio.srcRate, targetMs: 200, maxMs: 500 });
      return;
    }
    const i16 = new Int16Array(ev.data);
    const f32 = new Float32Array(i16.length);
    let pl = 0, pr = 0;
    for (let i = 0; i < i16.length; i += 2) {
      const l = i16[i] / 32768, r = i16[i + 1] / 32768;
      f32[i] = l; f32[i + 1] = r;
      const al = Math.abs(l), ar = Math.abs(r);
      if (al > pl) pl = al;
      if (ar > pr) pr = ar;
    }
    audio.vuL = Math.max(pl, audio.vuL * 0.8);
    audio.vuR = Math.max(pr, audio.vuR * 0.8);
    audio.node.port.postMessage({ type: 'pcm', data: f32 }, [f32.buffer]);
  };
  sock.onclose = () => {
    if (audio.on && audio.ws === sock) setTimeout(() => { if (audio.on) openAudioSocket(); }, 1000);
  };
  audio.ws = sock;
}

async function stopAudio() {
  audio.on = false;
  if (audio.ws) { try { audio.ws.close(); } catch (_) { /* ignore */ } audio.ws = null; }
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
fetch('/api/snapshot').then((r) => r.json()).then((s) => { if (!state.snap) ingest(s); }).catch(() => { /* ws will deliver */ });
connect();
window.addEventListener('resize', resize);
requestAnimationFrame(frame);
