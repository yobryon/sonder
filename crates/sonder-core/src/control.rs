//! Commands from a UI (TUI, web, CLI) into the running engine.

use serde::{Deserialize, Serialize};

use crate::music::LayerId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UserCommand {
    ToggleMute { layer: LayerId },
    /// Solo a layer (or clear solo if already soloed).
    ToggleSolo { layer: LayerId },
    SetLayerGain { layer: LayerId, gain: f32 },
    /// Add to the tempo (bpm); clamped to the scene's range.
    NudgeTempo { delta: f64 },
    /// Global density offset in -1..1 added to every layer's density.
    NudgeDensity { delta: f64 },
    /// Move the tonic by semitones (+/-).
    Transpose { semitones: i32 },
    /// Advance to the next mode in brightness order (or previous).
    NextMode { backwards: bool },
    /// Force a chord change now.
    NextChord,
    TogglePause,
    /// Write the current (possibly tweaked) scene to disk at `path` (engine chooses if empty).
    SaveScene { path: String },
    /// Replace the running scene with this TOML (restart in place).
    LoadScene { toml: String },
    /// Jump to the next form section now.
    NextSection,
    /// Replay only: seek to this many seconds into the log.
    Seek { to: f64 },
    /// Replay only: playback speed multiplier (1 = real time, 0 = pause).
    SetSpeed { speed: f64 },
    /// A person plays a key (on-screen piano, computer keyboard, Web MIDI). The
    /// composer feeds it to every `Role::Human` layer as a `note` / `note_off`
    /// event; hardware MIDI goes through the `human.midi` source instead.
    HumanNote { key: u8, velocity: f32, on: bool },
    Quit,
}
