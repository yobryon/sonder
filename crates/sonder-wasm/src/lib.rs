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
    serde_wasm_bindgen::to_value(&out).map_err(|e| JsError::new(&e.to_string()))
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
        Ok(Engine { control, host, clock, snapshot_rx, sims, cameras, frames_per_tick, ticks: 0, specs })
    }

    /// Sources the scene needs: `[{name, kind, params, builtin}]`. `builtin` sources are
    /// stepped by Rust; the rest must be fed via `push_sample`.
    pub fn source_specs(&self) -> Result<JsValue, JsError> {
        serde_wasm_bindgen::to_value(&self.specs).map_err(|e| JsError::new(&e.to_string()))
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
        let datum: Datum = serde_json::from_str(json).map_err(|e| JsError::new(&format!("bad sample for `{source}`: {e}")))?;
        let sample = Sample { source: SourceId::new(source), at: self.clock.now(), datum };
        self.control.ingest(sample);
        Ok(())
    }

    /// Feed an 8-bit grayscale webcam frame to a camera source; runs the tracker and emits
    /// the same events/readings/viz the native camera source does.
    pub fn push_frame(&mut self, source: &str, gray: &[u8], width: u32, height: u32) -> Result<(), JsError> {
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
