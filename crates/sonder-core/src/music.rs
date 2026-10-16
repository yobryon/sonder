//! Musical identifiers and helpers shared by the composer and the synth.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Index of a layer within a scene (stable for the life of an engine run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct LayerId(pub u16);

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}", self.0)
    }
}

/// Unique id of a sounding note, allocated by the composer, used for NoteOff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NoteId(pub u64);

/// What a layer does in the arrangement. Each role is a generator in
/// `sonder-compose` with its own musical logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Sustains the current chord; voice-led across changes.
    #[default]
    Pad,
    /// Tonic pedal; texture controls noise/detune.
    Drone,
    /// Root/fifth on strong beats; density adds motion.
    Bass,
    /// Random-walk melody in scale.
    Lead,
    /// Arpeggiates the current chord.
    Arp,
    /// Event-triggered single notes (short, plucky).
    Pluck,
    /// Event-triggered single notes (long, ringing).
    Bells,
    /// Percussion: event-triggered hits and/or signal-driven patterns.
    Perc,
    /// Unpitched bed (noise, filtered) driven by signals.
    Texture,
    /// A person: plays `note` / `note_off` events from a human input source (MIDI,
    /// keyboard), snapped to the harmony.
    Human,
}

impl Role {
    pub const ALL: [Role; 10] = [
        Role::Pad,
        Role::Drone,
        Role::Bass,
        Role::Lead,
        Role::Arp,
        Role::Pluck,
        Role::Bells,
        Role::Perc,
        Role::Texture,
        Role::Human,
    ];
    /// Does this role play notes in response to events (vs. on its own clock)?
    pub fn is_event_driven(self) -> bool {
        matches!(self, Role::Pluck | Role::Bells | Role::Perc | Role::Human)
    }
    pub fn name(self) -> &'static str {
        match self {
            Role::Pad => "pad",
            Role::Drone => "drone",
            Role::Bass => "bass",
            Role::Lead => "lead",
            Role::Arp => "arp",
            Role::Pluck => "pluck",
            Role::Bells => "bells",
            Role::Perc => "perc",
            Role::Texture => "texture",
            Role::Human => "human",
        }
    }
}

/// Keys a layer's `map` table may use. Values are signal expressions in 0..1.
/// Kept as documented string constants (not an enum) so scenes stay forgiving
/// and generators can add role-specific knobs.
pub mod map_keys {
    /// Contribution to the conductor's global energy (tempo nudges, dynamics).
    pub const ENERGY: &str = "energy";
    /// Contribution to global tension (chord complexity, mode darkness).
    pub const TENSION: &str = "tension";
    /// Contribution to global brightness (mode brightness, filter opening).
    pub const BRIGHTNESS: &str = "brightness";
    /// How many notes this layer plays (probability / subdivision).
    pub const DENSITY: &str = "density";
    /// Octave window of this layer, 0 = low, 1 = high.
    pub const REGISTER: &str = "register";
    /// Melodic step size for walking generators.
    pub const STEP: &str = "step";
    /// Noise / detune / grain amount for texture-ish roles.
    pub const TEXTURE: &str = "texture";
    /// Layer output gain (0..1, multiplied by instrument gain).
    pub const VOLUME: &str = "volume";
    /// Stereo position 0 = left, 1 = right.
    pub const PAN: &str = "pan";
    /// Filter cutoff offset, 0..1 → mapped by the synth (overrides instrument default).
    pub const CUTOFF: &str = "cutoff";
    /// Filter resonance 0..1.
    pub const RESONANCE: &str = "resonance";
    /// Reverb send 0..1.
    pub const REVERB: &str = "reverb";
    /// Delay send 0..1.
    pub const DELAY: &str = "delay";
    /// All recognized keys, for validation.
    pub const ALL: &[&str] = &[
        ENERGY, TENSION, BRIGHTNESS, DENSITY, REGISTER, STEP, TEXTURE, VOLUME, PAN, CUTOFF, RESONANCE, REVERB,
        DELAY,
    ];
}

/// Keys an `on_event.<name>` table may use. Values are expressions evaluated
/// in the context of one event (`intensity`, `attr("k")` available).
pub mod event_keys {
    /// 0..1 → position within the layer's register window, quantized to chord/scale.
    pub const PITCH: &str = "pitch";
    /// 0..1 velocity.
    pub const VELOCITY: &str = "velocity";
    /// 0..1 pan.
    pub const PAN: &str = "pan";
    /// Note length in beats (float). Default per role.
    pub const LENGTH: &str = "length";
    /// Grid to quantize the trigger onto: "off", "1/4", "1/8", "1/16", "1/8t"… (string, not expr).
    pub const QUANTIZE: &str = "quantize";
    /// 0..1 probability the event produces a note (default 1).
    pub const CHANCE: &str = "chance";
    /// For perc: which drum. "kick" | "snare" | "hat" | "click" | "noise" (string, not expr).
    pub const DRUM: &str = "drum";
    /// Pitch quantization target: "chord" (default for pluck/bells) | "scale" | "none".
    pub const SNAP: &str = "snap";
    pub const ALL: &[&str] = &[PITCH, VELOCITY, PAN, LENGTH, QUANTIZE, CHANCE, DRUM, SNAP];
}

/// MIDI note number → frequency in Hz (A4 = 440 by default).
#[inline]
pub fn midi_to_hz(midi: f64) -> f64 {
    440.0 * 2f64.powf((midi - 69.0) / 12.0)
}

/// Frequency in Hz → MIDI note number (fractional).
#[inline]
pub fn hz_to_midi(hz: f64) -> f64 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// Human-readable note name for a MIDI number, e.g. 60 → "C4".
pub fn midi_name(midi: i32) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let pc = midi.rem_euclid(12) as usize;
    let oct = midi.div_euclid(12) - 1;
    format!("{}{}", NAMES[pc], oct)
}

/// Pitch class names accepted in scenes (`key = "D"`, `"Eb"`, `"F#"`).
pub fn parse_pitch_class(s: &str) -> Option<u8> {
    let s = s.trim();
    let mut chars = s.chars();
    let base = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut pc: i32 = base;
    for c in chars {
        match c {
            '#' | '♯' => pc += 1,
            'b' | '♭' => pc -= 1,
            _ => return None,
        }
    }
    Some(pc.rem_euclid(12) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pitch_math() {
        assert!((midi_to_hz(69.0) - 440.0).abs() < 1e-9);
        assert!((hz_to_midi(880.0) - 81.0).abs() < 1e-9);
        assert_eq!(midi_name(60), "C4");
        assert_eq!(midi_name(61), "C#4");
        assert_eq!(parse_pitch_class("Eb"), Some(3));
        assert_eq!(parse_pitch_class("f#"), Some(6));
        assert_eq!(parse_pitch_class("Cb"), Some(11));
        assert_eq!(parse_pitch_class("H"), None);
    }
}
