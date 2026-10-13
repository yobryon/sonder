//! Modes and scale shapes.

/// A scale shape: pitch classes above the tonic, plus a brightness rank.
///
/// Brightness orders the seven diatonic modes by how many sharps they carry
/// relative to Dorian (the symmetric centre): locrian −3 … lydian +3. Other
/// shapes get an approximate rank so a conductor can still reason about them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    /// Canonical name, e.g. `"lydian"`.
    pub name: &'static str,
    /// Pitch classes from the tonic, ascending, starting at 0.
    pub steps: Vec<u8>,
    /// Relative brightness, −3 (locrian) … +3 (lydian).
    pub brightness: i8,
}

impl Mode {
    /// Number of notes in the mode.
    pub fn len(&self) -> usize {
        self.steps.len()
    }
    /// Always false: every mode has at least one note.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
    /// Is this one of the seven diatonic (church) modes?
    pub fn is_diatonic(&self) -> bool {
        DIATONIC_BY_BRIGHTNESS.contains(&self.name)
    }
}

struct ModeDef {
    name: &'static str,
    aliases: &'static [&'static str],
    steps: &'static [u8],
    brightness: i8,
}

const MODES: &[ModeDef] = &[
    ModeDef { name: "lydian", aliases: &[], steps: &[0, 2, 4, 6, 7, 9, 11], brightness: 3 },
    ModeDef { name: "ionian", aliases: &["major", "maj"], steps: &[0, 2, 4, 5, 7, 9, 11], brightness: 2 },
    ModeDef { name: "mixolydian", aliases: &["dominant"], steps: &[0, 2, 4, 5, 7, 9, 10], brightness: 1 },
    ModeDef { name: "dorian", aliases: &[], steps: &[0, 2, 3, 5, 7, 9, 10], brightness: 0 },
    ModeDef {
        name: "aeolian",
        aliases: &["minor", "min", "natural_minor"],
        steps: &[0, 2, 3, 5, 7, 8, 10],
        brightness: -1,
    },
    ModeDef { name: "phrygian", aliases: &[], steps: &[0, 1, 3, 5, 7, 8, 10], brightness: -2 },
    ModeDef { name: "locrian", aliases: &[], steps: &[0, 1, 3, 5, 6, 8, 10], brightness: -3 },
    ModeDef {
        name: "major_pentatonic",
        aliases: &["pentatonic", "pentatonic_major"],
        steps: &[0, 2, 4, 7, 9],
        brightness: 2,
    },
    ModeDef {
        name: "minor_pentatonic",
        aliases: &["pentatonic_minor"],
        steps: &[0, 3, 5, 7, 10],
        brightness: -1,
    },
    ModeDef { name: "whole_tone", aliases: &["wholetone"], steps: &[0, 2, 4, 6, 8, 10], brightness: 3 },
    ModeDef { name: "harmonic_minor", aliases: &[], steps: &[0, 2, 3, 5, 7, 8, 11], brightness: -1 },
    ModeDef { name: "melodic_minor", aliases: &["jazz_minor"], steps: &[0, 2, 3, 5, 7, 9, 11], brightness: 0 },
    ModeDef { name: "blues", aliases: &["blues_minor"], steps: &[0, 3, 5, 6, 7, 10], brightness: -1 },
    ModeDef { name: "hirajoshi", aliases: &[], steps: &[0, 2, 3, 7, 8], brightness: -2 },
    ModeDef { name: "in_sen", aliases: &["insen"], steps: &[0, 1, 5, 7, 10], brightness: -2 },
    ModeDef {
        name: "chromatic",
        aliases: &[],
        steps: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
        brightness: 0,
    },
];

/// The seven diatonic modes, darkest first.
const DIATONIC_BY_BRIGHTNESS: &[&str] =
    &["locrian", "phrygian", "aeolian", "dorian", "mixolydian", "ionian", "lydian"];

/// Normalize user spelling: case-insensitive, `-`/space → `_`.
fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// Look a mode up by name or alias (`"major"`, `"Dorian"`, `"minor-pentatonic"`…).
pub fn mode_by_name(s: &str) -> Option<Mode> {
    let key = normalize(s);
    MODES
        .iter()
        .find(|m| m.name == key || m.aliases.contains(&key.as_str()))
        .map(|m| Mode { name: m.name, steps: m.steps.to_vec(), brightness: m.brightness })
}

/// The seven diatonic modes ordered darkest → brightest:
/// locrian, phrygian, aeolian, dorian, mixolydian, ionian, lydian.
pub fn modes_by_brightness() -> &'static [&'static str] {
    DIATONIC_BY_BRIGHTNESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve() {
        assert_eq!(mode_by_name("major").unwrap().name, "ionian");
        assert_eq!(mode_by_name("Minor").unwrap().name, "aeolian");
        assert_eq!(mode_by_name("minor-pentatonic").unwrap().steps, vec![0, 3, 5, 7, 10]);
        assert!(mode_by_name("klingon").is_none());
    }

    #[test]
    fn brightness_is_monotonic_over_diatonic_modes() {
        let ranks: Vec<i8> = modes_by_brightness().iter().map(|n| mode_by_name(n).unwrap().brightness).collect();
        assert_eq!(ranks, vec![-3, -2, -1, 0, 1, 2, 3]);
    }

    #[test]
    fn all_modes_start_on_tonic_and_ascend() {
        for d in MODES {
            assert_eq!(d.steps[0], 0, "{}", d.name);
            assert!(d.steps.windows(2).all(|w| w[0] < w[1]), "{}", d.name);
            assert!(d.steps.iter().all(|&s| s < 12));
        }
    }
}
