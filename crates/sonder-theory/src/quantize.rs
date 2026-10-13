//! Pitch quantization: snap arbitrary pitches to a set of allowed pitch classes.

/// Pitch quantization helpers. Stateless; every method is a plain function.
pub struct Quantizer;

impl Quantizer {
    /// Nearest midi note whose pitch class is in `pcs`. Ties resolve to the
    /// lower note. With an empty set the input is simply rounded.
    pub fn nearest_in(pcs: &[u8], midi: f64) -> i32 {
        if pcs.is_empty() {
            return midi.round() as i32;
        }
        let base = midi.floor() as i32;
        let mut best: Option<(f64, i32)> = None;
        // Candidates: for every pc, the instance nearest below and above.
        for &pc in pcs {
            let pc = pc as i32 % 12;
            let below = base - (base - pc).rem_euclid(12);
            for cand in [below, below + 12] {
                let d = (cand as f64 - midi).abs();
                let better = match best {
                    None => true,
                    Some((bd, bm)) => d < bd - 1e-9 || ((d - bd).abs() <= 1e-9 && cand < bm),
                };
                if better {
                    best = Some((d, cand));
                }
            }
        }
        best.map(|(_, m)| m).unwrap_or(midi.round() as i32)
    }

    /// All midi notes in `lo..=hi` whose pitch class is in `pcs`, ascending.
    pub fn notes_in_window(pcs: &[u8], lo: i32, hi: i32) -> Vec<i32> {
        (lo..=hi)
            .filter(|m| pcs.iter().any(|&pc| (pc as i32 % 12) == m.rem_euclid(12)))
            .collect()
    }

    /// Pick the `pos01`-th member of the set inside the midi window `lo..=hi`
    /// (0 → lowest, 1 → highest). If the window holds no member, falls back to
    /// the nearest member to the interpolated position.
    pub fn in_register(pcs: &[u8], pos01: f64, lo: i32, hi: i32) -> i32 {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        let pos = if pos01.is_finite() {
            pos01.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let notes = Self::notes_in_window(pcs, lo, hi);
        if notes.is_empty() {
            let target = lo as f64 + (hi - lo) as f64 * pos;
            return Self::nearest_in(pcs, target);
        }
        let idx = (pos * (notes.len() - 1) as f64).round() as usize;
        notes[idx.min(notes.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_basic_and_ties() {
        let cmaj = [0, 2, 4, 5, 7, 9, 11];
        assert_eq!(Quantizer::nearest_in(&cmaj, 60.0), 60);
        assert_eq!(Quantizer::nearest_in(&cmaj, 60.4), 60);
        assert_eq!(Quantizer::nearest_in(&cmaj, 61.0), 60); // tie C/D → lower
        assert_eq!(Quantizer::nearest_in(&cmaj, 61.01), 62);
        assert_eq!(Quantizer::nearest_in(&[0], 66.0), 60); // tie C4/C5 → lower
        assert_eq!(Quantizer::nearest_in(&[0], 66.1), 72);
        assert_eq!(Quantizer::nearest_in(&[], 63.6), 64);
    }

    #[test]
    fn register_picks_by_position() {
        let dmaj = [2, 6, 9]; // D F# A
        let notes = Quantizer::notes_in_window(&dmaj, 60, 72);
        assert_eq!(notes, vec![62, 66, 69]);
        assert_eq!(Quantizer::in_register(&dmaj, 0.0, 60, 72), 62);
        assert_eq!(Quantizer::in_register(&dmaj, 1.0, 60, 72), 69);
        assert_eq!(Quantizer::in_register(&dmaj, 0.5, 60, 72), 66);
        // out-of-range positions clamp; NaN → middle
        assert_eq!(Quantizer::in_register(&dmaj, 7.0, 60, 72), 69);
        assert_eq!(Quantizer::in_register(&dmaj, f64::NAN, 60, 72), 66);
        // empty window falls back to nearest
        assert_eq!(Quantizer::in_register(&[1], 0.5, 62, 63), 61);
    }
}
