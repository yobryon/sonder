//! Voicings and voice leading.

use crate::quantize::Quantizer;

/// A set of sounding midi notes, one per voice. Voice `i` of the result of
/// [`voice_lead`] continues voice `i` of the input, so callers can keep note
/// identities across chord changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Voicing(pub Vec<i32>);

impl Voicing {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A close voicing of `pcs` (root first) stacked upward from `lo`, with
    /// `n` voices (cycling through the pitch classes if `n` exceeds them),
    /// pulled down by octaves until it fits under `hi` where possible.
    pub fn close(pcs: &[u8], lo: i32, hi: i32, n: usize) -> Voicing {
        if pcs.is_empty() || n == 0 {
            return Voicing(Vec::new());
        }
        let mut notes = Vec::with_capacity(n);
        let mut prev = lo - 1;
        for i in 0..n {
            let pc = pcs[i % pcs.len()] as i32 % 12;
            let next = prev + 1 + (pc - (prev + 1)).rem_euclid(12);
            notes.push(next);
            prev = next;
        }
        while notes.last().copied().unwrap_or(lo) > hi && notes[0] - 12 >= lo {
            for n in &mut notes {
                *n -= 12;
            }
        }
        Voicing(notes)
    }

    /// Total absolute motion in semitones between two voicings of equal size.
    pub fn motion_to(&self, other: &Voicing) -> i32 {
        self.0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| (a - b).abs())
            .sum()
    }
}

/// Move `from` to a voicing of `to_pcs` with `n` voices inside `lo..=hi`,
/// minimising total semitone motion. Every pitch class is used at least once
/// when `n >= to_pcs.len()`, and no pitch class is doubled when `n <= to_pcs.len()`.
/// Voice order is preserved (voice `i` → voice `i`). If `from` has a different
/// size it is first grown (with evenly spread voices) or shrunk (dropping the
/// highest voices).
pub fn voice_lead(from: &Voicing, to_pcs: &[u8], lo: i32, hi: i32, n: usize) -> Voicing {
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
    if to_pcs.is_empty() || n == 0 {
        return Voicing(Vec::new());
    }
    let pcs: Vec<u8> = {
        let mut v: Vec<u8> = Vec::new();
        for &p in to_pcs {
            if !v.contains(&(p % 12)) {
                v.push(p % 12);
            }
        }
        v
    };
    if from.is_empty() {
        return Voicing::close(&pcs, lo, hi, n);
    }

    // Normalise the number of voices.
    let mut origin: Vec<i32> = from.0.clone();
    origin.truncate(n);
    while origin.len() < n {
        let i = origin.len();
        origin.push(lo + ((hi - lo) * i as i32) / n.max(1) as i32);
    }

    let k = pcs.len();
    // cost[v][p]: cheapest in-window note of pc p for voice v, and that note.
    let mut best_note = vec![vec![0i32; k]; n];
    let mut cost = vec![vec![0i32; k]; n];
    for (v, &o) in origin.iter().enumerate() {
        for (p, &pc) in pcs.iter().enumerate() {
            let candidates = Quantizer::notes_in_window(&[pc], lo, hi);
            let (note, c) = candidates
                .iter()
                .map(|&m| (m, (m - o).abs()))
                .min_by_key(|&(m, c)| (c, m))
                .unwrap_or_else(|| {
                    let m = Quantizer::nearest_in(&[pc], o as f64);
                    (m, (m - o).abs())
                });
            best_note[v][p] = note;
            cost[v][p] = c;
        }
    }

    // Branch-and-bound over voice→pc assignments.
    struct Search<'a> {
        n: usize,
        k: usize,
        cost: &'a [Vec<i32>],
        best_total: i32,
        best: Vec<usize>,
        current: Vec<usize>,
        used: Vec<u8>,
    }
    impl Search<'_> {
        fn go(&mut self, v: usize, total: i32) {
            if total >= self.best_total {
                return;
            }
            if v == self.n {
                self.best_total = total;
                self.best = self.current.clone();
                return;
            }
            let remaining = self.n - v;
            let uncovered = self.used.iter().filter(|&&u| u == 0).count();
            if remaining < uncovered {
                return;
            }
            // Must we cover a new pc with this voice?
            let must_cover = remaining == uncovered;
            let distinct_only = self.n <= self.k;
            let mut order: Vec<usize> = (0..self.k).collect();
            order.sort_by_key(|&p| self.cost[v][p]);
            for p in order {
                if self.used[p] > 0 && (distinct_only || must_cover) {
                    continue;
                }
                self.used[p] += 1;
                self.current.push(p);
                self.go(v + 1, total + self.cost[v][p]);
                self.current.pop();
                self.used[p] -= 1;
            }
        }
    }
    let mut s = Search {
        n,
        k,
        cost: &cost,
        best_total: i32::MAX,
        best: Vec::new(),
        current: Vec::new(),
        used: vec![0; k],
    };
    s.go(0, 0);
    let assignment = if s.best.len() == n {
        s.best
    } else {
        (0..n).map(|v| v % k).collect()
    };

    // Realise notes; avoid two voices landing on the same midi note.
    let mut notes: Vec<i32> = assignment
        .iter()
        .enumerate()
        .map(|(v, &p)| best_note[v][p])
        .collect();
    for v in 0..n {
        let pc = pcs[assignment[v]];
        let mut tries = 0;
        while notes[..v].contains(&notes[v]) && tries < 8 {
            let alts = Quantizer::notes_in_window(&[pc], lo, hi);
            let o = origin[v];
            let pick = alts
                .iter()
                .copied()
                .filter(|m| !notes[..v].contains(m))
                .min_by_key(|&m| ((m - o).abs(), m));
            match pick {
                Some(m) => notes[v] = m,
                None => break,
            }
            tries += 1;
        }
    }
    Voicing(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcs_of(v: &Voicing) -> Vec<u8> {
        let mut p: Vec<u8> = v.0.iter().map(|m| m.rem_euclid(12) as u8).collect();
        p.sort();
        p.dedup();
        p
    }

    #[test]
    fn close_voicing_stacks_upward() {
        let v = Voicing::close(&[2, 6, 9], 60, 84, 3);
        assert_eq!(v.0, vec![62, 66, 69]);
        let v = Voicing::close(&[2, 6, 9], 60, 84, 4);
        assert_eq!(v.0, vec![62, 66, 69, 74]);
        let v = Voicing::close(&[2, 6, 9], 60, 72, 5); // too tall: pulled down, lo respected
        assert!(v.0[0] >= 60);
    }

    #[test]
    fn c_to_f_moves_minimally() {
        // C E G → F A C: classic common-tone voice leading, total motion 1+1+0 = 2? (E→F, G→A, C stays)
        let from = Voicing(vec![60, 64, 67]);
        let to = voice_lead(&from, &[5, 9, 0], 48, 84, 3);
        assert_eq!(pcs_of(&to), vec![0, 5, 9]);
        assert_eq!(from.motion_to(&to), 3);
        assert_eq!(to.0, vec![60, 65, 69]);
    }

    #[test]
    fn c_to_g_keeps_common_tone() {
        let from = Voicing(vec![60, 64, 67]);
        let to = voice_lead(&from, &[7, 11, 2], 48, 84, 3);
        assert_eq!(pcs_of(&to), vec![2, 7, 11]);
        assert_eq!(to.0[2], 67); // G stays
        assert_eq!(from.motion_to(&to), 3); // C→D (2) and E→D? no: E→D=2,C→B=1 → total 3
    }

    #[test]
    fn doubling_covers_all_pcs_and_avoids_unisons() {
        let from = Voicing(vec![60, 64, 67, 72]);
        let to = voice_lead(&from, &[2, 6, 9], 48, 84, 4);
        assert_eq!(pcs_of(&to), vec![2, 6, 9]);
        let mut sorted = to.0.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "no unisons: {:?}", to.0);
    }

    #[test]
    fn stays_in_window_and_resizes() {
        let from = Voicing(vec![60, 64]);
        let to = voice_lead(&from, &[0, 4, 7, 11], 55, 70, 4);
        assert_eq!(to.len(), 4);
        assert!(to.0.iter().all(|&m| (55..=70).contains(&m)));
        assert_eq!(pcs_of(&to), vec![0, 4, 7, 11]);
        let shrunk = voice_lead(&Voicing(vec![60, 64, 67, 71]), &[0, 4, 7], 55, 80, 3);
        assert_eq!(shrunk.len(), 3);
    }

    #[test]
    fn empty_input_yields_close_voicing() {
        let v = voice_lead(&Voicing::default(), &[0, 4, 7], 60, 84, 3);
        assert_eq!(v.0, vec![60, 64, 67]);
        assert!(voice_lead(&Voicing::default(), &[], 60, 84, 3).is_empty());
    }
}
