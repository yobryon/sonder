# Sonder — Architecture & Crate Contracts

This document is the contract between crates. `sonder-core` holds the shared data
types (read `crates/sonder-core/src/*.rs` — they are short and heavily commented).
Each crate below lists the public API it must expose; other crates code against it.

```
sonder-sources ─┐
sonder-vision  ─┤ Sample ──▶ sonder-expr ──▶ params/events ──▶ sonder-compose ──▶ SynthCommand ──▶ sonder-dsp
                │                                 ▲                 │                                 │
                │                                 └── sonder-theory ┘                                 ▼
                │                                                                             sonder-audio (cpal/wav/pcm/tap)
                └──────────────────────── sonder-engine (wiring, snapshots) ──────────────────────────┘
                                                   │
                                   sonder-tui  ◀───┴───▶  sonder-web          sonder (CLI)
```

## Threads & timing

- **tokio runtime**: sources, web server, TUI event loop.
- **Control thread** (plain `std::thread`, 50 Hz tick = 20 ms): drains source samples,
  runs `sonder-expr` per layer, runs `sonder-compose`, pushes `SynthCommand`s into an
  `rtrb` SPSC ring to the audio thread, builds `EngineSnapshot` (~20 Hz) into a
  `tokio::sync::watch`.
- **Audio thread** (cpal callback or offline loop): pops commands, `Synth::render`.
  Never allocates, never locks, never logs.
- **Time**: `sonder_core::Clock` → `Seconds` since engine start. `WallClock` live,
  `VirtualClock` for `render` (advanced by audio frames). Composer and expr get `now`
  passed in; they never read wall time themselves (determinism).

## Conventions

- Values that feed music are **0..1 floats** unless documented otherwise.
- All crates: `cargo clippy -- -D warnings` clean, unit tests, `#![forbid(unsafe_code)]`
  except where a backend needs FFI (cpal is fine; our code stays safe).
- No `unwrap()` on external data. Sources never panic the engine; they report
  `Datum::Status` and retry with backoff.
- Determinism: anything random takes a seed (`rand_chacha::ChaCha8Rng`).
- Logging via `tracing`. Audio thread: none.

---

## sonder-expr

A tiny expression language for feature extraction and mapping, evaluated at control
rate with **stateful nodes** (each call-site of a stateful function owns its state).

### Grammar

```
expr     := or
or       := and ( "||" and )*
and      := cmp ( "&&" cmp )*
cmp      := add ( ("<" | "<=" | ">" | ">=" | "==" | "!=") add )?
add      := mul ( ("+" | "-") mul )*
mul      := unary ( ("*" | "/" | "%") unary )*
unary    := ("-" | "!") unary | pow
pow      := atom ( "^" unary )?
atom     := number | string | ident | ident "(" args ")" | "(" expr ")"
number   := 1.5 | 2 | 10s | 250ms | 2m        -- unit suffixes convert to seconds
string   := 'single' | "double"
```

Bare identifiers resolve, in order: a **signal** defined earlier in the same layer's
`signals` table (by name), then a **source channel** (`field("name")` shorthand), then
builtin constants (`time`, `beat`, `bar`, `bpm`, `pi`, `intensity` in event context).
Unknown identifiers are a compile error with a helpful message listing known names.

### Builtins

Pure: `abs min max clamp(x, lo, hi) lerp(a, b, t) pow sqrt exp log floor ceil round
step(edge, x) smoothstep(e0, e1, x) if(cond, a, b) sin cos hash(x)` (`hash` of a string
or number → stable 0..1), `has(attr_name)`, `attr("k")` (event ctx; text attrs hash to
0..1 unless they parse as numbers), `attr_text("k") == "..."` comparisons (string
equality yields 0/1).

Stateful (per call-site): 
- `field("name")` latest reading (holds last value; 0 before first).
- `event("name")` impulse train: 1.0 × intensity on the tick an event arrived (sums if several), else 0. `event("*")` = any.
- `smooth(x, seconds)` one-pole low-pass with time constant.
- `rate(events, window_s)` events per second over a sliding window (uses the impulse train's event *count*, not intensity).
- `count(events, window_s)`.
- `sum(x, window_s)` sliding-window sum of impulse intensities.
- `derivative(x)` per-second change, smoothed over 2 ticks.
- `norm(x)` adaptive normalization: tracks running min/max with slow decay (~2 min half-life) and maps to 0..1; `norm(x, lo, hi)` fixed range.
- `ema(x, alpha)`; `lag(x, s)`; `hold(x, s)` peak-hold with decay; `maxw(x, s)` / `minw(x, s)` windowed max/min.
- `onset(x, threshold)` → 1.0 on the tick `x` crosses up through `threshold`, else 0. `every(seconds)` → 1.0 periodically. `change(x)` → 1 when x changes. These are **edge detectors**; see `Fired` below.
- `random()` seeded per call-site, new value each tick; `walk(step, s)` seeded random walk 0..1.

### API

```rust
pub struct ExprError { pub message: String, pub span: Option<(usize, usize)> }   // Display impl
pub fn compile(src: &str, scope: &Scope) -> Result<Program, ExprError>;          // standalone
pub struct Scope { pub signals: Vec<String>, pub fields: Vec<String> /* may be empty = allow any */, pub event_ctx: bool }

/// Everything for one layer: signals (ordered, may reference earlier ones) then map exprs.
pub struct LayerPipeline { .. }
impl LayerPipeline {
    pub fn compile(signals: &BTreeMap<String,String>, map: &BTreeMap<String,String>, seed: u64) -> Result<Self, ExprError>;
    pub fn push_reading(&mut self, values: &BTreeMap<String, f64>);
    pub fn push_event(&mut self, name: &str, intensity: f64, attrs: &BTreeMap<String, Value>, at: Seconds);
    /// Advance one control tick. Evaluates signals in declaration order, then map.
    pub fn tick(&mut self, now: Seconds, dt: f64, beat: f64, bar: f64, bpm: f64) -> Vec<Fired>;
    pub fn signals(&self) -> &BTreeMap<String, f64>;
    pub fn params(&self) -> &BTreeMap<String, f64>;   // map results
    pub fn field_names(&self) -> Vec<String>;         // channels referenced (for probe/validation)
}
/// A signal whose top-level node is an edge detector fired this tick. The engine turns these
/// into LayerEvents named after the signal (so `signals.spike = "onset(norm(cpu), 0.8)"` makes
/// an event "spike" that `on_event.spike` can trigger notes from).
pub struct Fired { pub name: String, pub intensity: f64 }

/// Compiled `on_event` value expression. Evaluated once per event; may read the layer's current
/// signals by name, `intensity`, `attr("k")`, `time`, `beat`.
pub struct EventExpr { .. }
impl EventExpr {
    pub fn compile(src: &str, signal_names: &[String]) -> Result<Self, ExprError>;
    pub fn eval(&self, ctx: &EventCtx) -> f64;
}
pub struct EventCtx<'a> { pub intensity: f64, pub attrs: &'a BTreeMap<String, Value>, pub signals: &'a BTreeMap<String, f64>, pub time: Seconds, pub beat: f64 }
/// Parse "1/16", "1/8t", "1/4", "off" into beats (None = off). Shared helper.
pub fn parse_grid(s: &str) -> Result<Option<f64>, ExprError>;
```

## sonder-theory

Pure music theory, no time, no RNG state (take `&mut impl Rng` where needed).

```rust
pub struct Mode { pub name: &'static str, pub steps: Vec<u8> /* pitch classes from tonic */, pub brightness: i8 }
pub fn mode_by_name(s: &str) -> Option<Mode>;     // ionian/major, dorian, …, pentatonics, whole_tone, harmonic_minor, melodic_minor, blues, hirajoshi, in_sen, chromatic
pub fn modes_by_brightness() -> &'static [&'static str];  // locrian … lydian (the diatonic 7, in order)
pub struct Scale { pub tonic: u8, pub mode: Mode }
impl Scale { pub fn pitch_classes(&self) -> Vec<u8>; pub fn degree_to_midi(&self, degree: i32 /* 0-based, can exceed len → octaves */, octave: i32) -> i32; pub fn contains(&self, midi: i32) -> bool; pub fn nearest(&self, midi: f64) -> i32; pub fn name(&self) -> String; }
pub struct Chord { pub root_degree: u8 /* 0-based */, pub tones: Vec<u8> /* scale-degree offsets, e.g. [0,2,4] triad, [0,2,4,6] 7th */, }
impl Chord { pub fn pitch_classes(&self, scale: &Scale) -> Vec<u8>; pub fn name(&self, scale: &Scale) -> String /* "Dmaj7", "Em", "Bø" */; pub fn roman(&self, scale: &Scale) -> String; }
pub fn diatonic_chord(scale: &Scale, degree: u8, extensions: u8 /* 0 triad,1 seventh,2 ninth */) -> Chord;
pub struct Quantizer;  // fns: nearest_in(pcs: &[u8], midi: f64) -> i32; in_register(pcs, pos01, lo_midi, hi_midi) -> i32 (pick the (pos01)-th note of the set within the window)
pub struct Voicing(pub Vec<i32>);  // midi notes
pub fn voice_lead(from: &Voicing, to_pcs: &[u8], lo: i32, hi: i32, n: usize) -> Voicing;  // minimal total motion, no doubled pcs where possible
pub mod progression {
  pub fn functional_next(current_degree: u8, tension: f64, rng: &mut impl Rng) -> u8;   // weighted cadence table for 7-note scales
  pub fn modal_shift(mode_name: &str, brighter: bool) -> Option<&'static str>;          // one accidental away
}
pub fn euclidean(steps: usize, pulses: usize, rotation: usize) -> Vec<bool>;
pub fn interval_name(semitones: i32) -> &'static str;
```

## sonder-compose

```rust
pub struct Composer { .. }
pub struct ComposerInput<'a> {
    pub layers: &'a [LayerInput],        // indexed by LayerId.0
    pub events: &'a [LayerEvent],        // events this tick (from sources and from expr `Fired`)
}
pub struct LayerInput { pub params: BTreeMap<String, f64> /* resolved map keys */, pub signals: BTreeMap<String, f64> }
pub struct ComposerOutput { pub commands: Vec<SynthCommand>, pub notes: Vec<NoteSnapshot> /* new notes this tick */ }
impl Composer {
    pub fn new(scene: &Scene, seed: u64) -> Result<Self, ComposeError>;   // compiles on_event exprs via sonder-expr
    pub fn tick(&mut self, now: Seconds, dt: f64, input: &ComposerInput) -> ComposerOutput;
    pub fn handle(&mut self, cmd: &UserCommand);     // mute/solo/tempo/density/transpose/mode/chord/pause
    pub fn transport(&self) -> TransportPos;
    pub fn fill_snapshot(&self, snap: &mut EngineSnapshot);  // harmony, mood, conductor_note, transport, per-layer muted/soloed/active_notes/params
    pub fn is_paused(&self) -> bool;
    pub fn layer_muted(&self, id: LayerId) -> bool;  // effective (solo-aware)
}
```
Composer owns: transport (tempo, swing, humanize), conductor (mood aggregation from layer `energy/tension/brightness` params with `mood_weight`; mode drift; chord progression every `bars_per_chord`), one generator per layer by role, NoteId allocation, and scheduling of NoteOffs (it emits NoteOff itself at the planned time). All pitches go through the scale/chord quantizer. Event-driven roles evaluate `on_event` mappings (`"*"` wildcard, specific name wins), apply `chance`, `quantize` (defer trigger to next grid point), `snap`. Default register windows per role (bass C1–C3, pad C3–C5, lead C4–C6, …) shifted by `register` param. Mute = no new notes + AllNotesOff once. Mood also feeds `SetLayerParam` (brightness → CutoffOctaves, texture → Texture) and `SetTempo`.

## sonder-dsp

```rust
pub struct Synth { .. }
impl Synth {
    pub fn new(sample_rate: f32, max_layers: usize) -> Self;   // preallocates everything
    pub fn handle(&mut self, cmd: SynthCommand);                 // realtime-safe (no alloc/lock), Box<Instrument> is moved into a slot and the old one pushed to `recycle`
    pub fn render(&mut self, out: &mut [f32]);                   // interleaved stereo, any length (internally blocks of 64)
    pub fn master_meter(&self) -> Meter; pub fn layer_meter(&self, layer: LayerId) -> Meter;
    pub fn drain_feedback(&mut self, f: &mut dyn FnMut(SynthFeedback));
    pub fn take_recycled(&mut self) -> Option<Box<Instrument>>;   // control side frees these
    pub fn active_voices(&self) -> usize;
}
pub mod presets { pub fn get(name: &str) -> Option<Instrument>; pub fn names() -> Vec<&'static str>; pub fn default_for_role(role: Role) -> &'static str; pub fn describe(name: &str) -> Option<&'static str>; }
pub struct Analyzer { .. }  // control-side spectrum: new(sample_rate, bands=48), push(&[f32] interleaved), bands(&self) -> &[f32] (0..1 log-spaced, smoothed)
```
Realtime safety is checked by a test that renders 10 s with `#[global_allocator]` counting allocations after warm-up = 0.

## sonder-audio

```rust
pub struct SynthHost { .. }   // owns Synth + rtrb consumer + meters/feedback out; lives on the audio thread
pub struct SynthController { .. }  // control side: send(SynthCommand) (non-blocking, drops with counter if full), meters(), poll_feedback(), take_recycled()
pub fn synth_pair(sample_rate: f32, max_layers: usize, tap: Option<TapConfig>) -> (SynthController, SynthHost);
impl SynthHost { pub fn render(&mut self, out: &mut [f32]) }   // pops commands then renders; writes meters to shared atomics; pushes to tap ring
pub struct AudioTap { .. }  // control/web side: pop_chunk(&mut Vec<f32>) -> usize; for web streaming + Analyzer
pub enum Backend { Native /*cpal*/, Wav { path, seconds }, PcmStdout, Null /* consumes host on a timer thread at real speed */ }
pub struct AudioStream { .. } // start(backend, host) -> Result<AudioStream>; sample_rate(); keep alive; Drop stops
pub fn render_offline(host: &mut SynthHost, seconds: f64, mut on_block: impl FnMut(f64 /*t*/)) -> Vec<f32>;  // drives VirtualClock in caller via on_block
```

## sonder-sources

```rust
pub struct SourceCtx { pub id: SourceId, pub tx: tokio::sync::mpsc::Sender<Sample>, pub clock: Arc<dyn Clock>, pub http: reqwest::Client, pub shutdown: tokio::sync::watch::Receiver<bool>, pub seed: u64 }
#[async_trait-free]: pub trait Source: Send { fn run(self: Box<Self>, ctx: SourceCtx) -> Pin<Box<dyn Future<Output = ()> + Send>>; }
pub struct SourceInfo { pub kind: &'static str, pub summary: &'static str, pub params: Vec<ParamInfo>, pub channels: Vec<&'static str>, pub events: Vec<&'static str>, pub network: bool }
pub struct ParamInfo { pub name: &'static str, pub summary: &'static str, pub default: Option<String> }
pub type Factory = fn(&SourceSpec) -> Result<Box<dyn Source>, SourceError>;
pub struct Registry { .. }  // new() has all builtins; register(info, factory); create(spec) -> Result<Box<dyn Source>>; catalog() -> &[SourceInfo]
pub mod probe { pub async fn probe_json(url: &str, polls: usize, interval: Duration) -> Result<ProbeReport>; pub struct ProbeReport { pub fields: Vec<ProbedField { path, values: Vec<f64>, changed: bool, suggestion: String }> , pub events_suggestion: Option<String> } ; pub fn starter_scene(report, url) -> Scene }
pub mod jsonpath { pub fn extract(v: &serde_json::Value, path: &str) -> Option<&Value>; pub fn flatten_numeric(v, prefix) -> Vec<(String, f64)> }  // ".a.b[0].c"
```
Builtins (kinds): `synthetic.freeway` (lanes, flow 0..1, speed, seed; events `car{lane,x,speed,size,brightness}` on exit, readings `count, mean_speed, flow`; emits `Datum::Viz {cars:[{x,y,lane,size,bright}]}` at 10 Hz), `synthetic.walk`, `system.metrics`, `clock.astro` (lat, lon), `wikipedia.recent-changes` (wikis filter, include_bots), `usgs.earthquakes` (feed = hour|day, min_magnitude), `opensky.flights` (bbox), `open-meteo.weather` (lat, lon, interval), `noaa.space-weather`, `coinbase.trades` (products), `iss.position`, `hackernews.firehose`, `github.events`, `noaa.tides` (station), `http-json` (url, interval, fields, events_on), `sse` (url, fields, event_name), `websocket` (url, subscribe (json), fields, event_name), `file-tail` (path, format jsonl|csv|lines), `stdin`.
Every network source: timeout, exponential backoff with jitter, `Status` samples, polite default intervals, User-Agent `sonder/0.1 (+https://github.com/genuye/sonder)`.

## sonder-vision

```rust
pub struct TrackerConfig { pub threshold: f32 /*brightness 0..1*/, pub min_blob_px: u32, pub max_blobs: usize, pub downscale: u32, pub mode: Mode /* Bright (headlights) | Motion (frame diff) */ }
pub struct Tracker { .. } // new(cfg); process(&mut self, gray: &[u8], w: u32, h: u32, t: Seconds) -> FrameAnalysis
pub struct FrameAnalysis { pub motion_energy: f32, pub blobs: Vec<Blob {id,x,y,w,h,brightness}> /* normalized 0..1 coords */, pub entered: Vec<Blob>, pub exited: Vec<Blob /* with travel distance, duration, mean speed */>, pub crossings: Vec<Blob> /* crossed the configured line */ }
pub mod mjpeg { pub struct FrameStream (parses multipart/x-mixed-replace from a bytes Stream or AsyncRead → JPEG frames as Bytes) }
pub fn decode_gray(jpeg: &[u8], downscale: u32) -> Result<(Vec<u8>, u32, u32)>;
pub fn factory() -> (SourceInfo, Factory)  // kind "camera.mjpeg": url (http(s)://… or "-" for stdin), threshold, line (0..1 y of crossing line), min_blob, downscale, fps_cap. Events: `blob{x,y,size,brightness,speed,lane}` on exit/crossing; readings: motion, blobs, mean_speed. Viz payload: {blobs:[{x,y,w,h,b}]}.
```

## sonder-engine

```rust
pub struct EngineOptions { pub backend: AudioBackendChoice /*Auto|Native|Null|PcmStdout*/, pub sample_rate: u32, pub seed: Option<u64>, pub web_tap: bool, pub source_overrides: Vec<(String /*layer*/, SourceSpec)> }
pub struct EngineHandle { pub snapshot: tokio::sync::watch::Receiver<Arc<EngineSnapshot>>, pub commands: tokio::sync::mpsc::Sender<UserCommand>, pub audio_tap: Option<AudioTap>, pub sample_rate: u32, pub done: tokio::sync::oneshot::Receiver<()> }
pub async fn start(scene: Scene, opts: EngineOptions) -> anyhow::Result<EngineHandle>;
pub fn render_to_wav(scene: &Scene, seconds: f64, seed: u64, path: &Path) -> anyhow::Result<RenderStats>;   // offline, deterministic
pub fn load_scene(path_or_name: &str) -> anyhow::Result<Scene>;   // built-in scenes embedded from ../../scenes/*.toml via include_str!, else path
pub fn builtin_scenes() -> Vec<(&'static str, Scene)>;
pub fn validate(scene: &Scene) -> Result<(), Vec<String>>;   // structure + expr compile + source kinds + instrument names
```

## sonder-web / sonder-tui

```rust
sonder_web::serve(addr: SocketAddr, handle: WebHandle { snapshot, commands, audio_tap: Option<AudioTap>, sample_rate }) -> anyhow::Result<()>   // GET / (embedded index.html, vanilla JS), GET /api/snapshot, WS /ws (JSON snapshots at 20 Hz; client → UserCommand JSON), WS /audio (binary: i16 interleaved stereo frames, first message JSON {sample_rate, channels})
sonder_tui::run(snapshot, commands) -> anyhow::Result<()>   // returns on `q`
```
