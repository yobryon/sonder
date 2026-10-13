# Sonder — Product Design

> *sonder* (n.): the realization that each random passerby is living a life as vivid and
> complex as your own.

**One line:** Sonder is a realtime generative music engine that listens to the world —
live, public, stackable data streams — and turns whatever those streams are doing into
an evolving piece of music you can sit inside of.

---

## 1. The idea, and where it goes

The founding image: a camera above a freeway at night. Headlights slide across the frame.
Each one is a person going somewhere. An ambient soundscape breathes with the traffic —
a swell when a platoon of cars rolls through, a lone bell when a single truck crosses
at 3 a.m., the key drifting brighter as dawn traffic builds.

That image contains everything Sonder is:

1. **A live source** (the camera) producing **events** (a car crosses) and **signals**
   (how many cars, how fast, how dense).
2. **A musical interpretation** that is *not* a sonification chart — not "pitch = speed" —
   but an **arrangement**: the data steers a composer, the composer writes music, a synth
   plays it. Data is the weather the music lives in, not a MIDI cable.
3. **Stackability.** The freeway is one voice. Add the earthquakes happening on the same
   planet right now as a sparse sub-bass. Add Wikipedia's edit firehose as a shimmer of
   plucked notes. Add the solar wind as the slow filter on the pad. Each source is a
   *layer* in an ensemble, and a global **conductor** keeps the ensemble in one key, one
   tempo, one mood.

So the product is not "a freeway synth". It is **an instrument whose player is the
world**, and a **composer** that keeps that player musical. The freeway is the first
preset.

### What makes it different from sonification

Most data-to-sound projects are instrumentation: map value to pitch, play. The result is
informative and unlistenable. Sonder inverts the priority: **it must always sound like
music**, and the data must be *legible* within the music. We achieve that with three
rules enforced by architecture, not by hoping:

- **Everything is quantized to a shared harmonic state.** Every layer asks the conductor
  "what notes are allowed right now?" before it plays. Twelve independent sources cannot
  produce a cluster chord because they never had independent pitch authority.
- **Data drives *parameters of generators*, not notes directly** (except for event-triggered
  hits, which are still pitch-quantized). Density, register, brightness, tension, step
  size, swing — these are what a signal controls. The generator does the composing.
- **Slow things stay slow.** Adaptive normalization and smoothing make a source that is
  "busy" relative to *itself* sound busy, whether it is 3 earthquakes/hour or 3,000
  trades/second. The engine never clips into chaos because a feed got loud.

---

## 2. Who it's for

- **Ambient listeners / focus-music people** who want generative music that is *about
  something* — the city they live in, the planet, the market, the night sky.
- **Makers, streamers, installations**: a gallery piece fed by a traffic camera; a
  lobby soundscape fed by the building's own sensors; a Twitch stream whose music reacts
  to chat velocity.
- **Developers / data people** who have a feed (a log, a metrics endpoint, a websocket)
  and want to *hear* it running — anomaly-detection-by-ear.
- **Musicians** who want an unpredictable but musical collaborator. MIDI out means Sonder
  can drive a hardware or DAW rig.

---

## 3. Core concepts (the vocabulary)

| Term | Meaning |
|---|---|
| **Source** | A live producer of raw data. HTTP poll, SSE, WebSocket, MJPEG camera, file tail, stdin, system metrics, or a synthetic simulation. Emits **Samples**. |
| **Sample** | A timestamped datum from a source: a **Reading** (named numeric channels: `speed=61.2, count=14`) or an **Event** (a discrete occurrence with an intensity and attributes: `car_crossed{lane=2, brightness=0.8}`). |
| **Signal** | A continuous, normalized (≈0..1) control value derived from samples through a **feature expression**: `norm(smooth(rate(event("car"), 10), 2))`. Signals are what the music responds to. |
| **Layer** | One stackable voice: a source + feature expressions + a musical **role** + an **instrument** + a **mapping** (which signals drive which musical parameters, and what an event triggers). |
| **Role** | What a layer *does* in the arrangement: `pad`, `drone`, `bass`, `lead`, `arp`, `pluck`, `bells`, `perc`, `texture`. Each role is a generator with its own musical logic. |
| **Conductor** | The global musical brain: key, mode, chord progression, tempo, meter, and **mood** (energy / tension / brightness). Reads aggregate signals from all layers and drifts harmony over time. Owns the pitch quantizer every layer uses. |
| **Instrument** | A synth patch (oscillators, filter, envelopes, effects sends). Instruments are presets referenced by name, or defined inline in a scene. |
| **Scene** | A declarative TOML file: a list of layers plus conductor settings. A scene is the shareable unit — "Night Freeway", "Planet Tonight", "Markets Open". |
| **Engine** | The running system: sources → signals → conductor+generators → synth → output + visualization. |

---

## 4. Experience

### 4.1 First run

```
$ sonder play night-freeway
```

Within a second you hear a slow pad in D Lydian. The TUI shows a dark frame; dots slide
across it — simulated headlights, because you haven't pointed it at a camera yet. Each dot
that leaves the frame plucks a note. The pad gets brighter as traffic builds. Press `w`
and a browser opens with the same thing rendered properly: a canvas of moving lights, a
live piano-roll, the current chord, and audio in the browser (so it works on machines with
no local audio, e.g. WSL or a headless server).

```
$ sonder play night-freeway --source camera.mjpeg --url http://.../mjpg/video.mjpg
```

Same scene, real traffic.

### 4.2 Stacking

```
$ sonder play planet-tonight
```

A curated scene stacking: USGS earthquakes (sub-bass hits, magnitude → weight,
longitude → pan), OpenSky flights over a region (density → arp density), Wikipedia
recent-changes (plucks; language → register; bot edits muted), NOAA solar wind (pad filter),
and local astronomical clock (moon phase → mode brightness; sunset → tempo falls).

Every layer can be muted/soloed live (`1`–`9`). The conductor shows *why* it's doing
what it's doing: "energy 0.62 (driven by wiki.rate), tension rising → Dorian in 2 bars".

### 4.3 Point it at anything

```
$ sonder probe https://api.example.com/metrics.json
  Polled 3× over 6s. Numeric fields that changed:
    .requests_per_sec      12.1 → 14.8 → 13.9   (suggest: norm(smooth(field, 2)))
    .active_users          812  → 815  → 819
    .errors                0    → 0    → 1       (sparse; suggest event on change)
  Write a starter scene? [Y/n]
```

Generic adapters (`http-json`, `sse`, `websocket`, `file-tail`, `stdin`) with a tiny
path language (`.a.b[0].c`) make any JSON-ish feed a source. `probe` writes the starter
scene for you.

### 4.4 Composing, not configuring

A scene is readable:

```toml
[conductor]
key = "D"
mode = "lydian"
tempo = 62
drift = "slow"            # how eagerly harmony wanders

[[layer]]
name = "traffic"
role = "pad"
instrument = "pad.glass"
source = { kind = "synthetic.freeway", lanes = 4, flow = 0.4 }
signals.density    = "norm(smooth(rate(event('car'), 10), 3))"
signals.speed      = "norm(smooth(field('mean_speed'), 4))"
map.energy         = "density"        # feeds the conductor's mood
map.brightness     = "speed"
map.register       = "0.3 + 0.4 * speed"

[[layer]]
name = "headlights"
role = "pluck"
instrument = "pluck.nylon"
source = "traffic"                     # share the source above
on_event.car.pitch    = "attr('lane') / 4"       # lane → degree within range
on_event.car.velocity = "0.4 + 0.6 * intensity"
on_event.car.pan      = "attr('x')"
on_event.car.quantize = "1/16"
```

### 4.5 Outputs

- Local audio (`cpal`), browser audio (web UI), WAV render (`sonder render`), raw PCM to
  stdout (pipe to `ffplay`/`paplay`/anything), and **MIDI out** to drive external gear.

---

## 5. The musical engine (the part that matters)

### 5.1 Conductor

- **Harmonic state**: tonic, mode (Ionian … Locrian plus pentatonics, whole-tone,
  harmonic minor, etc.), current chord (as scale degrees + extensions), and a
  **progression** policy: *functional* (I–vi–IV–V style walks with cadence weights),
  *modal drift* (change one note of the mode at a time, Lydian ↔ Ionian ↔ Mixolydian
  ↔ Dorian …, giving slow brightness changes), or *static*.
- **Mood vector**: `energy`, `tension`, `brightness`, `density` in 0..1. Each layer can
  publish into mood via `map.energy = ...`. The conductor aggregates (weighted mean),
  smooths heavily (minutes), and uses it to choose: mode brightness (brightness →
  Lydian side; low → Aeolian/Phrygian), chord complexity (tension → more extensions,
  borrowed chords), tempo nudges (energy, within scene bounds), and global dynamics.
- **Transport**: beat clock with tempo, meter, bar/beat/tick, swing. Emits ticks to
  generators; everything quantizable quantizes to it.
- **Pitch quantizer**: `nearest_in_scale`, `nearest_in_chord`, `degree(n)`,
  `register(0..1) → octave window`. Every note passes through here.
- **Voice-leading**: chord changes move pad voices by minimal motion.

### 5.2 Generators (roles)

| Role | Behaviour |
|---|---|
| `pad` | Sustains the current chord in a register; crossfades on changes with voice leading; `density` adds extensions; `brightness` → filter. |
| `drone` | Tonic (+5th) pedal; `texture` signal controls noise/detune. |
| `bass` | Root/fifth on strong beats; `density` adds passing tones and syncopation; `register` picks octave. |
| `lead` | Markov/random-walk melody in scale; `step` controls interval size, `density` note probability per subdivision, `register` window; rests when energy is low. |
| `arp` | Arpeggiates the current chord in a pattern (up/down/updown/random/euclidean); `density` → subdivision, `register` → span. |
| `pluck` / `bells` | Event-triggered. Each event → one note; event attrs → pitch/velocity/pan via the layer's `on_event` mapping; pitch is quantized to the chord or scale; optional grid quantization. |
| `perc` | Event-triggered percussion (kick/snare/hat/click/noise), or signal-driven Euclidean patterns. |
| `texture` | Granular-ish noise bed, filtered and panned by signals; no pitch. |

### 5.3 Synth

A polyphonic subtractive/FM synth, pure Rust, `f32`, block-based, lock-free from the
control thread:

- Oscillators: sine, triangle, saw and square (PolyBLEP anti-aliased), pulse w/ PWM,
  supersaw (7 detuned), FM pair (ratio/index), white/pink noise, wavetable.
- Per voice: 2 oscs with mix/detune, state-variable filter (LP/HP/BP, resonance,
  key-track, env/LFO/signal modulation), amp ADSR, filter ADSR, 1 LFO, pan, velocity
  curves. Voice stealing by oldest/quietest.
- Effects buses: tempo-synced stereo delay (feedback, filter, ping-pong), plate-style
  reverb (Dattorro-inspired FDN), chorus, and a master chain with soft saturation and a
  lookahead-free limiter.
- Mixer: per-layer gain/pan/sends; master. Metering (RMS/peak per layer) and a
  spectrum feed for the UI.
- Preset library: `pad.glass`, `pad.warm`, `drone.sub`, `bass.sub`, `bass.pluck`,
  `lead.soft`, `pluck.nylon`, `pluck.glass`, `bells.fm`, `perc.kick`, `perc.hat`,
  `perc.click`, `texture.wind`, … Scenes can also define instruments inline.

### 5.4 Signal processing (features)

A small expression language evaluated at a fixed control rate (50 Hz) with stateful
nodes:

- Sources of truth: `field("name")` (latest reading of a channel), `event("name")`
  (impulse train), `intensity`, `attr("k")` (inside `on_event` only), `time`, `beat`.
- Stateful: `smooth(x, seconds)`, `rate(events, window_s)` (events/sec), `count(events,
  window_s)`, `derivative(x)`, `norm(x)` (adaptive min/max with slow decay), `norm(x, lo,
  hi)`, `ema`, `lag(x, s)`, `onset(x, thresh)` (→ events), `hold(x, s)`, `max(x, s)`.
- Pure: arithmetic, `clamp`, `lerp`, `pow`, `abs`, `min`, `max`, `step`, `hash(text)` (→
  stable 0..1), `if(c, a, b)`, comparisons.

Why adaptive normalization matters: it's the mechanism that makes any feed musical.
A source that is loud *for itself* is loud in the music.

---

## 6. Sources (curated gallery)

Always-works, no network: `synthetic.freeway` (the founding image, simulated),
`system.metrics` (CPU/mem/net), `clock.astro` (sun/moon from lat/lon),
`synthetic.random-walk`.

Live, public, no API key:

| Source | Protocol | Gives |
|---|---|---|
| `camera.mjpeg` | MJPEG over HTTP / stdin | motion energy, blob count, blob crossing events with x/y/size/brightness. Any traffic/weather/harbor cam. `ffmpeg … -f mjpeg -` for everything else. |
| `wikipedia.recent-changes` | SSE | edit events (bytes changed, wiki, bot, user hash), edit rate |
| `usgs.earthquakes` | HTTP poll (GeoJSON) | quake events (magnitude, depth, lat/lon) |
| `opensky.flights` | HTTP poll | aircraft count, mean alt/speed, new-aircraft events, per-bbox |
| `open-meteo.weather` | HTTP poll | temperature, wind, clouds, precipitation, pressure |
| `noaa.space-weather` | HTTP poll | solar wind speed/density, Bz, Kp |
| `coinbase.trades` | WebSocket | trade events (size, side, price), price signal |
| `iss.position` | HTTP poll | lat/lon/velocity |
| `hackernews.firehose` | HTTP poll | new stories/comments as events, rate |
| `github.events` | HTTP poll | public push/PR/issue events |
| `noaa.tides` | HTTP poll | water level (slow) |

Generic: `http-json`, `sse`, `websocket`, `file-tail`, `stdin` (JSON lines), each with a
field-path extractor and `events_on = "change" | "every" | "threshold"` policies.

---

## 7. Interfaces

- **CLI** `sonder`: `play <scene|path>`, `list` (scenes), `sources`, `instruments`,
  `probe <url>`, `render <scene> --seconds N --out f.wav`, `serve` (web only, headless),
  `validate <scene>`. Global flags: `--web [port]`, `--no-audio`, `--midi <port>`,
  `--pcm-stdout`, `--seed`.
- **TUI** (ratatui): layers with meters & mute/solo, signal sparklines, event ticker,
  scrolling piano-roll, conductor panel (key/mode/chord/tempo/mood), source health,
  master spectrum. Keys: `1-9` mute, `shift+1-9` solo, `space` pause, `[`/`]` tempo,
  `-`/`=` density, `k` next key, `m` next mode, `w` open web UI, `s` save snapshot
  scene, `q` quit.
- **Web UI** (embedded, served by `axum`): same state over WebSocket plus **audio
  streamed to the browser** (PCM frames → AudioWorklet), and a scene-specific
  visualizer (the freeway renders as headlights). Zero build-step frontend (vanilla JS).

---

## 8. Architecture

Rust workspace, one crate per concern, all depending on `sonder-core` for shared types:

```
sources ──Sample──▶ features (expr) ──Signal/Event──▶ composer ──SynthCommand──▶ dsp ──▶ audio out
                                                        │                          │
                                                        ▼                          ▼
                                                   EngineSnapshot ◀────── meters/spectrum
                                                        │
                                              TUI / Web (WebSocket) / log
```

- **Threads**: tokio runtime for sources + web; a dedicated control thread (50 Hz) for
  features + composer; a realtime audio callback thread for DSP. Control→audio over a
  lock-free SPSC ring (`rtrb`); audio→control meters over an atomic snapshot.
- **Determinism**: `render` runs the whole pipeline offline with a fixed seed and a
  virtual clock; synthetic sources are seeded. Same scene + seed + synthetic sources ⇒
  identical WAV. Tests rely on this.
- **Crates**: `sonder-core` (types, scene schema, commands), `sonder-expr` (feature
  language), `sonder-sources`, `sonder-vision` (MJPEG + motion/blob tracking),
  `sonder-theory` (pitch/scale/chord/progression), `sonder-compose` (conductor,
  transport, generators), `sonder-dsp` (synth + effects), `sonder-audio` (cpal/wav/pcm
  backends), `sonder-engine` (wiring), `sonder-web`, `sonder-tui`, `sonder` (CLI).

---

## 9. Non-goals (v1)

Streaming to listeners at scale; DAW plugin (VST/CLAP) packaging; machine-learned
"taste"; cloud accounts. All plausible later, none needed to make the thing sing.

## 10. Success criteria

1. `sonder play night-freeway` sounds good for an hour, unattended, with no audible
   glitches, in a terminal with no sound card (via web audio).
   — *Verified 2026-10-13: 1 h headless soak, 0 underruns, memory flat at 16.6 MB, harmony
   drifting Lydian ↔ Ionian; audio and visuals checked in Chrome via the web UI.*
2. A listener can tell, blind, whether traffic is heavy or light.
   — *Mechanism in place: traffic density → pad energy/register, pluck rate, bass density,
   arp volume; the rush cycle is audible in renders. Listening study not yet run.*
3. Any JSON endpoint becomes a playable layer in under two minutes via `probe`.
   — *Verified: `sonder probe http://api.open-notify.org/iss-now.json -o iss.toml` writes a
   scene that validates and plays.*
4. Stacking five live sources stays in one key and one groove.
   — *Verified: `planet-tonight` ran with USGS, OpenSky, Wikipedia, NOAA SWPC and the astro
   clock all live, every note quantized to the conductor's scale.*
5. `render` is deterministic and tested.
   — *Verified: byte-identical WAVs for the same seed (engine + CLI tests).*
