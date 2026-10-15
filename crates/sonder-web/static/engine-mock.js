// Dev stand-in for site/pkg/sonder_wasm.js (same exports, same shapes). Produces a quiet
// 220 Hz sine and a plausible, evolving fake snapshot so the UI, transport, worker timing
// and source adapters can be developed with no wasm build. Select with `?mock=1`.

const SCENE = (name, title, description, tags, layers) =>
  ({ name, title, description, tags, toml: `name = "${title}"\ndescription = "${description}"\ntags = [${tags.map((t) => `"${t}"`).join(', ')}]\n\n` + layers });

const SCENES = [
  SCENE('night-freeway', 'Night Freeway', 'Headlights over an overpass at 3am. Simulated traffic, or point it at a camera.', ['ambient', 'offline', 'camera', 'founding'],
    `viz = "freeway"\n\n[[layer]]\nname = "traffic"\nrole = "pad"\ninstrument = "pad.glass"\nsource = { kind = "synthetic.freeway", lanes = 4, flow = 0.4, speed = 0.55 }\n\n[[layer]]\nname = "headlights"\nrole = "pluck"\ninstrument = "pluck.glass"\nsource = "traffic"\n\n[[layer]]\nname = "trucks"\nrole = "bells"\ninstrument = "bells.glass"\nsource = "traffic"\n\n[[layer]]\nname = "undertow"\nrole = "bass"\ninstrument = "bass.sub"\nsource = "traffic"\n`),
  SCENE('planet-tonight', 'Planet Tonight', 'Earthquakes, aircraft, Wikipedia edits, solar wind and the moon — stacked.', ['live', 'ambient', 'stacked'],
    `viz = "map"\n\n[[layer]]\nname = "sky"\nrole = "pad"\ninstrument = "pad.warm"\nsource = { kind = "clock.astro", lat = 51.5, lon = -0.12 }\n\n[[layer]]\nname = "solar-wind"\nrole = "drone"\ninstrument = "drone.sub"\nsource = { kind = "noaa.space-weather" }\n\n[[layer]]\nname = "quakes"\nrole = "perc"\ninstrument = "perc.kit"\nsource = { kind = "usgs.earthquakes", feed = "all_hour", min_magnitude = 1.0 }\n\n[[layer]]\nname = "aircraft"\nrole = "arp"\ninstrument = "arp.pulse"\nsource = { kind = "opensky.flights", bbox = [50.5, -1.5, 52.0, 1.0] }\n\n[[layer]]\nname = "edits"\nrole = "pluck"\ninstrument = "pluck.nylon"\nsource = { kind = "wikipedia.recent-changes", include_bots = false }\n`),
  SCENE('wikipedia-rain', 'Wikipedia Rain', 'Live Wikipedia edits as rainfall on a glass roof.', ['live', 'ambient'],
    `viz = "generic"\n\n[[layer]]\nname = "edits"\nrole = "pluck"\ninstrument = "pluck.glass"\nsource = { kind = "wikipedia.recent-changes", include_bots = false, types = ["edit"] }\n\n[[layer]]\nname = "roof"\nrole = "pad"\ninstrument = "pad.strings"\nsource = "edits"\n`),
  SCENE('markets-open', 'Markets Open', 'Live BTC-USD trades: buys rise, sells fall, volume decides how loud.', ['live', 'rhythmic'],
    `viz = "generic"\n\n[[layer]]\nname = "tape"\nrole = "pluck"\ninstrument = "pluck.marimba"\nsource = { kind = "coinbase.trades", products = ["BTC-USD"] }\n`),
  SCENE('hacker-news', 'Hacker News', 'New stories ring, comments patter. Rate is the energy.', ['live'],
    `viz = "generic"\n\n[[layer]]\nname = "items"\nrole = "pluck"\ninstrument = "pluck.marimba"\nsource = { kind = "hackernews.firehose" }\n`),
  SCENE('weather-station', 'Weather Station', 'Wind, cloud, rain, pressure, tide and sun for one place.', ['live', 'ambient', 'slow'],
    `viz = "generic"\n\n[[layer]]\nname = "sky"\nrole = "pad"\ninstrument = "pad.choir"\nsource = { kind = "open-meteo.weather", lat = 37.77, lon = -122.42 }\n\n[[layer]]\nname = "tide"\nrole = "bass"\ninstrument = "bass.sub"\nsource = { kind = "noaa.tides", station = "9414290" }\n\n[[layer]]\nname = "sun"\nrole = "drone"\ninstrument = "drone.saw"\nsource = { kind = "clock.astro", lat = 37.77, lon = -122.42 }\n`),
  SCENE('orbit', 'Orbit', 'The ISS position as a 92-minute melody.', ['live', 'ambient', 'slow'],
    `viz = "map"\n\n[[layer]]\nname = "station"\nrole = "lead"\ninstrument = "lead.sine"\nsource = { kind = "iss.position" }\n`),
  SCENE('this-browser', 'This Browser', 'Your mouse, clicks, scroll and keys. Works anywhere a browser does.', ['offline', 'browser', 'ambient'],
    `viz = "generic"\n\n[[layer]]\nname = "motion"\nrole = "pad"\ninstrument = "pad.warm"\nsource = { kind = "browser.input" }\n`),
  SCENE('this-machine', 'This Machine', 'Your own CPU, memory, network and processes.', ['offline', 'ambient'],
    `viz = "generic"\n\n[[layer]]\nname = "cpu"\nrole = "pad"\ninstrument = "pad.warm"\nsource = { kind = "system.metrics" }\n`),
];

export default async function init() { return {}; }
export function version() { return 'mock-0.0'; }
export function builtin_scenes() { return SCENES.map((s) => ({ ...s, tags: [...s.tags] })); }
export function validate_scene(toml) {
  const errs = [];
  if (!/^name\s*=/m.test(toml)) errs.push('missing `name`');
  if (!/\[\[layer\]\]/.test(toml)) errs.push('no layers');
  return errs;
}

const BUILTIN = (kind) => kind.startsWith('synthetic.') || kind === 'clock.astro';

/** Minimal TOML scraping: layer names, roles, instruments and inline `source = {...}` tables. */
function parseScene(toml) {
  const name = /^name\s*=\s*"([^"]*)"/m.exec(toml)?.[1] || 'untitled';
  const viz = /^viz\s*=\s*"([^"]*)"/m.exec(toml)?.[1] || 'generic';
  const layers = [];
  const specs = [];
  const blocks = toml.split(/\[\[layer\]\]/).slice(1);
  blocks.forEach((b, i) => {
    const lname = /^name\s*=\s*"([^"]*)"/m.exec(b)?.[1] || `layer${i}`;
    const role = /^role\s*=\s*"([^"]*)"/m.exec(b)?.[1] || 'pad';
    const instrument = /^instrument\s*=\s*"([^"]*)"/m.exec(b)?.[1] || '';
    const src = /^source\s*=\s*(\{[^\n]*\}|"[^"]*")/m.exec(b)?.[1];
    let source = null;
    if (src && src.startsWith('{')) {
      const kind = /kind\s*=\s*"([^"]*)"/.exec(src)?.[1] || 'unknown';
      const params = {};
      for (const m of src.matchAll(/(\w+)\s*=\s*("([^"]*)"|\[[^\]]*\]|[-\d.]+|true|false)/g)) {
        if (m[1] === 'kind') continue;
        let v = m[2];
        if (m[3] !== undefined) v = m[3];
        else if (v.startsWith('[')) { try { v = JSON.parse(v); } catch (_) { v = []; } }
        else if (v === 'true' || v === 'false') v = v === 'true';
        else v = parseFloat(v);
        params[m[1]] = v;
      }
      specs.push({ name: lname, kind, params, builtin: BUILTIN(kind) });
      source = lname;
    } else if (src) source = src.slice(1, -1);
    layers.push({ id: i, name: lname, role, instrument, source });
  });
  return { name, viz, layers, specs };
}

export class Engine {
  constructor(sceneToml, seed, sampleRate) {
    const errs = validate_scene(sceneToml);
    if (errs.length) throw new Error(errs.join('; '));
    this.scene = parseScene(sceneToml);
    this.seed = BigInt(seed);
    this.sr = sampleRate;
    this.fpt = Math.round(sampleRate / 50);
    this.t = 0;
    this.phase = 0;
    this.paused = false;
    this.bpm = 62;
    this.beats = 0;
    this.muted = new Set();
    this.solo = null;
    this.events = [];
    this.notes = [];
    this.sources = new Map(this.scene.specs.map((s) => [s.name, { name: s.name, kind: s.kind, state: s.builtin ? 'live' : 'connecting', message: s.builtin ? 'simulated' : '', rate: 0, last_sample_at: null, readings: {}, times: [] }]));
    this.viz = {};
    this.captions = [];
    this.section = 0;
    this.sectionAt = 0;
    this.cars = [];
    this.lastBeat = -1;
    this.rng = Number(this.seed % 2147483647n) || 1;
  }
  rand() { this.rng = (this.rng * 48271) % 2147483647; return this.rng / 2147483647; }
  free() {}
  frames_per_tick() { return this.fpt; }
  time() { return this.t; }
  source_specs() { return this.scene.specs.map((s) => ({ ...s, params: { ...s.params } })); }
  layerFor(source) { const l = this.scene.layers.find((x) => x.source === source); return l ? l.id : 0; }
  push_sample(source, json) {
    const d = JSON.parse(json);
    const s = this.sources.get(source);
    if (!s) throw new Error(`unknown source \`${source}\``);
    s.times.push(this.t); s.last_sample_at = this.t;
    if (d.type === 'status') { s.state = d.state; s.message = d.message; }
    else if (d.type === 'reading') Object.assign(s.readings, d.values);
    else if (d.type === 'event') {
      const layer = this.layerFor(source);
      this.emitEvent(layer, d.name, d.intensity, d.attrs || {});
    } else if (d.type === 'viz') this.viz[source] = d.payload;
    else throw new Error(`bad sample type ${d.type}`);
  }
  // An event → ticker row (with a caption label) → a note that remembers its cause; the
  // salient ones also become captions.
  emitEvent(layer, name, intensity, attrs) {
    const at = this.t;
    const first = Object.keys(attrs).slice(0, 2).map((k) => `${k} ${typeof attrs[k] === 'number' ? attrs[k].toFixed(2) : attrs[k]}`).join(', ');
    const label = `${name}${first ? ` · ${first}` : ''} on ${this.scene.layers[layer]?.name || 'L' + layer}`;
    this.events.push({ at, layer, name, intensity, attrs, label });
    const cause = { event: name, at, label, attrs };
    this.notes.push({ at, layer, midi: 48 + Math.floor(this.rand() * 36), velocity: 0.3 + 0.6 * Math.min(1, intensity), pan: 0.5, duration: 0.3 + 2 * Math.min(1, intensity), active: true, cause, reason: 'trigger' });
    if (intensity > 0.55 || this.rand() < 0.08) {
      this.captions.push({ at, layer, text: intensity > 0.85 ? `${label} — the loudest in a while` : label, importance: Math.min(1, intensity) });
      if (this.captions.length > 16) this.captions.splice(0, this.captions.length - 16);
    }
  }
  push_frame(source, gray, w, h) {
    if (!(gray instanceof Uint8Array) || gray.length !== w * h) throw new Error('bad frame');
    let sum = 0; for (let i = 0; i < gray.length; i += 7) sum += gray[i];
    const mean = sum / Math.ceil(gray.length / 7) / 255;
    const s = this.sources.get(source);
    if (s) { s.state = 'live'; s.readings.mean_brightness = mean; s.readings.fps = 15; s.times.push(this.t); }
    this.viz[source] = { blobs: [{ x: 0.5, y: 0.5, w: 0.1, h: 0.1, b: mean, id: 1 }], line: 0.5, w, h, motion: 0.1 };
  }
  command(json) {
    const c = JSON.parse(json);
    switch (c.cmd) {
      case 'toggle_mute': this.muted.has(c.layer) ? this.muted.delete(c.layer) : this.muted.add(c.layer); break;
      case 'toggle_solo': this.solo = this.solo === c.layer ? null : c.layer; break;
      case 'toggle_pause': this.paused = !this.paused; break;
      case 'nudge_tempo': this.bpm = Math.max(40, Math.min(200, this.bpm + c.delta)); break;
      case 'next_section': this.section++; this.sectionAt = this.t; break;
      case 'load_scene': break;
      case 'nudge_density': case 'transpose': case 'next_mode': case 'next_chord': case 'set_layer_gain': case 'save_scene': case 'quit': break;
      default: throw new Error(`unknown command ${c.cmd}`);
    }
  }
  tick(out) {
    if (out.length !== 2 * this.fpt) throw new Error('bad buffer length');
    const dt = this.fpt / this.sr;
    const amp = this.paused ? 0 : 0.05;
    const inc = (2 * Math.PI * 220) / this.sr;
    for (let i = 0; i < this.fpt; i++) { const v = Math.sin(this.phase) * amp; out[2 * i] = v; out[2 * i + 1] = v; this.phase += inc; if (this.phase > 2 * Math.PI) this.phase -= 2 * Math.PI; }
    if (!this.paused) {
      this.t += dt;
      this.beats += dt * this.bpm / 60;
      const beat = Math.floor(this.beats);
      if (beat !== this.lastBeat) {
        this.lastBeat = beat;
        this.notes.push({ at: this.t, layer: beat % Math.max(1, this.scene.layers.length), midi: 50 + [0, 2, 4, 6, 7, 9, 11][beat % 7], velocity: 0.5 + 0.3 * this.rand(), pan: this.rand(), duration: 0.5, active: true });
      }
      // Simulated freeway traffic for the viz.
      if (this.scene.specs.some((s) => s.kind === 'synthetic.freeway')) {
        if (this.rand() < dt * 1.2) this.cars.push({ x: this.rand(), lane: Math.floor(this.rand() * 4), y: 0, size: 0.3 + this.rand() * 0.9, bright: 0.6 + 0.4 * this.rand(), v: 0.08 + 0.1 * this.rand() });
        for (const c of this.cars) { c.y += c.v * dt; c.x += (c.lane / 3 - 0.5) * 0.02 * dt; }
        const gone = this.cars.filter((c) => c.y >= 1);
        this.cars = this.cars.filter((c) => c.y < 1);
        for (const c of gone) this.emitEvent(1, 'car', Math.min(1, 0.5 * c.size * c.bright), { lane: c.lane, x: c.x, size: c.size, speed: c.v * 6, brightness: c.bright });
        const src = this.sources.get('traffic');
        if (src) { src.readings = { count: this.cars.length, mean_speed: 0.5 + 0.2 * Math.sin(this.t / 7) }; src.times.push(this.t); }
        this.viz.traffic = { cars: this.cars.map((c) => ({ x: c.x, y: c.y, lane: c.lane, size: c.size, bright: c.bright })) };
      }
    }
      // Map scenes: fake geo events (quakes along the ring of fire-ish) and lightning strikes so
      // Live Earth has something to draw without the network.
      if (this.scene.viz === 'map') {
        if (this.rand() < dt * 0.7) {
          const lat = -40 + this.rand() * 90, lon = this.rand() < 0.5 ? 100 + this.rand() * 60 : -130 + this.rand() * 60;
          const mag = 1 + this.rand() * 5;
          this.emitEvent(Math.min(2, this.scene.layers.length - 1), 'quake', Math.min(1, mag / 6), { mag: Math.round(mag * 10) / 10, lat: Math.round(lat * 100) / 100, lon: Math.round(lon * 100) / 100, lat01: (lat + 90) / 180, lon01: (lon + 180) / 360, place: 'somewhere wet' });
        }
        this.strikes = (this.strikes || []).filter((s) => this.t - s.t < 5);
        if (this.rand() < dt * 6) { const lat = -20 + this.rand() * 40, lon = -70 + this.rand() * 120; this.strikes.push({ t: this.t, lat01: (lat + 90) / 180, lon01: (lon + 180) / 360 }); }
        this.viz.lightning = { strikes: this.strikes.map((s) => ({ lat01: s.lat01, lon01: s.lon01, age_s: this.t - s.t })) };
      }
      // Form: sections cycle every 16 bars (or on next_section).
      if ((this.beats - this.sectionAt * this.bpm / 60) / 4 >= 16) { this.section++; this.sectionAt = this.t; }
    }
    for (const n of this.notes) if (n.active && this.t - n.at > n.duration) n.active = false;
    if (this.notes.length > 256) this.notes.splice(0, this.notes.length - 256);
    if (this.events.length > 64) this.events.splice(0, this.events.length - 64);
  }
  snapshot_json() {
    const t = this.t;
    const bpb = 4;
    const beatsTotal = Math.floor(this.beats);
    const mood = { energy: 0.5 + 0.3 * Math.sin(t / 9), tension: 0.3 + 0.2 * Math.sin(t / 13 + 1), brightness: 0.55 + 0.25 * Math.sin(t / 17 + 2) };
    const sources = [];
    for (const s of this.sources.values()) {
      s.times = s.times.filter((x) => t - x <= 1);
      sources.push({ name: s.name, kind: s.kind, state: s.state, message: s.message, rate: s.times.length, last_sample_at: s.last_sample_at, readings: s.readings });
    }
    const layers = this.scene.layers.map((l, i) => ({
      id: l.id, name: l.name, role: l.role, instrument: l.instrument, muted: this.muted.has(l.id), soloed: this.solo === l.id,
      signals: { flow: 0.5 + 0.4 * Math.sin(t / (5 + i)), speed: 0.5 + 0.3 * Math.cos(t / (7 + i)) },
      params: { energy: Math.max(0, Math.min(1, mood.energy + 0.1 * i)), density: 0.4 + 0.2 * Math.sin(t / 4 + i) },
      meter: { rms: this.paused ? 0 : 0.02 + 0.02 * Math.abs(Math.sin(t * 2 + i)), peak: this.paused ? 0 : 0.08 },
      active_notes: this.notes.filter((n) => n.active && n.layer === l.id).length, source: l.source,
    }));
    return JSON.stringify({
      scene_name: this.scene.name, viz_kind: this.scene.viz, time: t, paused: this.paused,
      transport: { bar: Math.floor(beatsTotal / bpb), beat: beatsTotal % bpb, bpm: this.bpm, beats_per_bar: bpb },
      harmony: { tonic: 2, key_name: 'D', mode: 'lydian', scale: [2, 4, 6, 8, 9, 11, 1], chord: [2, 6, 9, 1], chord_name: 'Dmaj7', chord_degree: 1, beats_to_change: 8 - (beatsTotal % 8) },
      mood, conductor_note: `mock engine · energy ${mood.energy.toFixed(2)} drifting on a sine · seed ${this.seed}`,
      form: (() => {
        const names = ['intro', 'build', 'peak', 'release'];
        const section = names[this.section % names.length];
        const barsIn = Math.floor((this.beats - this.sectionAt * this.bpm / 60) / bpb);
        return { section, bar_in_section: Math.max(0, barsIn), bars_in_section: 16, arc: Math.min(1, Math.max(0, barsIn / 16)), motif: this.section % 2 ? 'B' : 'A', note: `${section} · motif ${this.section % 2 ? 'B' : 'A'} · ${this.section % 4 === 2 ? 'answering the loudest layer' : 'listening'}` };
      })(),
      captions: this.captions,
      layers, sources, events: this.events, notes: this.notes,
      master: { rms: this.paused ? 0 : 0.03, peak: this.paused ? 0 : 0.05 },
      spectrum: Array.from({ length: 48 }, (_, i) => Math.max(0, 0.6 * Math.exp(-((i - 8) ** 2) / 20) * (0.7 + 0.3 * Math.sin(t * 3 + i)))),
      viz: this.viz, underruns: 0, audio_backend: 'mock',
    });
  }
}
