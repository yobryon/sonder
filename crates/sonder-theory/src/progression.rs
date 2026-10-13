//! Chord progression and mode drift policies.

use rand::{Rng, RngExt};

use crate::mode::{mode_by_name, modes_by_brightness};

/// Cadence table for 7-note scales: `BASE[from][to]` is the base weight of
/// moving from degree `from` to degree `to` (0-based, I = 0).
const BASE: [[f64; 7]; 7] = [
    // I     ii    iii   IV    V     vi    vii
    [0.04, 0.15, 0.08, 0.30, 0.25, 0.16, 0.02], // from I
    [0.10, 0.03, 0.05, 0.10, 0.55, 0.05, 0.12], // from ii
    [0.05, 0.10, 0.03, 0.30, 0.10, 0.40, 0.02], // from iii
    [0.30, 0.20, 0.03, 0.05, 0.35, 0.05, 0.02], // from IV
    [0.55, 0.03, 0.05, 0.12, 0.03, 0.20, 0.02], // from V
    [0.08, 0.30, 0.08, 0.30, 0.15, 0.03, 0.06], // from vi
    [0.55, 0.03, 0.15, 0.05, 0.12, 0.08, 0.02], // from vii
];

/// Modal wandering table: tonic-centred, favours IV / ii / vi / VII colour
/// chords and rarely the dominant, so the mode's flavour stays audible.
const MODAL: [[f64; 7]; 7] = [
    // I     ii    iii   IV    V     vi    vii
    [0.05, 0.22, 0.08, 0.28, 0.08, 0.14, 0.15], // from I
    [0.45, 0.03, 0.08, 0.20, 0.04, 0.10, 0.10], // from ii
    [0.35, 0.15, 0.03, 0.25, 0.04, 0.12, 0.06], // from iii
    [0.45, 0.15, 0.05, 0.03, 0.08, 0.12, 0.12], // from IV
    [0.45, 0.10, 0.05, 0.20, 0.03, 0.12, 0.05], // from V
    [0.40, 0.15, 0.05, 0.22, 0.05, 0.03, 0.10], // from vi
    [0.50, 0.10, 0.05, 0.20, 0.05, 0.08, 0.02], // from vii
];

fn weighted_pick(weights: &[f64], rng: &mut impl Rng) -> usize {
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return 0;
    }
    let mut r = rng.random::<f64>() * total;
    for (i, w) in weights.iter().enumerate() {
        if r < *w {
            return i;
        }
        r -= w;
    }
    weights.len() - 1
}

/// Shape a weight row by tension: flatten toward the unexpected and lean on
/// the dominant and leading-tone chords.
fn shaped(row: &[f64; 7], tension: f64) -> [f64; 7] {
    let t = tension.clamp(0.0, 1.0);
    let mut w = [0.0; 7];
    for (i, base) in row.iter().enumerate() {
        w[i] = base.max(1e-3).powf(1.0 - 0.75 * t);
    }
    w[4] += 0.35 * t;
    w[6] += 0.25 * t;
    w
}

/// Next chord degree from a weighted functional-harmony cadence table.
/// `tension` in 0..1 shifts weight toward less expected moves and toward the
/// dominant / leading-tone chords. Degrees are 0-based and taken mod 7.
pub fn functional_next(current_degree: u8, tension: f64, rng: &mut impl Rng) -> u8 {
    let row = &BASE[(current_degree % 7) as usize];
    weighted_pick(&shaped(row, tension), rng) as u8
}

/// Next chord degree for modal (non-functional) harmony: tonic-centred colour
/// changes. `tension` works as in [`functional_next`].
pub fn modal_next(current_degree: u8, tension: f64, rng: &mut impl Rng) -> u8 {
    let row = &MODAL[(current_degree % 7) as usize];
    weighted_pick(&shaped(row, tension), rng) as u8
}

/// The diatonic mode one accidental brighter (or darker) than `mode_name`.
/// `None` at the ends (lydian / locrian) or for non-diatonic modes.
pub fn modal_shift(mode_name: &str, brighter: bool) -> Option<&'static str> {
    let canonical = mode_by_name(mode_name)?.name;
    let order = modes_by_brightness();
    let i = order.iter().position(|&m| m == canonical)?;
    if brighter {
        order.get(i + 1).copied()
    } else {
        i.checked_sub(1).map(|j| order[j])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn modal_shift_is_one_accidental() {
        assert_eq!(modal_shift("ionian", true), Some("lydian"));
        assert_eq!(modal_shift("major", false), Some("mixolydian"));
        assert_eq!(modal_shift("lydian", true), None);
        assert_eq!(modal_shift("locrian", false), None);
        assert_eq!(modal_shift("hirajoshi", true), None);
        // Exactly one pitch class differs between neighbours.
        for pair in modes_by_brightness().windows(2) {
            let a = mode_by_name(pair[0]).unwrap().steps;
            let b = mode_by_name(pair[1]).unwrap().steps;
            let diff = a.iter().filter(|x| !b.contains(x)).count();
            assert_eq!(diff, 1, "{} vs {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn dominant_resolves_to_tonic_mostly() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let n = 2000;
        let to_tonic = (0..n)
            .filter(|_| functional_next(4, 0.0, &mut rng) == 0)
            .count();
        assert!(to_tonic > n / 3, "V→I {to_tonic}/{n}");
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        let to_tonic_tense = (0..n)
            .filter(|_| functional_next(4, 1.0, &mut rng) == 0)
            .count();
        assert!(
            to_tonic_tense < to_tonic,
            "tension should make V→I less inevitable"
        );
    }

    #[test]
    fn tension_favours_dominant_and_leading_tone() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let n = 3000;
        let calm = (0..n)
            .filter(|_| matches!(functional_next(0, 0.0, &mut rng), 4 | 6))
            .count();
        let tense = (0..n)
            .filter(|_| matches!(functional_next(0, 1.0, &mut rng), 4 | 6))
            .count();
        assert!(tense > calm);
        for d in 0..7 {
            assert!(modal_next(d, 0.5, &mut rng) < 7);
        }
    }
}
