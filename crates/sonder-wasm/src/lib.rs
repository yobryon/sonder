//! Sonder in the browser.
//!
//! The same control loop and synth as the native engine, driven from JavaScript: a Web
//! Worker calls [`Engine::tick`] every 20 ms of engine time, feeds live data from JS
//! adapters through [`Engine::push_sample`] (or webcam frames through
//! [`Engine::push_frame`]), and ships the rendered audio and snapshots to the page.
//! The synthetic sources (`synthetic.freeway`, `synthetic.walk`, `clock.astro`) are
//! stepped here in Rust so the offline scenes work with no JS adapter at all.
//!
//! See `docs/WASM.md` for the JS-facing contract.
//!
//! Object detection (cargo feature `detect`, off by default): build with
//! `--features detect`, have JS fetch the ONNX model named by the camera's `model_url`
//! param, pass the bytes to [`Engine::load_model`], then push RGB frames with
//! [`Engine::push_frame_rgb`] instead of gray ones with [`Engine::push_frame`].

#![forbid(unsafe_code)]

mod sims;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use sonder_audio::{SynthController, SynthHost};
use sonder_core::*;
use sonder_engine::control::ControlLoop;
use sonder_engine::TICK_SECONDS;
use wasm_bindgen::prelude::*;

use sims::Sim;

/// Scenes the browser build offers: the native library minus kinds that cannot exist in a
/// browser, plus browser-only ones.
const BROWSER_ONLY_SCENES: &[(&str, &str)] = &[("this-browser", include_str!("../../../scenes/this-browser.toml"))];
const NATIVE_ONLY_SCENES: &[&str] = &["this-machine"];

#[derive(Serialize)]
struct SceneEntry {
    name: String,
    title: String,
    description: String,
    tags: Vec<String>,
    toml: String,
}

#[derive(Serialize)]
struct SourceSpecEntry {
    name: String,
    kind: String,
    params: BTreeMap<String, serde_json::Value>,
    builtin: bool,
}

/// Crate version string.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Built-in scenes as `[{name, title, description, tags, toml}]`.
#[wasm_bindgen]
pub fn builtin_scenes() -> Result<JsValue, JsError> {
    let mut out = Vec::new();
    for (name, src) in sonder_engine::scenes::BUILTIN_SCENE_SOURCES.iter().chain(BROWSER_ONLY_SCENES) {
        if NATIVE_ONLY_SCENES.contains(name) {
            continue;
        }
        if let Ok(scene) = sonder_engine::parse_scene(src) {
            out.push(SceneEntry {
                name: name.to_string(),
                title: scene.name.clone(),
                description: scene.description.clone(),
                tags: scene.tags.clone(),
                toml: src.to_string(),
            });
        }
    }
    to_js(&out)
}

/// Serialize as plain JS objects/arrays (serde-wasm-bindgen's default turns maps into
/// `Map`s, which JSON.stringify renders as `{}`).
fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsError> {
    let ser = serde_wasm_bindgen::Serializer::json_compatible();
    v.serialize(&ser).map_err(|e| JsError::new(&e.to_string()))
}

/// Validation errors for a scene TOML (empty when valid).
#[wasm_bindgen]
pub fn validate_scene(toml_src: &str) -> Vec<String> {
    match sonder_engine::parse_scene(toml_src) {
        Err(e) => vec![format!("{e:#}")],
        Ok(scene) => sonder_engine::validate(&scene).err().unwrap_or_default(),
    }
}

/// One running Sonder engine.
#[wasm_bindgen]
pub struct Engine {
    control: ControlLoop,
    host: SynthHost,
    clock: Arc<VirtualClock>,
    snapshot_rx: tokio::sync::watch::Receiver<Arc<EngineSnapshot>>,
    sims: Vec<Sim>,
    cameras: BTreeMap<String, sims::Camera>,
    frames_per_tick: usize,
    ticks: u64,
    specs: Vec<SourceSpecEntry>,
    /// True once a log is loaded: sims stop and JS samples are ignored.
    replaying: bool,
    /// The object detector shared by all camera sources (feature `detect`; see `load_model`).
    #[cfg(feature = "detect")]
    detector: Option<sonder_vision::Detector>,
}

/// What `Engine::load_log` returns (as a plain JS object).
#[derive(Serialize)]
struct ReplayInfo {
    title: String,
    scene: String,
    seed: u64,
    epoch_unix: f64,
    created: String,
    length: f64,
    /// Source names in the log.
    sources: Vec<String>,
    /// Scene sources the log feeds (empty = nothing in this scene listens to it).
    fed: Vec<String>,
    audio_dropped: Vec<String>,
}

#[wasm_bindgen]
impl Engine {
    /// Build an engine for a scene. Throws (as a JS error) when the scene is invalid.
    #[wasm_bindgen(constructor)]
    pub fn new(scene_toml: &str, seed: u64, sample_rate: f32) -> Result<Engine, JsError> {
        console_error_panic_hook::set_once();
        let scene = sonder_engine::parse_scene(scene_toml).map_err(|e| JsError::new(&format!("{e:#}")))?;
        if let Err(errs) = sonder_engine::validate(&scene) {
            return Err(JsError::new(&format!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "))));
        }
        let sample_rate = if sample_rate.is_finite() && sample_rate >= 8000.0 { sample_rate } else { 48_000.0 };
        let seed = if seed == 0 { scene.conductor.seed.unwrap_or(1) } else { seed };
        let max_layers = scene.layers.len().max(1);
        let (controller, host): (SynthController, SynthHost) = sonder_audio::synth_pair(sample_rate, max_layers, None);
        let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
        let clock = Arc::new(VirtualClock::new());

        let mut sims = Vec::new();
        let mut cameras = BTreeMap::new();
        let mut specs = Vec::new();
        for (id, spec) in ControlLoop::source_specs(&scene) {
            let builtin = match Sim::new(&id, &spec, seed) {
                Ok(Some(sim)) => {
                    sims.push(sim);
                    true
                }
                Ok(None) => false,
                Err(e) => return Err(JsError::new(&format!("source `{id}`: {e}"))),
            };
            if spec.kind.starts_with("camera.") {
                cameras.insert(id.to_string(), sims::Camera::new(&id, &spec));
            }
            specs.push(SourceSpecEntry { name: id.to_string(), kind: spec.kind.clone(), params: spec.params.clone(), builtin });
        }

        let control = ControlLoop::new(scene, seed, controller, None, None, "browser".into(), snapshot_tx)
            .map_err(|e| JsError::new(&format!("{e:#}")))?;
        let frames_per_tick = (sample_rate as f64 * TICK_SECONDS).round() as usize;
        Ok(Engine {
            control,
            host,
            clock,
            snapshot_rx,
            sims,
            cameras,
            frames_per_tick,
            ticks: 0,
            specs,
            replaying: false,
            #[cfg(feature = "detect")]
            detector: None,
        })
    }

    /// Load an object-detection model (YOLOv8/11-format ONNX bytes; JS fetches them from the
    /// camera source's `model_url` param, see [`Engine::source_specs`]). `input` is the model
    /// input size in pixels (0 = the first camera's `model_input`, default 320). Afterwards
    /// feed RGB frames with [`Engine::push_frame_rgb`]. Only in builds with the `detect`
    /// feature.
    #[cfg(feature = "detect")]
    pub fn load_model(&mut self, bytes: &[u8], input: u32) -> Result<(), JsError> {
        let mut cfg = self
            .cameras
            .values()
            .find(|c| c.detect.enabled)
            .or_else(|| self.cameras.values().next())
            .map(|c| c.detect.detector_config())
            .unwrap_or_default();
        if input > 0 {
            cfg.input = input;
        }
        let det = sonder_vision::Detector::from_bytes(bytes, cfg).map_err(|e| JsError::new(&format!("cannot load model: {e}")))?;
        self.detector = Some(det);
        Ok(())
    }

    /// Whether a detection model is loaded.
    #[cfg(feature = "detect")]
    pub fn has_model(&self) -> bool {
        self.detector.is_some()
    }

    /// Feed an interleaved 8-bit RGB frame to a camera source whose spec has `detect = true`:
    /// runs the detector and the tracker-by-class, emitting `blob` / `crossing` events with
    /// `class`, per-class count readings and viz with class names. Requires
    /// [`Engine::load_model`] first. Only in builds with the `detect` feature.
    #[cfg(feature = "detect")]
    pub fn push_frame_rgb(&mut self, source: &str, rgb: &[u8], width: u32, height: u32) -> Result<(), JsError> {
        if self.replaying {
            return Ok(());
        }
        let Some(cam) = self.cameras.get_mut(source) else {
            return Err(JsError::new(&format!("`{source}` is not a camera source in this scene")));
        };
        if rgb.len() != (width as usize) * (height as usize) * 3 || width == 0 || height == 0 {
            return Err(JsError::new("frame size does not match width × height × 3"));
        }
        let Some(det) = self.detector.as_mut() else {
            return Err(JsError::new("no detection model loaded (call load_model first)"));
        };
        let now = self.clock.now();
        let samples = cam.process_rgb(det, rgb, width, height, now).map_err(|e| JsError::new(&e))?;
        for s in samples {
            self.control.ingest(s);
        }
        Ok(())
    }

    /// Switch to replay mode: play this `.sonder` log (plain JSONL bytes — inflate gzip in
    /// JS first) into the current scene under the log's source names. The sims stop and
    /// `push_sample` / `push_frame` are ignored from now on. Returns `ReplayInfo`.
    pub fn load_log(&mut self, bytes: &[u8], title: &str) -> Result<JsValue, JsError> {
        let log = sonder_engine::datalog::load_bytes(bytes).map_err(|e| JsError::new(&e.to_string()))?;
        let h = log.header().clone();
        let sources = log.source_names();
        let audio_dropped = log.audio_dropped().iter().map(|s| s.to_string()).collect();
        let replay = sonder_engine::replay::Replay::new(log.clone(), title, sonder_engine::replay::ReplayConfig::default());
        self.control.set_replay(Some(replay));
        self.replaying = true;
        self.sims.clear();
        let fed = self.control.replay_fed_sources();
        to_js(&ReplayInfo { title: title.to_string(), scene: h.scene, seed: h.seed, epoch_unix: h.epoch_unix, created: h.created, length: log.length_seconds(), sources, fed, audio_dropped })
    }

    /// Replay only: jump to `to` seconds into the log.
    pub fn seek(&mut self, to: f64) {
        self.control.command(UserCommand::Seek { to });
    }

    /// Replay only: playback speed (0 pauses the log).
    pub fn set_speed(&mut self, speed: f64) {
        self.control.command(UserCommand::SetSpeed { speed });
    }

    /// Is a log playing?
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// Sources the scene needs: `[{name, kind, params, builtin}]`. `builtin` sources are
    /// stepped by Rust; the rest must be fed via `push_sample`.
    pub fn source_specs(&self) -> Result<JsValue, JsError> {
        to_js(&self.specs)
    }

    /// Interleaved stereo frames rendered per tick (`sample_rate / 50`).
    pub fn frames_per_tick(&self) -> usize {
        self.frames_per_tick
    }

    /// Engine time in seconds.
    pub fn time(&self) -> f64 {
        self.clock.now()
    }

    /// Feed one sample from a JS adapter. `json` is a `Datum` object with a `type` tag:
    /// `reading` / `event` / `status` / `viz` (see `docs/WASM.md`).
    pub fn push_sample(&mut self, source: &str, json: &str) -> Result<(), JsError> {
        if self.replaying {
            return Ok(());
        }
        let datum: Datum = serde_json::from_str(json).map_err(|e| JsError::new(&format!("bad sample for `{source}`: {e}")))?;
        let sample = Sample { source: SourceId::new(source), at: self.clock.now(), datum };
        self.control.ingest(sample);
        Ok(())
    }

    /// Feed an 8-bit grayscale webcam frame to a camera source; runs the tracker and emits
    /// the same events/readings/viz the native camera source does.
    pub fn push_frame(&mut self, source: &str, gray: &[u8], width: u32, height: u32) -> Result<(), JsError> {
        if self.replaying {
            return Ok(());
        }
        let Some(cam) = self.cameras.get_mut(source) else {
            return Err(JsError::new(&format!("`{source}` is not a camera source in this scene")));
        };
        if gray.len() != (width as usize) * (height as usize) || width == 0 || height == 0 {
            return Err(JsError::new("frame size does not match width × height"));
        }
        let now = self.clock.now();
        for s in cam.process(gray, width, height, now) {
            self.control.ingest(s);
        }
        Ok(())
    }

    /// Advance one control tick (20 ms) and render its audio into `out`
    /// (length must be `2 * frames_per_tick()`).
    pub fn tick(&mut self, out: &mut [f32]) -> Result<(), JsError> {
        if out.len() != self.frames_per_tick * 2 {
            return Err(JsError::new(&format!("out must hold {} floats", self.frames_per_tick * 2)));
        }
        self.ticks += 1;
        let t = self.ticks as f64 * TICK_SECONDS;
        self.clock.set(t);
        for sim in &mut self.sims {
            for s in sim.advance_to(t) {
                self.control.ingest(s);
            }
        }
        self.control.tick(t);
        self.host.render(out);
        Ok(())
    }

    /// The latest snapshot as JSON (same shape the native web UI receives).
    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(&**self.snapshot_rx.borrow()).unwrap_or_else(|_| "{}".into())
    }

    /// Apply a `UserCommand` (JSON, e.g. `{"cmd":"toggle_mute","layer":0}`).
    pub fn command(&mut self, json: &str) -> Result<(), JsError> {
        let cmd: UserCommand = serde_json::from_str(json).map_err(|e| JsError::new(&format!("bad command: {e}")))?;
        self.control.command(cmd);
        Ok(())
    }
}
