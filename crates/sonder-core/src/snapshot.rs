//! A serializable picture of the whole engine for UIs. Published at ~20 Hz via
//! `tokio::sync::watch` (or equivalent) by `sonder-engine`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::command::Meter;
use crate::music::{LayerId, Role};
use crate::sample::{SourceState, Value};
use crate::time::{Seconds, TransportPos};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HarmonySnapshot {
    /// Tonic pitch class 0..11.
    pub tonic: u8,
    /// "D"
    pub key_name: String,
    /// "lydian"
    pub mode: String,
    /// Scale pitch classes 0..11, ascending from tonic.
    pub scale: Vec<u8>,
    /// Current chord as pitch classes.
    pub chord: Vec<u8>,
    /// Human name, e.g. "Dmaj7" or "ii".
    pub chord_name: String,
    /// Roman-numeral-ish degree (1-based) of the chord root.
    pub chord_degree: u8,
    /// Beats until the next planned chord change.
    pub beats_to_change: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Mood {
    pub energy: f64,
    pub tension: f64,
    pub brightness: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LayerSnapshot {
    pub id: LayerId,
    pub name: String,
    pub role: Role,
    pub instrument: String,
    pub muted: bool,
    pub soloed: bool,
    /// Current value of each named signal (from `signals`), 0..1-ish.
    pub signals: BTreeMap<String, f64>,
    /// Current resolved musical parameters (from `map`).
    pub params: BTreeMap<String, f64>,
    pub meter: Meter,
    pub active_notes: u16,
    /// Source this layer reads from (name), if any.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub name: String,
    pub kind: String,
    pub state: SourceState,
    pub message: String,
    /// Samples received in the last second.
    pub rate: f64,
    pub last_sample_at: Option<Seconds>,
    /// Latest reading values, for the UI.
    pub readings: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventSnapshot {
    pub at: Seconds,
    pub layer: LayerId,
    pub name: String,
    pub intensity: f64,
    /// A few attributes worth showing (sources decide what is interesting).
    pub attrs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteSnapshot {
    pub at: Seconds,
    pub layer: LayerId,
    pub midi: f32,
    pub velocity: f32,
    pub pan: f32,
    /// Planned duration in seconds (None = until released).
    pub duration: Option<f64>,
    /// Is it still sounding?
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EngineSnapshot {
    pub scene_name: String,
    pub viz_kind: String,
    pub time: Seconds,
    pub paused: bool,
    pub transport: TransportPos,
    pub harmony: HarmonySnapshot,
    pub mood: Mood,
    /// Human explanation of what the conductor is doing and why.
    pub conductor_note: String,
    pub layers: Vec<LayerSnapshot>,
    pub sources: Vec<SourceSnapshot>,
    /// Most recent events, newest last (bounded, ~64).
    pub events: Vec<EventSnapshot>,
    /// Recent notes for a piano-roll, newest last (bounded, ~256).
    pub notes: Vec<NoteSnapshot>,
    pub master: Meter,
    /// Log-spaced magnitude bands (0..1), ~48 bins, for a spectrum display.
    pub spectrum: Vec<f32>,
    /// Latest `Datum::Viz` payload per source, keyed by source name.
    pub viz: BTreeMap<String, serde_json::Value>,
    pub underruns: u32,
    pub audio_backend: String,
}
