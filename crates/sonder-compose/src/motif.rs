//! Motif memory: short phrases melodic generators played, kept so they can be
//! restated later — plainly in the same section, transformed across sections.
//!
//! A motif is stored shape-only: scale-degree intervals from its first note and
//! onset offsets in beats from its first onset. Restating it means picking a
//! new base degree, so the same motif fits any chord or register.

use rand::RngExt;
use rand_chacha::ChaCha8Rng;

/// Most motifs remembered per piece.
pub const MAX_MOTIFS: usize = 8;
/// Phrase length bounds (notes) for a phrase to count as a motif.
pub const MIN_NOTES: usize = 2;
pub const MAX_NOTES: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct Motif {
    /// Scale-degree offsets from the first note (first is 0).
    pub degrees: Vec<i32>,
    /// Onset offsets in beats from the first note (first is 0).
    pub beats: Vec<f64>,
    /// Form section in which it was first played.
    pub section: String,
    /// Stable hash of the shape, for display.
    pub hash: u64,
    /// "M1", "M2", … in order of discovery.
    pub name: String,
}

/// How a motif is being restated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Plain,
    /// Same shape from a chord tone (the generator picks the base).
    Transpose,
    /// Mirror the intervals.
    Invert,
    /// Twice as slow.
    Augment,
    /// Backwards.
    Retrograde,
}

impl Transform {
    pub fn suffix(self) -> &'static str {
        match self {
            Transform::Plain => "",
            Transform::Transpose => " tr",
            Transform::Invert => " inv",
            Transform::Augment => " aug",
            Transform::Retrograde => " retro",
        }
    }
}

/// A motif ready to be played back.
#[derive(Debug, Clone, PartialEq)]
pub struct Restatement {
    pub degrees: Vec<i32>,
    pub beats: Vec<f64>,
    pub transform: Transform,
    /// "M2 inv"
    pub name: String,
}

#[derive(Debug, Default)]
pub struct MotifMemory {
    motifs: Vec<Motif>,
    discovered: u32,
    /// Name of the motif in play (empty when none).
    current: String,
}

fn hash_shape(degrees: &[i32], beats: &[f64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for &d in degrees {
        mix(d as i64 as u64);
    }
    for &b in beats {
        mix(((b * 8.0).round() as i64) as u64);
    }
    h
}

impl MotifMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.motifs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.motifs.is_empty()
    }
    pub fn motifs(&self) -> &[Motif] {
        &self.motifs
    }

    /// Name of the motif currently being restated (empty when none).
    pub fn current(&self) -> &str {
        &self.current
    }
    pub fn set_current(&mut self, name: &str) {
        self.current = name.to_string();
    }
    pub fn clear_current(&mut self) {
        self.current.clear();
    }

    /// Remember a phrase: absolute scale-degree indices and onset beats, in
    /// order. Too short / long phrases and exact repeats are ignored. Returns
    /// the motif's name when it was new.
    pub fn remember(&mut self, indices: &[i32], onsets: &[f64], section: &str) -> Option<String> {
        let n = indices.len().min(onsets.len());
        if !(MIN_NOTES..=MAX_NOTES).contains(&n) {
            return None;
        }
        let degrees: Vec<i32> = indices[..n].iter().map(|i| i - indices[0]).collect();
        let beats: Vec<f64> = onsets[..n]
            .iter()
            .map(|b| ((b - onsets[0]) * 8.0).round() / 8.0)
            .collect();
        if beats.windows(2).any(|w| w[1] <= w[0]) || degrees.iter().all(|&d| d == 0) {
            return None;
        }
        let hash = hash_shape(&degrees, &beats);
        if self.motifs.iter().any(|m| m.hash == hash) {
            return None;
        }
        self.discovered += 1;
        let name = format!("M{}", self.discovered);
        if self.motifs.len() >= MAX_MOTIFS {
            self.motifs.remove(0);
        }
        self.motifs.push(Motif {
            degrees,
            beats,
            section: section.to_string(),
            hash,
            name: name.clone(),
        });
        Some(name)
    }

    /// Pick a motif to restate in `section`. Same-section motifs are preferred
    /// and mostly restated plainly; motifs from other sections are mostly
    /// transformed. `None` when the memory is empty.
    pub fn recall(&mut self, section: &str, rng: &mut ChaCha8Rng) -> Option<Restatement> {
        if self.motifs.is_empty() {
            return None;
        }
        let same: Vec<usize> = self
            .motifs
            .iter()
            .enumerate()
            .filter(|(_, m)| m.section == section)
            .map(|(i, _)| i)
            .collect();
        let pick_same = !same.is_empty() && rng.random::<f64>() < 0.7;
        let idx = if pick_same {
            same[rng.random_range(0..same.len())]
        } else {
            rng.random_range(0..self.motifs.len())
        };
        let m = &self.motifs[idx];
        let transform = if m.section == section {
            if rng.random::<f64>() < 0.7 {
                Transform::Plain
            } else {
                Transform::Transpose
            }
        } else {
            match rng.random_range(0..5) {
                0 => Transform::Plain,
                1 => Transform::Transpose,
                2 => Transform::Invert,
                3 => Transform::Augment,
                _ => Transform::Retrograde,
            }
        };
        let r = Self::apply(m, transform);
        self.current = r.name.clone();
        Some(r)
    }

    fn apply(m: &Motif, transform: Transform) -> Restatement {
        let (degrees, beats) = match transform {
            Transform::Plain | Transform::Transpose => (m.degrees.clone(), m.beats.clone()),
            Transform::Invert => (m.degrees.iter().map(|d| -d).collect(), m.beats.clone()),
            Transform::Augment => (m.degrees.clone(), m.beats.iter().map(|b| b * 2.0).collect()),
            Transform::Retrograde => {
                let mut d: Vec<i32> = m.degrees.iter().rev().copied().collect();
                let first = d[0];
                for x in &mut d {
                    *x -= first;
                }
                (d, m.beats.clone())
            }
        };
        Restatement {
            degrees,
            beats,
            transform,
            name: format!("{}{}", m.name, transform.suffix()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn remembers_shapes_not_pitches() {
        let mut mem = MotifMemory::new();
        assert_eq!(
            mem.remember(&[10, 12, 11], &[0.0, 0.5, 1.0], "A"),
            Some("M1".into())
        );
        // Same shape, transposed and shifted in time: not new.
        assert_eq!(mem.remember(&[3, 5, 4], &[8.0, 8.5, 9.0], "B"), None);
        assert_eq!(mem.remember(&[3], &[0.0], "A"), None, "too short");
        assert_eq!(
            mem.remember(&[3, 3, 3], &[0.0, 0.5, 1.0], "A"),
            None,
            "no shape"
        );
        assert_eq!(
            mem.remember(
                &[3, 4, 5, 6, 7, 8, 9],
                &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
                "A"
            ),
            None,
            "too long"
        );
        assert_eq!(mem.remember(&[0, 2], &[0.0, 0.25], "A"), Some("M2".into()));
        assert_eq!(mem.len(), 2);
        assert_eq!(mem.motifs()[0].degrees, vec![0, 2, 1]);
    }

    #[test]
    fn memory_is_bounded() {
        let mut mem = MotifMemory::new();
        for i in 0..20 {
            mem.remember(&[0, i + 1], &[0.0, 0.5], "A");
        }
        assert_eq!(mem.len(), MAX_MOTIFS);
        assert_eq!(mem.motifs()[0].name, "M13");
    }

    #[test]
    fn recall_prefers_plain_at_home_and_transforms_abroad() {
        let mut mem = MotifMemory::new();
        mem.remember(&[0, 2, 1, 4], &[0.0, 0.5, 1.0, 2.0], "A");
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let mut plain_home = 0;
        let mut transformed_abroad = 0;
        for _ in 0..200 {
            let r = mem.recall("A", &mut rng).unwrap();
            if r.transform == Transform::Plain {
                plain_home += 1;
            }
            let r = mem.recall("B", &mut rng).unwrap();
            if r.transform != Transform::Plain {
                transformed_abroad += 1;
            }
            assert_eq!(r.degrees[0], 0);
            assert_eq!(r.beats[0], 0.0);
        }
        assert!(plain_home > 110, "{plain_home}");
        assert!(transformed_abroad > 120, "{transformed_abroad}");
        assert!(mem.current().starts_with("M1"));
        mem.clear_current();
        assert!(mem.current().is_empty());
        assert!(MotifMemory::new().recall("A", &mut rng).is_none());
    }

    #[test]
    fn transforms_are_right() {
        let m = Motif {
            degrees: vec![0, 2, -1],
            beats: vec![0.0, 0.5, 1.5],
            section: "A".into(),
            hash: 0,
            name: "M1".into(),
        };
        let inv = MotifMemory::apply(&m, Transform::Invert);
        assert_eq!(inv.degrees, vec![0, -2, 1]);
        assert_eq!(inv.name, "M1 inv");
        let aug = MotifMemory::apply(&m, Transform::Augment);
        assert_eq!(aug.beats, vec![0.0, 1.0, 3.0]);
        let retro = MotifMemory::apply(&m, Transform::Retrograde);
        assert_eq!(retro.degrees, vec![0, 3, 1]);
        assert_eq!(retro.beats, m.beats);
    }
}
