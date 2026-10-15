//! Commands from the composer (control thread) to the synth (audio thread).
//!
//! These cross a lock-free SPSC ring buffer, so they must be `Send`, cheap to
//! move, and must not allocate on the receiving side. `Box<Instrument>` is
//! allocated on the control side and dropped on the control side (the audio
//! thread sends it back via [`SynthFeedback::Recycle`]) — or simply leaked to
//! a background drop queue; the DSP crate decides, but must not free it on
//! the audio thread.

use serde::{Deserialize, Serialize};

use crate::instrument::{Drum, Effects, Instrument};
use crate::music::{LayerId, NoteId};

/// Parameters a layer exposes to the composer at control rate. Values 0..1
/// unless noted. The synth smooths these to avoid zipper noise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerParam {
    /// Linear gain multiplier.
    Gain(f32),
    /// 0 = left, 1 = right.
    Pan(f32),
    /// Filter cutoff *offset* in octaves relative to the instrument's cutoff.
    CutoffOctaves(f32),
    /// Overrides instrument resonance when set.
    Resonance(f32),
    ReverbSend(f32),
    DelaySend(f32),
    /// Generic "texture" macro: synth maps to detune/noise/drive per instrument.
    Texture(f32),
    /// Mute toggle (hard, with a short fade).
    Mute(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SynthCommand {
    /// Start a note. `pitch_hz` is already quantized by the composer.
    NoteOn {
        layer: LayerId,
        note: NoteId,
        pitch_hz: f32,
        /// 0..1
        velocity: f32,
        /// 0..1 offset around the instrument's pan (0.5 = no offset).
        pan: f32,
    },
    /// Release a note (enters its release stage).
    NoteOff { layer: LayerId, note: NoteId },
    /// Change pitch of a sounding note (glide handled by the synth).
    NotePitch { layer: LayerId, note: NoteId, pitch_hz: f32 },
    /// Trigger a percussion hit on a layer.
    Drum {
        layer: LayerId,
        drum: Drum,
        velocity: f32,
        pan: f32,
        /// 0..1 tone/pitch macro (e.g. hat brightness, kick tune).
        tone: f32,
    },
    /// Release every note on a layer.
    AllNotesOff { layer: LayerId },
    /// (Re)assign the instrument a layer plays. The synth lets existing notes
    /// finish on the old patch.
    SetInstrument { layer: LayerId, instrument: Box<Instrument> },
    SetLayerParam { layer: LayerId, param: LayerParam },
    /// Tempo for synced effects (delay times).
    SetTempo { bpm: f32 },
    SetEffects(Box<Effects>),
    /// Live audio for a layer's granular instrument (see `Wave::Grain`). The synth copies
    /// it into the layer's preallocated ring and returns the box via `take_recycled_audio`.
    AudioChunk { layer: LayerId, chunk: Box<AudioChunk> },
    /// Hard stop everything (voices + effect tails fade over ~50ms).
    Panic,
}

/// A chunk of mono audio for [`SynthCommand::AudioChunk`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AudioChunk {
    pub rate: u32,
    pub samples: Vec<f32>,
}

/// Messages from the audio thread back to the control side. Non-realtime
/// consumers read these; the audio thread never blocks on them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SynthFeedback {
    /// A note ended on its own (release finished or was stolen).
    NoteEnded { layer: LayerId, note: NoteId },
    /// Voice was stolen before ending.
    NoteStolen { layer: LayerId, note: NoteId },
    /// Count of buffer underruns observed since last report.
    Underruns(u32),
}

/// Per-layer and master metering, published by the synth after each block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Meter {
    pub rms: f32,
    pub peak: f32,
}
