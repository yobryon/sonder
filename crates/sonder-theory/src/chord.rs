//! Chords as stacks of scale degrees, with naming.

use crate::scale::{pitch_class_name, Scale};

/// A chord built from a scale: a root degree and the degree offsets stacked
/// on it (`[0, 2, 4]` = triad, `[0, 2, 4, 6]` = seventh, `[0, 2, 4, 6, 8]` = ninth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// 0-based scale degree of the root.
    pub root_degree: u8,
    /// Scale-degree offsets from the root (first is normally 0).
    pub tones: Vec<u8>,
}

/// How a third/fifth stack reads, for naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quality {
    Major,
    Minor,
    Diminished,
    Augmented,
    Sus4,
    Sus2,
    /// Root + fifth, or something we can't classify.
    Open,
}

impl Chord {
    /// Build from a root degree and offsets.
    pub fn new(root_degree: u8, tones: Vec<u8>) -> Self {
        Self { root_degree, tones }
    }

    /// Number of extensions above the triad (0 triad, 1 seventh, 2 ninth…).
    pub fn extensions(&self) -> u8 {
        self.tones.len().saturating_sub(3) as u8
    }

    /// Pitch classes in stacking order (root first). Duplicates are removed.
    pub fn pitch_classes(&self, scale: &Scale) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(self.tones.len());
        for &t in &self.tones {
            let pc = scale.degree_pc(self.root_degree as i32 + t as i32);
            if !out.contains(&pc) {
                out.push(pc);
            }
        }
        out
    }

    /// Root pitch class.
    pub fn root_pc(&self, scale: &Scale) -> u8 {
        scale.degree_pc(self.root_degree as i32)
    }

    /// Intervals (semitones above root, 0..12) of every tone.
    fn intervals(&self, scale: &Scale) -> Vec<u8> {
        let root = self.root_pc(scale) as i32;
        self.pitch_classes(scale).iter().map(|&pc| (pc as i32 - root).rem_euclid(12) as u8).collect()
    }

    fn quality(iv: &[u8]) -> Quality {
        let has = |x: u8| iv.contains(&x);
        if has(4) {
            if has(8) && !has(7) { Quality::Augmented } else { Quality::Major }
        } else if has(3) {
            if has(6) && !has(7) { Quality::Diminished } else { Quality::Minor }
        } else if has(5) {
            Quality::Sus4
        } else if has(2) {
            Quality::Sus2
        } else {
            Quality::Open
        }
    }

    /// Jazz-style chord symbol: "D", "Em", "Bø", "Dmaj7", "G7", "F#m7", "Cmaj9", "Csus4".
    pub fn name(&self, scale: &Scale) -> String {
        let root = pitch_class_name(self.root_pc(scale), scale.prefers_flats());
        let iv = self.intervals(scale);
        let q = Self::quality(&iv);
        let has = |x: u8| iv.contains(&x);
        let seventh = if has(11) { Some(11) } else if has(10) { Some(10) } else if has(9) && q == Quality::Diminished { Some(9) } else { None };
        let ninth = has(2) && q != Quality::Sus2;

        let mut s = String::from(root);
        match (q, seventh) {
            (Quality::Major, None) => {}
            (Quality::Major, Some(11)) => s.push_str(if ninth { "maj9" } else { "maj7" }),
            (Quality::Major, Some(_)) => s.push_str(if ninth { "9" } else { "7" }),
            (Quality::Minor, None) => s.push('m'),
            (Quality::Minor, Some(11)) => s.push_str(if ninth { "m(maj9)" } else { "m(maj7)" }),
            (Quality::Minor, Some(_)) => s.push_str(if ninth { "m9" } else { "m7" }),
            (Quality::Diminished, None) => s.push('°'),
            (Quality::Diminished, Some(10)) => s.push_str(if ninth { "ø9" } else { "ø" }),
            (Quality::Diminished, Some(_)) => s.push_str("°7"),
            (Quality::Augmented, None) => s.push('+'),
            (Quality::Augmented, Some(11)) => s.push_str("+maj7"),
            (Quality::Augmented, Some(_)) => s.push_str("+7"),
            (Quality::Sus4, None) => s.push_str("sus4"),
            (Quality::Sus4, Some(_)) => s.push_str("7sus4"),
            (Quality::Sus2, _) => s.push_str("sus2"),
            (Quality::Open, None) => s.push('5'),
            (Quality::Open, Some(_)) => s.push_str("7(no3)"),
        }
        if ninth && seventh.is_none() && !matches!(q, Quality::Sus2 | Quality::Open) {
            s.push_str("add9");
        }
        s
    }

    /// Roman numeral: upper-case for major/augmented, lower-case for
    /// minor/diminished, with `°`/`+`/`7`/`9` suffixes ("ii", "V7", "viiø", "Imaj7").
    pub fn roman(&self, scale: &Scale) -> String {
        const NUMERALS: [&str; 12] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII"];
        let deg = (self.root_degree as usize) % scale.len().max(1);
        let numeral = NUMERALS[deg.min(11)];
        let iv = self.intervals(scale);
        let q = Self::quality(&iv);
        let has = |x: u8| iv.contains(&x);
        let mut s = match q {
            Quality::Major | Quality::Augmented | Quality::Sus4 | Quality::Sus2 | Quality::Open => numeral.to_string(),
            Quality::Minor | Quality::Diminished => numeral.to_ascii_lowercase(),
        };
        match q {
            Quality::Augmented => s.push('+'),
            Quality::Diminished if has(10) => s.push('ø'),
            Quality::Diminished => s.push('°'),
            _ => {}
        }
        let ext = if has(2) && (has(10) || has(11)) {
            "9"
        } else if has(11) && q == Quality::Major {
            "maj7"
        } else if has(10) || has(11) || (has(9) && q == Quality::Diminished) {
            if q == Quality::Diminished && has(10) { "" } else { "7" }
        } else {
            ""
        };
        s.push_str(ext);
        s
    }
}

/// Stack thirds on `degree`: `extensions` 0 → triad, 1 → seventh, 2 → ninth
/// (clamped to 4, i.e. a thirteenth).
pub fn diatonic_chord(scale: &Scale, degree: u8, extensions: u8) -> Chord {
    let n = scale.len().max(1) as u8;
    let ext = extensions.min(4) as usize;
    let tones: Vec<u8> = (0..3 + ext).map(|i| (2 * i) as u8).collect();
    Chord::new(degree % n, tones)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_by_name;

    fn scale(tonic: u8, mode: &str) -> Scale {
        Scale::new(tonic, mode_by_name(mode).unwrap())
    }

    #[test]
    fn names_in_d_major() {
        let d = scale(2, "major");
        let names: Vec<String> = (0..7).map(|deg| diatonic_chord(&d, deg, 0).name(&d)).collect();
        assert_eq!(names, vec!["D", "Em", "F#m", "G", "A", "Bm", "C#°"]);
        let sevenths: Vec<String> = (0..7).map(|deg| diatonic_chord(&d, deg, 1).name(&d)).collect();
        assert_eq!(sevenths, vec!["Dmaj7", "Em7", "F#m7", "Gmaj7", "A7", "Bm7", "C#ø"]);
        assert_eq!(diatonic_chord(&d, 0, 2).name(&d), "Dmaj9");
        assert_eq!(diatonic_chord(&d, 4, 2).name(&d), "A9");
        assert_eq!(diatonic_chord(&d, 1, 2).name(&d), "Em9");
    }

    #[test]
    fn flats_in_flat_keys_and_odd_qualities() {
        let f = scale(5, "major");
        assert_eq!(diatonic_chord(&f, 3, 0).name(&f), "Bb");
        let hm = scale(0, "harmonic_minor");
        assert_eq!(diatonic_chord(&hm, 2, 0).name(&hm), "Eb+");
        assert_eq!(diatonic_chord(&hm, 6, 1).name(&hm), "B°7");
        assert_eq!(diatonic_chord(&hm, 0, 1).name(&hm), "Cm(maj7)");
        assert_eq!(Chord::new(0, vec![0, 3, 4]).name(&scale(0, "major")), "Csus4");
    }

    #[test]
    fn romans() {
        let c = scale(0, "major");
        let r: Vec<String> = (0..7).map(|deg| diatonic_chord(&c, deg, 0).roman(&c)).collect();
        assert_eq!(r, vec!["I", "ii", "iii", "IV", "V", "vi", "vii°"]);
        assert_eq!(diatonic_chord(&c, 4, 1).roman(&c), "V7");
        assert_eq!(diatonic_chord(&c, 6, 1).roman(&c), "viiø");
        assert_eq!(diatonic_chord(&c, 0, 1).roman(&c), "Imaj7");
        assert_eq!(diatonic_chord(&c, 1, 2).roman(&c), "ii9");
    }

    #[test]
    fn pitch_classes_dedupe_and_wrap() {
        let p = scale(0, "major_pentatonic"); // C D E G A
        let ch = diatonic_chord(&p, 0, 2); // degrees 0,2,4,6,8 → C E A D G
        assert_eq!(ch.pitch_classes(&p), vec![0, 4, 9, 2, 7]);
        let ch = Chord::new(0, vec![0, 5]); // C and C (wrapped) → dedup
        assert_eq!(ch.pitch_classes(&p), vec![0]);
    }
}
