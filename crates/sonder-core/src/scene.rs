//! Scene schema: the declarative TOML a user writes.
//!
//! ```toml
//! name = "Night Freeway"
//! description = "Headlights over an overpass at 3am."
//!
//! [conductor]
//! key = "D"
//! mode = "lydian"
//! tempo = 62
//! drift = "slow"
//!
//! [[layer]]
//! name = "traffic"
//! role = "pad"
//! instrument = "pad.glass"
//! source = { kind = "synthetic.freeway", lanes = 4, flow = 0.4 }
//! signals.density = "norm(smooth(rate(event('car'), 10), 3))"
//! map.energy = "density"
//!
//! [[layer]]
//! name = "headlights"
//! role = "pluck"
//! instrument = "pluck.nylon"
//! source = "traffic"                 # share the layer above's source
//! on_event.car.pitch = "attr('lane') / 4"
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::instrument::{Effects, Instrument};
use crate::music::Role;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Scene {
    pub name: String,
    pub description: String,
    /// Free-form tags for `sonder list` ("ambient", "live", "offline").
    pub tags: Vec<String>,
    pub conductor: ConductorConfig,
    pub effects: Effects,
    /// Inline instrument definitions, referenced by `layer.instrument`.
    pub instruments: BTreeMap<String, Instrument>,
    #[serde(rename = "layer")]
    pub layers: Vec<LayerConfig>,
    /// Scene-specific visualization hint for the UI ("freeway", "map", "generic").
    pub viz: String,
}

/// How eagerly the conductor wanders harmonically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Drift {
    /// Never changes key/mode; chords still move.
    Static,
    #[default]
    Slow,
    Medium,
    Fast,
}

/// Chord-progression policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Progression {
    /// Tonal walks with cadence weights (I–vi–IV–V style).
    Functional,
    /// Change one scale note at a time; brightness steers Lydian↔Locrian.
    #[default]
    Modal,
    /// Stay on one chord; only mood changes voicing.
    Static,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConductorConfig {
    /// Tonic, e.g. "D", "Eb", "F#".
    pub key: String,
    /// Mode / scale name as understood by `sonder-theory`:
    /// ionian/major, dorian, phrygian, lydian, mixolydian, aeolian/minor, locrian,
    /// major_pentatonic, minor_pentatonic, whole_tone, harmonic_minor, melodic_minor,
    /// blues, hirajoshi, in_sen, chromatic.
    pub mode: String,
    pub tempo: f64,
    /// Allowed tempo range for energy-driven nudges.
    pub tempo_range: [f64; 2],
    pub beats_per_bar: u8,
    /// Swing 0..1 (0 = straight).
    pub swing: f64,
    pub drift: Drift,
    pub progression: Progression,
    /// Bars per chord (float allowed, e.g. 2.0).
    pub bars_per_chord: f64,
    /// How quickly the mood vector follows its inputs, in seconds (big = glacial).
    pub mood_smoothing: f64,
    /// Baseline mood used when no layer publishes a value: [energy, tension, brightness].
    pub mood_baseline: [f64; 3],
    /// Humanization 0..1 (timing/velocity jitter).
    pub humanize: f64,
    /// RNG seed for deterministic renders. None = random per run.
    pub seed: Option<u64>,
    /// Musical form: how the piece is shaped over minutes.
    pub form: FormConfig,
}

/// How the piece is shaped over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FormStyle {
    /// No sections: a steady state shaped only by the data.
    Static,
    /// Slow tension arcs (intro → build → peak → release), lengths from `arc_minutes`,
    /// steered by energy. The default for ambient scenes.
    #[default]
    Arc,
    /// Alternating sections of `section_bars` (A/B/A'/B'…) with motifs that recur.
    Sections,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormConfig {
    pub style: FormStyle,
    /// Typical arc length in minutes (Arc style).
    pub arc_minutes: f64,
    /// Bars per section (Sections style).
    pub section_bars: u32,
    /// 0..1 how strongly form overrides data (0 = data only, 1 = form only).
    pub strength: f64,
    /// Let melodic layers answer salient events on other layers (call-and-response).
    pub call_and_response: bool,
    /// Motif memory: 0..1 how often melodic generators restate remembered motifs.
    pub motif_recall: f64,
}

impl Default for FormConfig {
    fn default() -> Self {
        Self { style: FormStyle::Arc, arc_minutes: 6.0, section_bars: 16, strength: 0.5, call_and_response: true, motif_recall: 0.4 }
    }
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            key: "D".into(),
            mode: "lydian".into(),
            tempo: 70.0,
            tempo_range: [55.0, 95.0],
            beats_per_bar: 4,
            swing: 0.0,
            drift: Drift::Slow,
            progression: Progression::Modal,
            bars_per_chord: 2.0,
            mood_smoothing: 20.0,
            mood_baseline: [0.4, 0.3, 0.5],
            humanize: 0.15,
            seed: None,
            form: FormConfig::default(),
        }
    }
}

/// Where a layer gets its data: an inline source spec, or the name of another
/// layer whose source it shares (so one feed can drive several roles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LayerSource {
    Shared(String),
    Spec(SourceSpec),
    /// No data; the generator runs on the conductor alone.
    None(NoSource),
}

impl Default for LayerSource {
    fn default() -> Self {
        LayerSource::None(NoSource {})
    }
}

/// Serializes as `{}` — a layer with no source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NoSource {}

/// `{ kind = "usgs.earthquakes", min_magnitude = 2.5 }` — `kind` picks the
/// adapter in `sonder-sources`; every other key is passed to it as a parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub kind: String,
    #[serde(flatten)]
    pub params: BTreeMap<String, serde_json::Value>,
}

impl SourceSpec {
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into(), params: BTreeMap::new() }
    }
    pub fn with(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.params.get(key).and_then(|v| v.as_f64())
    }
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(|v| v.as_str())
    }
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.params.get(key).and_then(|v| v.as_bool())
    }
    pub fn get_str_list(&self, key: &str) -> Option<Vec<String>> {
        match self.params.get(key)? {
            serde_json::Value::String(s) => Some(vec![s.clone()]),
            serde_json::Value::Array(a) => Some(a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
            _ => None,
        }
    }
}

/// One `on_event.<name>` table. All string values are expressions unless noted
/// in [`crate::music::event_keys`].
pub type EventMapping = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerConfig {
    pub name: String,
    pub role: Role,
    /// Instrument preset name (built-in or from `[instruments]`). Empty = role default.
    pub instrument: String,
    pub source: LayerSource,
    /// Named signal expressions over the source: `density = "norm(rate(event('car'), 10))"`.
    pub signals: BTreeMap<String, String>,
    /// Musical parameter ← expression (may reference `signals` by name).
    /// Keys documented in [`crate::music::map_keys`].
    pub map: BTreeMap<String, String>,
    /// Per-event-name trigger mappings. Key `"*"` matches any event.
    pub on_event: BTreeMap<String, EventMapping>,
    /// Starts muted.
    pub muted: bool,
    /// Weight of this layer's mood contributions (energy/tension/brightness), default 1.
    pub mood_weight: f64,
    /// Static gain multiplier 0..1 (in addition to `map.volume`).
    pub gain: f32,
    /// Caption template for this layer's events, e.g. `"{size} {product} {side} on Kraken"`
    /// (`{attr}` placeholders; `{intensity}`, `{name}` available). Overrides the source's.
    pub caption: String,
}

impl Default for LayerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            role: Role::Pad,
            instrument: String::new(),
            source: LayerSource::default(),
            signals: BTreeMap::new(),
            map: BTreeMap::new(),
            on_event: BTreeMap::new(),
            muted: false,
            mood_weight: 1.0,
            gain: 1.0,
            caption: String::new(),
        }
    }
}

impl Scene {
    /// Structural validation that doesn't need the expression parser or the
    /// source registry: names unique & non-empty, shared sources resolve, map
    /// keys recognized, instruments referenced exist (if inline).
    pub fn validate_structure(&self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let mut names = std::collections::HashSet::new();
        for (i, l) in self.layers.iter().enumerate() {
            let ctx = if l.name.is_empty() { format!("layer #{i}") } else { format!("layer `{}`", l.name) };
            if l.name.is_empty() {
                errs.push(format!("{ctx}: name is required"));
            } else if !names.insert(l.name.as_str()) {
                errs.push(format!("{ctx}: duplicate layer name"));
            }
            if let LayerSource::Shared(target) = &l.source {
                match self.layers.iter().find(|o| &o.name == target) {
                    None => errs.push(format!("{ctx}: shares source of unknown layer `{target}`")),
                    Some(o) if matches!(o.source, LayerSource::Shared(_)) => {
                        errs.push(format!("{ctx}: `{target}` is itself a shared reference; point at the layer with the spec"))
                    }
                    Some(o) if matches!(o.source, LayerSource::None(_)) => {
                        errs.push(format!("{ctx}: `{target}` has no source to share"))
                    }
                    _ => {}
                }
            }
            for k in l.map.keys() {
                if !crate::music::map_keys::ALL.contains(&k.as_str()) {
                    errs.push(format!("{ctx}: unknown map key `{k}` (known: {})", crate::music::map_keys::ALL.join(", ")));
                }
            }
            for (ev, m) in &l.on_event {
                for k in m.keys() {
                    if !crate::music::event_keys::ALL.contains(&k.as_str()) {
                        errs.push(format!("{ctx}: on_event.{ev}: unknown key `{k}`"));
                    }
                }
            }
            if !l.gain.is_finite() || !(0.0..=4.0).contains(&l.gain) {
                errs.push(format!("{ctx}: gain {} out of range 0..4", l.gain));
            }
            if !l.mood_weight.is_finite() || l.mood_weight < 0.0 {
                errs.push(format!("{ctx}: mood_weight {} must be finite and ≥ 0", l.mood_weight));
            }
        }
        if crate::music::parse_pitch_class(&self.conductor.key).is_none() {
            errs.push(format!("conductor.key `{}` is not a pitch class", self.conductor.key));
        }
        let c = &self.conductor;
        if !(c.tempo.is_finite() && c.tempo > 0.0) {
            errs.push("conductor.tempo must be positive".into());
        }
        let [lo, hi] = c.tempo_range;
        if !(lo.is_finite() && hi.is_finite() && lo > 0.0 && hi >= lo) {
            errs.push(format!("conductor.tempo_range {:?} must be finite with 0 < lo ≤ hi", c.tempo_range));
        }
        if c.mood_baseline.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v)) {
            errs.push(format!("conductor.mood_baseline {:?} must be within 0..1", c.mood_baseline));
        }
        for (name, v) in [("swing", c.swing), ("humanize", c.humanize)] {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                errs.push(format!("conductor.{name} {v} must be within 0..1"));
            }
        }
        if !(c.bars_per_chord.is_finite() && c.bars_per_chord > 0.0) {
            errs.push("conductor.bars_per_chord must be positive".into());
        }
        if !(c.mood_smoothing.is_finite() && c.mood_smoothing >= 0.0) {
            errs.push("conductor.mood_smoothing must be finite and ≥ 0".into());
        }
        if self.conductor.beats_per_bar == 0 {
            errs.push("conductor.beats_per_bar must be ≥ 1".into());
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// Resolve which layer owns the source a layer reads from.
    pub fn source_owner<'a>(&'a self, layer: &'a LayerConfig) -> Option<&'a LayerConfig> {
        match &layer.source {
            LayerSource::Spec(_) => Some(layer),
            LayerSource::Shared(name) => self.layers.iter().find(|l| &l.name == name),
            LayerSource::None(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_scene_from_json() {
        // JSON stands in for TOML here (core doesn't depend on toml); the data model is the same.
        let j = serde_json::json!({
            "name": "t",
            "conductor": { "key": "D", "mode": "lydian", "tempo": 62 },
            "layer": [
                { "name": "traffic", "role": "pad", "instrument": "pad.glass",
                  "source": { "kind": "synthetic.freeway", "lanes": 4, "flow": 0.4 },
                  "signals": { "density": "norm(rate(event('car'), 10))" },
                  "map": { "energy": "density" } },
                { "name": "headlights", "role": "pluck", "source": "traffic",
                  "on_event": { "car": { "pitch": "attr('lane') / 4" } } },
                { "name": "free", "role": "drone" }
            ]
        });
        let s: Scene = serde_json::from_value(j).unwrap();
        assert_eq!(s.layers.len(), 3);
        match &s.layers[0].source {
            LayerSource::Spec(spec) => {
                assert_eq!(spec.kind, "synthetic.freeway");
                assert_eq!(spec.get_f64("lanes"), Some(4.0));
            }
            _ => panic!(),
        }
        assert!(matches!(s.layers[1].source, LayerSource::Shared(_)));
        assert!(matches!(s.layers[2].source, LayerSource::None(_)));
        s.validate_structure().unwrap();
        assert_eq!(s.source_owner(&s.layers[1]).unwrap().name, "traffic");
    }

    #[test]
    fn validation_catches_bad_refs() {
        let mut s = Scene::default();
        s.layers.push(LayerConfig { name: "a".into(), source: LayerSource::Shared("zzz".into()), ..Default::default() });
        s.layers.push(LayerConfig { name: "a".into(), ..Default::default() });
        let errs = s.validate_structure().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unknown layer")));
        assert!(errs.iter().any(|e| e.contains("duplicate")));
    }

    #[test]
    fn validation_rejects_non_finite_scalars() {
        let mut s = Scene::default();
        s.conductor.tempo = f64::NAN;
        s.conductor.mood_baseline = [0.5, f64::INFINITY, 0.5];
        s.conductor.tempo_range = [100.0, 50.0];
        s.layers.push(LayerConfig { name: "a".into(), gain: f32::NAN, mood_weight: -1.0, ..Default::default() });
        let errs = s.validate_structure().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("tempo must be positive")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("mood_baseline")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("tempo_range")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("gain")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("mood_weight")), "{errs:?}");
    }
}
