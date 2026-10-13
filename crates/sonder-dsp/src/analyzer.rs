//! Control-thread spectrum analyzer for the UI: accumulates interleaved stereo
//! into a mono window and produces log-spaced band magnitudes in 0..1.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// FFT window length in samples.
pub const WINDOW: usize = 2048;
/// Hop between analyses.
const HOP: usize = 1024;
const F_LO: f32 = 40.0;
const F_HI: f32 = 16_000.0;
const DB_FLOOR: f32 = -78.0;

pub struct Analyzer {
    sample_rate: f32,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    ring: Vec<f32>,
    ring_pos: usize,
    since_last: usize,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Bin ranges `[lo, hi)` per band.
    band_bins: Vec<(usize, usize)>,
    bands: Vec<f32>,
    raw: Vec<f32>,
}

impl std::fmt::Debug for Analyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Analyzer").field("sample_rate", &self.sample_rate).field("bands", &self.bands.len()).finish()
    }
}

impl Analyzer {
    /// `bands` log-spaced bands between 40 Hz and 16 kHz (typically 48).
    pub fn new(sample_rate: f32, bands: usize) -> Self {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 1000.0 { sample_rate } else { 48_000.0 };
        let bands = bands.clamp(1, WINDOW / 2);
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(WINDOW);
        let scratch_len = fft.get_inplace_scratch_len();
        let window: Vec<f32> = (0..WINDOW)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WINDOW as f32).cos())
            .collect();
        let bin_hz = sample_rate / WINDOW as f32;
        let hi = F_HI.min(sample_rate * 0.49);
        let mut band_bins = Vec::with_capacity(bands);
        for b in 0..bands {
            let f0 = F_LO * (hi / F_LO).powf(b as f32 / bands as f32);
            let f1 = F_LO * (hi / F_LO).powf((b + 1) as f32 / bands as f32);
            let lo = ((f0 / bin_hz).floor() as usize).clamp(1, WINDOW / 2 - 1);
            let mut hi_bin = ((f1 / bin_hz).floor() as usize).clamp(1, WINDOW / 2);
            if hi_bin <= lo {
                hi_bin = lo + 1;
            }
            band_bins.push((lo, hi_bin));
        }
        Self {
            sample_rate,
            fft,
            window,
            ring: vec![0.0; WINDOW],
            ring_pos: 0,
            since_last: 0,
            buf: vec![Complex::default(); WINDOW],
            scratch: vec![Complex::default(); scratch_len],
            band_bins,
            bands: vec![0.0; bands],
            raw: vec![0.0; bands],
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Feed interleaved stereo samples. Runs an FFT every 1024 mono samples.
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            let m = (frame[0] + frame[1]) * 0.5;
            self.ring[self.ring_pos] = if m.is_finite() { m } else { 0.0 };
            self.ring_pos = (self.ring_pos + 1) % WINDOW;
            self.since_last += 1;
            if self.since_last >= HOP {
                self.since_last = 0;
                self.analyze();
            }
        }
    }

    fn analyze(&mut self) {
        for i in 0..WINDOW {
            let s = self.ring[(self.ring_pos + i) % WINDOW];
            self.buf[i] = Complex::new(s * self.window[i], 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        // Hann window coherent gain is 0.5; a full-scale sine gives |X| = N/4.
        let norm = 4.0 / WINDOW as f32;
        for (b, &(lo, hi)) in self.band_bins.iter().enumerate() {
            let mut peak = 0f32;
            for k in lo..hi {
                peak = peak.max(self.buf[k].norm() * norm);
            }
            let db = 20.0 * peak.max(1e-9).log10();
            let v = ((db - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);
            self.raw[b] = v;
            // Fast attack, slow release.
            let prev = self.bands[b];
            self.bands[b] = if v > prev { prev + 0.7 * (v - prev) } else { prev + 0.2 * (v - prev) };
        }
    }

    /// Smoothed band magnitudes 0..1, low to high frequency.
    pub fn bands(&self) -> &[f32] {
        &self.bands
    }

    /// Centre frequency of band `i` in Hz.
    pub fn band_center_hz(&self, i: usize) -> f32 {
        let n = self.bands.len() as f32;
        let hi = F_HI.min(self.sample_rate * 0.49);
        F_LO * (hi / F_LO).powf((i as f32 + 0.5) / n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_lands_in_the_right_band() {
        let sr = 48_000.0;
        let mut a = Analyzer::new(sr, 48);
        let f = 1000.0;
        let mut buf = vec![0.0; 2 * 8192];
        for i in 0..8192 {
            let s = (std::f32::consts::TAU * f * i as f32 / sr).sin();
            buf[i * 2] = s;
            buf[i * 2 + 1] = s;
        }
        a.push(&buf);
        let bands = a.bands();
        let (best, &val) = bands.iter().enumerate().max_by(|x, y| x.1.partial_cmp(y.1).unwrap()).unwrap();
        let fc = a.band_center_hz(best);
        assert!((fc / f).log2().abs() < 0.3, "best band {best} at {fc} Hz");
        assert!(val > 0.7, "{val}");
        assert!(bands[0] < 0.3, "{}", bands[0]);
    }
}
