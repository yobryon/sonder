//! A tonic plus a mode.

use crate::mode::Mode;

/// Spell a pitch class with sharps or flats.
pub fn pitch_class_name(pc: u8, flats: bool) -> &'static str {
    const SHARPS: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    const FLATS: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];
    let i = (pc % 12) as usize;
    if flats { FLATS[i] } else { SHARPS[i] }
}

/// A concrete scale: a tonic pitch class and a [`Mode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scale {
    /// Tonic pitch class 0..=11.
    pub tonic: u8,
    pub mode: Mode,
}

impl Scale {
    pub fn new(tonic: u8, mode: Mode) -> Self {
        Self { tonic: tonic % 12, mode }
    }

    /// Number of degrees in the scale.
    pub fn len(&self) -> usize {
        self.mode.steps.len()
    }
    pub fn is_empty(&self) -> bool {
        self.mode.steps.is_empty()
    }

    /// Does this key conventionally spell with flats (F, Bb, Eb, Ab, Db, Gb)?
    pub fn prefers_flats(&self) -> bool {
        matches!(self.tonic, 5 | 10 | 3 | 8 | 1)
    }

    /// Pitch classes ascending from the tonic (the tonic comes first).
    pub fn pitch_classes(&self) -> Vec<u8> {
        self.mode.steps.iter().map(|s| (self.tonic + s) % 12).collect()
    }

    /// Pitch class of a (possibly out-of-range) degree.
    pub fn degree_pc(&self, degree: i32) -> u8 {
        let n = self.len() as i32;
        let idx = degree.rem_euclid(n) as usize;
        (self.tonic + self.mode.steps[idx]) % 12
    }

    /// MIDI note of `degree` in `octave`. Degree 0 in octave 4 is the tonic
    /// at or above C4 (so D lydian degree 0, octave 4 → D4 = 62). Degrees past
    /// the scale length wrap into higher octaves; negative degrees go down.
    pub fn degree_to_midi(&self, degree: i32, octave: i32) -> i32 {
        let n = self.len() as i32;
        let oct_shift = degree.div_euclid(n);
        let idx = degree.rem_euclid(n) as usize;
        (octave + 1) * 12 + self.tonic as i32 + self.mode.steps[idx] as i32 + 12 * oct_shift
    }

    /// Which degree (0-based) has this pitch class, if any.
    pub fn degree_of_pc(&self, pc: u8) -> Option<usize> {
        let rel = (pc as i32 - self.tonic as i32).rem_euclid(12) as u8;
        self.mode.steps.iter().position(|&s| s == rel)
    }

    /// Absolute degree index of a midi note (octaves × len + degree), or None
    /// if the note is not in the scale. The inverse of [`Scale::midi_from_index`].
    pub fn index_of_midi(&self, midi: i32) -> Option<i32> {
        let rel = midi - self.tonic as i32;
        let oct = rel.div_euclid(12);
        let pc = rel.rem_euclid(12) as u8;
        let deg = self.mode.steps.iter().position(|&s| s == pc)? as i32;
        Some(oct * self.len() as i32 + deg)
    }

    /// Midi note for an absolute degree index (see [`Scale::index_of_midi`]).
    pub fn midi_from_index(&self, index: i32) -> i32 {
        let n = self.len() as i32;
        let oct = index.div_euclid(n);
        let deg = index.rem_euclid(n) as usize;
        self.tonic as i32 + self.mode.steps[deg] as i32 + 12 * oct
    }

    /// Is this midi note in the scale?
    pub fn contains(&self, midi: i32) -> bool {
        let pc = midi.rem_euclid(12) as u8;
        self.degree_of_pc(pc).is_some()
    }

    /// Nearest scale note to a fractional midi value. Ties go to the lower note.
    pub fn nearest(&self, midi: f64) -> i32 {
        crate::quantize::Quantizer::nearest_in(&self.pitch_classes(), midi)
    }

    /// "D lydian"
    pub fn name(&self) -> String {
        format!("{} {}", pitch_class_name(self.tonic, self.prefers_flats()), self.mode.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_by_name;

    fn d_lydian() -> Scale {
        Scale::new(2, mode_by_name("lydian").unwrap())
    }

    #[test]
    fn pitch_classes_from_tonic() {
        assert_eq!(d_lydian().pitch_classes(), vec![2, 4, 6, 8, 9, 11, 1]);
        assert_eq!(d_lydian().name(), "D lydian");
        assert_eq!(Scale::new(10, mode_by_name("major").unwrap()).name(), "Bb ionian");
    }

    #[test]
    fn degrees_wrap_octaves() {
        let s = d_lydian();
        assert_eq!(s.degree_to_midi(0, 4), 62);
        assert_eq!(s.degree_to_midi(7, 4), 74);
        assert_eq!(s.degree_to_midi(-1, 4), 61); // C# below D4
        assert_eq!(s.degree_to_midi(4, 3), 57); // A3
    }

    #[test]
    fn index_roundtrip() {
        let s = d_lydian();
        for midi in 30..100 {
            if let Some(i) = s.index_of_midi(midi) {
                assert_eq!(s.midi_from_index(i), midi);
            } else {
                assert!(!s.contains(midi));
            }
        }
    }

    #[test]
    fn nearest_prefers_lower_on_tie() {
        let s = Scale::new(0, mode_by_name("major_pentatonic").unwrap()); // C D E G A
        assert_eq!(s.nearest(65.5), 64); // between E(64) and G(67)? no: 65.5 is 1.5 from both E and G → lower E
        assert_eq!(s.nearest(61.0), 60); // C# is 1 from C and 1 from D → C
        assert_eq!(s.nearest(66.0), 67); // F# is 2 from E, 1 from G
    }
}
