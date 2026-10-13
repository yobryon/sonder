//! Synth patch schema. Consumed by `sonder-dsp`, referenced by name from scenes,
//! and definable inline in a scene's `[instruments.<name>]` table.
//!
//! Every field has a sensible default so presets can be terse:
//!
//! ```toml
//! [instruments."pluck.glass"]
//! osc = [{ wave = "triangle" }, { wave = "sine", octave = 1, level = 0.3 }]
//! filter = { kind = "lp", cutoff = 2400, resonance = 0.2, env_amount = 0.6 }
//! amp_env = { attack = 0.002, decay = 0.4, sustain = 0.0, release = 0.6 }
//! reverb_send = 0.4
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Wave {
    #[default]
    Sine,
    Triangle,
    /// PolyBLEP anti-aliased sawtooth.
    Saw,
    /// PolyBLEP anti-aliased square.
    Square,
    /// Variable-width pulse (see [`Osc::pulse_width`]).
    Pulse,
    /// Seven detuned saws (see [`Osc::spread`]).
    Supersaw,
    /// Two-operator FM: carrier sine modulated by a sine at `fm_ratio` with `fm_index`.
    Fm,
    /// White noise.
    Noise,
    /// Pink (1/f) noise.
    Pink,
}

/// One oscillator in a voice. Up to two per instrument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Osc {
    pub wave: Wave,
    /// Linear level 0..1.
    pub level: f32,
    /// Octave offset (integer).
    pub octave: i8,
    /// Semitone offset.
    pub semitones: f32,
    /// Fine detune in cents.
    pub detune_cents: f32,
    /// Pulse width 0.05..0.95 for [`Wave::Pulse`].
    pub pulse_width: f32,
    /// Detune spread in cents for [`Wave::Supersaw`].
    pub spread: f32,
    /// Modulator/carrier ratio for [`Wave::Fm`].
    pub fm_ratio: f32,
    /// Modulation index for [`Wave::Fm`] (0 = pure sine).
    pub fm_index: f32,
    /// How much the amp envelope scales fm_index (bright attack → mellow tail).
    pub fm_index_env: f32,
}

impl Default for Osc {
    fn default() -> Self {
        Self {
            wave: Wave::Sine,
            level: 1.0,
            octave: 0,
            semitones: 0.0,
            detune_cents: 0.0,
            pulse_width: 0.5,
            spread: 12.0,
            fm_ratio: 2.0,
            fm_index: 1.0,
            fm_index_env: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilterKind {
    Off,
    #[default]
    Lp,
    Hp,
    Bp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub kind: FilterKind,
    /// Base cutoff in Hz.
    pub cutoff: f32,
    /// 0..1 resonance (mapped to Q by the synth).
    pub resonance: f32,
    /// Filter envelope depth in octaves (can be negative).
    pub env_amount: f32,
    /// 0..1: how much the cutoff follows note pitch.
    pub key_track: f32,
    /// LFO depth in octaves.
    pub lfo_amount: f32,
    /// Velocity → cutoff in octaves.
    pub velocity_amount: f32,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            kind: FilterKind::Lp,
            cutoff: 8000.0,
            resonance: 0.1,
            env_amount: 0.0,
            key_track: 0.0,
            lfo_amount: 0.0,
            velocity_amount: 0.0,
        }
    }
}

/// ADSR envelope, times in seconds, sustain 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adsr {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Default for Adsr {
    fn default() -> Self {
        Self { attack: 0.01, decay: 0.2, sustain: 0.8, release: 0.3 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    Saw,
    Square,
    /// Sample-and-hold random.
    Random,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lfo {
    pub shape: LfoShape,
    pub rate_hz: f32,
    /// Depth in cents.
    pub to_pitch: f32,
    /// Depth 0..1 (tremolo).
    pub to_amp: f32,
    /// Depth 0..1 (auto-pan).
    pub to_pan: f32,
    /// Depth 0..1 for pulse width.
    pub to_pulse_width: f32,
    /// Retrigger LFO phase on each note.
    pub retrigger: bool,
}

impl Default for Lfo {
    fn default() -> Self {
        Self {
            shape: LfoShape::Sine,
            rate_hz: 0.5,
            to_pitch: 0.0,
            to_amp: 0.0,
            to_pan: 0.0,
            to_pulse_width: 0.0,
            retrigger: false,
        }
    }
}

/// A complete synth patch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Instrument {
    /// One or two oscillators. Empty = one default sine.
    pub osc: Vec<Osc>,
    pub filter: Filter,
    pub amp_env: Adsr,
    pub filter_env: Adsr,
    pub lfo: Lfo,
    /// Portamento time in seconds (0 = off). Only meaningful for mono-ish use.
    pub glide: f32,
    /// Output gain, linear.
    pub gain: f32,
    /// Default pan 0..1 (0.5 = center). Note-level pan is added as an offset.
    pub pan: f32,
    /// Stereo width 0..1 — spreads oscillators / unison across the field.
    pub width: f32,
    pub reverb_send: f32,
    pub delay_send: f32,
    /// Max simultaneous voices for this instrument.
    pub polyphony: u8,
    /// 0..1 how much velocity scales amplitude (0 = fixed level).
    pub velocity_to_amp: f32,
    /// Soft saturation drive 0..1 applied post-filter.
    pub drive: f32,
    /// Optional hint for the UI.
    pub description: String,
}

impl Default for Instrument {
    fn default() -> Self {
        Self {
            osc: vec![Osc::default()],
            filter: Filter::default(),
            amp_env: Adsr::default(),
            filter_env: Adsr { attack: 0.01, decay: 0.3, sustain: 0.0, release: 0.3 },
            lfo: Lfo::default(),
            glide: 0.0,
            gain: 0.8,
            pan: 0.5,
            width: 0.0,
            reverb_send: 0.2,
            delay_send: 0.0,
            polyphony: 8,
            velocity_to_amp: 0.7,
            drive: 0.0,
            description: String::new(),
        }
    }
}

/// Percussion sounds are synthesized, not sampled. The composer picks one of
/// these per hit; `sonder-dsp` owns the recipes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Drum {
    Kick,
    Snare,
    Hat,
    /// Short filtered click / tick.
    Click,
    /// Noise burst, length from velocity.
    Noise,
    /// Pitched tom-like body.
    Tom,
    /// Metallic rimshot-ish.
    Rim,
}

impl Drum {
    pub fn parse(s: &str) -> Option<Drum> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "kick" => Drum::Kick,
            "snare" => Drum::Snare,
            "hat" | "hihat" => Drum::Hat,
            "click" | "tick" => Drum::Click,
            "noise" => Drum::Noise,
            "tom" => Drum::Tom,
            "rim" => Drum::Rim,
            _ => return None,
        })
    }
}

/// Settings for the shared effect buses and master chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effects {
    pub reverb: ReverbSettings,
    pub delay: DelaySettings,
    pub master: MasterSettings,
}

impl Default for Effects {
    fn default() -> Self {
        Self { reverb: ReverbSettings::default(), delay: DelaySettings::default(), master: MasterSettings::default() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverbSettings {
    /// 0..1 room size / decay.
    pub size: f32,
    /// 0..1 high-frequency damping.
    pub damping: f32,
    /// Pre-delay in ms.
    pub predelay_ms: f32,
    /// Return level 0..1.
    pub level: f32,
    /// Low-cut on the wet signal, Hz.
    pub lowcut_hz: f32,
}

impl Default for ReverbSettings {
    fn default() -> Self {
        Self { size: 0.8, damping: 0.4, predelay_ms: 20.0, level: 0.8, lowcut_hz: 150.0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DelaySettings {
    /// Delay time in beats (tempo-synced). E.g. 0.75 = dotted eighth.
    pub beats: f32,
    pub feedback: f32,
    /// Ping-pong between channels.
    pub ping_pong: bool,
    /// Low-pass on the feedback path, Hz.
    pub damp_hz: f32,
    pub level: f32,
}

impl Default for DelaySettings {
    fn default() -> Self {
        Self { beats: 0.75, feedback: 0.35, ping_pong: true, damp_hz: 4000.0, level: 0.7 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterSettings {
    pub gain: f32,
    /// Soft saturation 0..1.
    pub saturation: f32,
    /// Limiter ceiling in dBFS (negative).
    pub ceiling_db: f32,
}

impl Default for MasterSettings {
    fn default() -> Self {
        Self { gain: 0.9, saturation: 0.1, ceiling_db: -1.0 }
    }
}
