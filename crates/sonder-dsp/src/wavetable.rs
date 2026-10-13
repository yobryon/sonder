//! Band-limited single-cycle wavetables. Every [`Table`] is generated once by
//! additive synthesis into [`MIPS`] mip levels, each keeping fewer harmonics so
//! that a voice can pick the level whose highest partial stays below Nyquist.
//!
//! The tables live in a process-wide [`OnceLock`]; [`Synth::new`](crate::Synth::new)
//! forces initialisation so the audio thread never allocates.

use std::sync::OnceLock;

use sonder_core::Table;

/// Samples per cycle.
pub const TABLE_LEN: usize = 2048;
/// Number of mip levels per table.
pub const MIPS: usize = 4;
/// Highest harmonic in mip 0; each further level halves it (64, 32, 16, 8).
pub const MAX_HARMONIC: usize = 64;
const N_TABLES: usize = Table::ALL.len();
/// Stored cycle length (one guard sample so interpolation needs no wrap).
const STRIDE: usize = TABLE_LEN + 1;

/// All tables, all mips, contiguous.
#[derive(Debug)]
pub struct Tables {
    data: Vec<f32>,
}

static TABLES: OnceLock<Tables> = OnceLock::new();

/// Shared tables, generated on first use.
pub fn tables() -> &'static Tables {
    TABLES.get_or_init(Tables::generate)
}

/// Highest harmonic kept at mip `level`.
#[inline]
pub fn mip_max_harmonic(level: usize) -> usize {
    MAX_HARMONIC >> level.min(MIPS - 1)
}

/// Pick the mip level for a fundamental: the most detailed one whose highest
/// harmonic stays below Nyquist (the coarsest if none does).
#[inline]
pub fn mip_for(freq_hz: f32, sample_rate: f32) -> usize {
    let f = freq_hz.max(1e-3);
    let allowed = sample_rate * 0.5 / f;
    for level in 0..MIPS {
        if (mip_max_harmonic(level) as f32) <= allowed {
            return level;
        }
    }
    MIPS - 1
}

/// Relative amplitude of harmonic `n` (1-based) for each table, before band limiting.
fn harmonic(table: Table, n: usize) -> f32 {
    let nf = n as f32;
    match table {
        Table::Organ => match n {
            1 => 1.0,
            2 => 0.8,
            3 => 0.5,
            4 => 0.6,
            6 => 0.3,
            8 => 0.2,
            _ => 0.0,
        },
        Table::Vox => {
            // 1/n backbone with two formant bumps (around partials 3-4 and 9-10).
            let f1 = (-((nf - 3.5) * (nf - 3.5)) / 2.0).exp();
            let f2 = (-((nf - 9.5) * (nf - 9.5)) / 4.0).exp();
            if n > 40 {
                0.0
            } else {
                (0.35 + 1.6 * f1 + 1.0 * f2) / nf
            }
        }
        Table::Glass => match n {
            1 => 1.0,
            2 => 0.08,
            3 => 0.45,
            5 => 0.25,
            7 => 0.14,
            11 => 0.07,
            _ => 0.0,
        },
        Table::Metal => {
            const PRIMES: [usize; 12] = [1, 2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31];
            if PRIMES.contains(&n) {
                1.0 / nf.sqrt()
            } else {
                0.0
            }
        }
        Table::Pulse3 => {
            // Fourier series of a 1/3-duty pulse: |sin(πn/3)| / n.
            (std::f32::consts::PI * nf / 3.0).sin().abs() / nf
        }
        Table::Sine8 => {
            if n <= 8 {
                1.0 / nf
            } else {
                0.0
            }
        }
    }
}

impl Tables {
    fn generate() -> Self {
        let mut data = vec![0.0f32; N_TABLES * MIPS * STRIDE];
        for (ti, &table) in Table::ALL.iter().enumerate() {
            let mut norm = 1.0f32;
            for level in 0..MIPS {
                let max_h = mip_max_harmonic(level);
                let start = (ti * MIPS + level) * STRIDE;
                let cycle = &mut data[start..start + STRIDE];
                for (i, s) in cycle.iter_mut().enumerate().take(TABLE_LEN) {
                    let phase = i as f64 / TABLE_LEN as f64;
                    let mut acc = 0.0f64;
                    for n in 1..=max_h {
                        let a = harmonic(table, n) as f64;
                        if a != 0.0 {
                            // Alternate the sign of even partials so peaks don't pile up.
                            let sign = if n % 2 == 0 { -1.0 } else { 1.0 };
                            acc += sign * a * (std::f64::consts::TAU * phase * n as f64).sin();
                        }
                    }
                    *s = acc as f32;
                }
                // Normalise every mip by the peak of mip 0 so level is pitch-independent.
                if level == 0 {
                    let peak = cycle.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-6);
                    norm = 1.0 / peak;
                }
                for s in cycle.iter_mut().take(TABLE_LEN) {
                    *s *= norm;
                }
                cycle[TABLE_LEN] = cycle[0];
            }
        }
        Self { data }
    }

    /// One cycle (plus guard sample) of `table` at mip `level`.
    #[inline]
    pub fn cycle(&self, table: Table, level: usize) -> &[f32] {
        let ti = table as usize;
        let start = (ti * MIPS + level.min(MIPS - 1)) * STRIDE;
        &self.data[start..start + STRIDE]
    }

    /// Linearly interpolated read at `phase` (turns, any value) from mip `level`.
    #[inline]
    pub fn sample(&self, table: Table, level: usize, phase: f64) -> f32 {
        let cycle = self.cycle(table, level);
        let p = phase - phase.floor();
        let x = p * TABLE_LEN as f64;
        let i = (x as usize).min(TABLE_LEN - 1);
        let frac = (x - i as f64) as f32;
        let a = cycle[i];
        let b = cycle[i + 1];
        a + (b - a) * frac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_and_mip_is_finite_and_bounded() {
        let t = tables();
        for &table in &Table::ALL {
            for level in 0..MIPS {
                let c = t.cycle(table, level);
                assert_eq!(c.len(), STRIDE);
                let mut peak = 0f32;
                for &s in c {
                    assert!(s.is_finite(), "{table:?} mip {level}");
                    peak = peak.max(s.abs());
                }
                assert!(peak > 0.3 && peak <= 1.5, "{table:?} mip {level} peak {peak}");
                assert_eq!(c[0], c[TABLE_LEN], "guard sample");
            }
            // Mip 0 is normalised to unit peak.
            let p0 = t.cycle(table, 0).iter().fold(0f32, |m, s| m.max(s.abs()));
            assert!((p0 - 1.0).abs() < 1e-4, "{table:?} peak {p0}");
        }
    }

    #[test]
    fn mip_selection_follows_pitch() {
        let sr = 48_000.0;
        assert_eq!(mip_for(100.0, sr), 0);
        assert_eq!(mip_for(500.0, sr), 1);
        assert_eq!(mip_for(1000.0, sr), 2);
        assert_eq!(mip_for(2500.0, sr), 3);
        assert_eq!(mip_for(10_000.0, sr), MIPS - 1);
        let mut last = 0;
        for i in 1..200 {
            let m = mip_for(i as f32 * 50.0, sr);
            assert!(m >= last);
            last = m;
        }
    }

    #[test]
    fn interpolation_is_continuous_and_wraps() {
        let t = tables();
        for &table in &Table::ALL {
            let a = t.sample(table, 0, 0.999_999);
            let b = t.sample(table, 0, 1.0);
            let c = t.sample(table, 0, 0.0);
            assert!((a - b).abs() < 0.05, "{table:?}");
            assert_eq!(b, c);
            assert!(t.sample(table, 0, -0.25).is_finite());
        }
    }
}
