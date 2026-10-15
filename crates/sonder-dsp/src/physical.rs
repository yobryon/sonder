//! Physical models: a Karplus–Strong plucked string ([`sonder_core::Wave::String`])
//! and a modal resonator bank ([`sonder_core::Wave::Modal`]). Both are pitched by
//! the note, excited at note-on, and cheap enough for the full voice pool. The
//! string's delay line is allocated once per voice at construction.

use std::f32::consts::{PI, TAU};

use sonder_core::Model;

use crate::math::{lerp, sane, Rng};

/// Lowest string fundamental the delay line can hold, Hz.
const STRING_MIN_HZ: f32 = 25.0;
/// Largest number of resonators in a [`ModalBank`].
pub const MAX_MODES: usize = 32;

fn clamp01(x: f32, default: f32) -> f32 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        default
    }
}

/// Karplus–Strong string: noise burst → delay line (integer + first-order allpass
/// fractional delay) → loss/low-pass loop filter.
#[derive(Debug, Clone)]
pub struct KarplusStrong {
    buf: Vec<f32>,
    write: usize,
    /// Integer part of the loop delay.
    delay: usize,
    /// Allpass coefficient and state for the fractional part.
    ap_coef: f32,
    ap_x1: f32,
    ap_y1: f32,
    /// Loop filter: `y = loss · (h0·x + h1·x[n-1])`.
    h0: f32,
    h1: f32,
    loss: f32,
    lp_x1: f32,
    /// Remaining excitation samples to write.
    exc_left: usize,
    exc_len: usize,
    exc_lp: f32,
    exc_bright: f32,
    exc_comb_delay: usize,
    rng: Rng,
    sample_rate: f32,
    damping: f32,
    freq: f32,
}

impl KarplusStrong {
    pub fn new(sample_rate: f32) -> Self {
        let n = ((sample_rate / STRING_MIN_HZ) as usize + 8).max(64);
        Self {
            buf: vec![0.0; n],
            write: 0,
            delay: 64,
            ap_coef: 0.0,
            ap_x1: 0.0,
            ap_y1: 0.0,
            h0: 1.0,
            h1: 0.0,
            loss: 0.999,
            lp_x1: 0.0,
            exc_left: 0,
            exc_len: 0,
            exc_lp: 0.0,
            exc_bright: 1.0,
            exc_comb_delay: 0,
            rng: Rng::new(7),
            sample_rate,
            damping: 0.3,
            freq: 0.0,
        }
    }

    /// Excite the string (note-on). Clears the previous ring-out.
    pub fn trigger(&mut self, m: &Model, hz: f32, seed: u32) {
        self.buf.fill(0.0);
        self.ap_x1 = 0.0;
        self.ap_y1 = 0.0;
        self.lp_x1 = 0.0;
        self.exc_lp = 0.0;
        self.rng = Rng::new(seed);
        self.damping = clamp01(m.damping, 0.3);
        self.freq = 0.0;
        self.set_freq(hz);
        let bright = clamp01(m.brightness, 0.7);
        // One-pole low-pass on the burst: 0 → ~600 Hz, 1 → wide open.
        let cutoff = lerp(600.0, 16_000.0, bright * bright);
        self.exc_bright = 1.0 - (-TAU * cutoff / self.sample_rate).exp();
        self.exc_len = self.delay.max(2);
        self.exc_left = self.exc_len;
        let pos = clamp01(m.position, 0.2).clamp(0.02, 0.5);
        self.exc_comb_delay = ((pos * self.delay as f32) as usize).clamp(1, self.delay.saturating_sub(1).max(1));
    }

    /// Retune (per block). Keeps the loop delay = `sample_rate / hz` including the
    /// loop filter's phase delay so the pitch lands within a few cents.
    pub fn set_freq(&mut self, hz: f32) {
        let hz = sane(hz, STRING_MIN_HZ, self.sample_rate * 0.25);
        if (hz - self.freq).abs() < 1e-3 {
            return;
        }
        self.freq = hz;
        let period = self.sample_rate / hz;
        // Loop filter: blend of the input and a two-point average; damping controls
        // the blend and the per-period loss (shorter decay for higher damping).
        let d = lerp(0.15, 1.0, self.damping);
        self.h0 = 1.0 - d * 0.5;
        self.h1 = d * 0.5;
        let t60 = lerp(5.0, 0.15, self.damping * self.damping.sqrt());
        self.loss = 10f32.powf(-3.0 * period / (t60 * self.sample_rate));
        // Phase delay of the loop filter at the fundamental.
        let w = TAU / period;
        let lp_delay = (self.h1 * w.sin()).atan2(self.h0 + self.h1 * w.cos()) / w;
        let target = (period - lp_delay).max(2.5);
        // Allpass fractional part kept in [0.3, 1.3) for a well-behaved coefficient.
        let max_delay = self.buf.len() - 2;
        let mut n = (target - 0.3).floor() as usize;
        n = n.clamp(1, max_delay);
        let frac = (target - n as f32).clamp(0.05, 1.5);
        self.delay = n;
        self.ap_coef = (1.0 - frac) / (1.0 + frac);
    }

    /// One sample of string output.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        let n = self.buf.len();
        // Read the delayed sample (integer delay) then fractional allpass.
        let ri = (self.write + n - self.delay) % n;
        let x = self.buf[ri];
        let ap = self.ap_coef * (x - self.ap_y1) + self.ap_x1;
        self.ap_x1 = x;
        self.ap_y1 = ap;
        // Loop filter.
        let y = self.loss * (self.h0 * ap + self.h1 * self.lp_x1);
        self.lp_x1 = ap;
        let mut v = y;
        if self.exc_left > 0 {
            // Brightness-shaped noise burst with a pick-position comb.
            let white = self.rng.bipolar();
            self.exc_lp += self.exc_bright * (white - self.exc_lp);
            let comb_i = (self.write + n - self.exc_comb_delay) % n;
            let delayed = if self.exc_left + self.exc_comb_delay <= self.exc_len { self.buf[comb_i] } else { 0.0 };
            v += self.exc_lp - delayed * 0.9;
            self.exc_left -= 1;
        }
        let v = if v.is_finite() { v.clamp(-2.0, 2.0) } else { 0.0 };
        self.buf[self.write] = v;
        self.write += 1;
        if self.write >= n {
            self.write = 0;
        }
        v
    }
}

/// Bell-like partial ratios for `stiffness = 1` (prime = 1).
const BELL_RATIOS: [f32; 12] = [1.0, 1.2, 1.5, 2.0, 2.5, 2.67, 3.0, 3.33, 4.0, 4.5, 5.33, 6.0];

#[derive(Debug, Clone, Copy, Default)]
struct Mode {
    a1: f32,
    a2: f32,
    gain: f32,
    y1: f32,
    y2: f32,
}

/// Bank of two-pole resonators excited by a short burst.
#[derive(Debug, Clone)]
pub struct ModalBank {
    modes: [Mode; MAX_MODES],
    n: usize,
    ratios: [f32; MAX_MODES],
    decays: [f32; MAX_MODES],
    weights: [f32; MAX_MODES],
    exc_left: u32,
    exc_len: u32,
    exc_lp: f32,
    exc_coef: f32,
    rng: Rng,
    sample_rate: f32,
    freq: f32,
    norm: f32,
}

impl ModalBank {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            modes: [Mode::default(); MAX_MODES],
            n: 0,
            ratios: [1.0; MAX_MODES],
            decays: [1.0; MAX_MODES],
            weights: [0.0; MAX_MODES],
            exc_left: 0,
            exc_len: 1,
            exc_lp: 0.0,
            exc_coef: 1.0,
            rng: Rng::new(11),
            sample_rate,
            freq: 0.0,
            norm: 1.0,
        }
    }

    /// Strike (note-on): resolve the partial series, decays and gains, then excite.
    pub fn trigger(&mut self, m: &Model, hz: f32, seed: u32) {
        self.rng = Rng::new(seed);
        self.n = (m.modes as usize).clamp(1, MAX_MODES);
        let stiffness = clamp01(m.stiffness, 0.0);
        let damping = clamp01(m.damping, 0.3);
        let position = clamp01(m.position, 0.2).clamp(0.03, 0.97);
        let bright = clamp01(m.brightness, 0.7);
        let t60 = lerp(8.0, 0.08, damping.sqrt());
        let mut total = 0.0;
        for k in 0..self.n {
            let harmonic = (k + 1) as f32;
            // Past the table, continue the series with a stiff-bar-like stretch.
            let bell = BELL_RATIOS.get(k).copied().unwrap_or_else(|| harmonic * (1.0 + 0.04 * (k as f32 - 10.0)).max(1.0));
            self.ratios[k] = lerp(harmonic, bell, stiffness);
            // Higher modes die faster; brightness slows that roll-off a little.
            self.decays[k] = t60 / (1.0 + k as f32 * lerp(0.9, 0.3, bright));
            // Strike position: mode shape sin(πkp) with a floor so no mode fully vanishes.
            let shape = (PI * harmonic * position).sin().abs().max(0.12);
            let w = shape / (1.0 + k as f32 * lerp(0.6, 0.15, bright));
            self.weights[k] = w;
            total += w;
        }
        self.norm = if total > 0.0 { 0.9 / total } else { 0.0 };
        for mode in &mut self.modes {
            mode.y1 = 0.0;
            mode.y2 = 0.0;
        }
        self.freq = 0.0;
        self.set_freq(hz);
        // Excitation: 0.5–3 ms burst, darker for low brightness.
        self.exc_len = ((lerp(3.0, 0.5, bright) * 1e-3 * self.sample_rate) as u32).max(4);
        self.exc_left = self.exc_len;
        self.exc_lp = 0.0;
        let cutoff = lerp(800.0, 12_000.0, bright);
        self.exc_coef = 1.0 - (-TAU * cutoff / self.sample_rate).exp();
    }

    /// Retune (per block): recompute every resonator's coefficients.
    pub fn set_freq(&mut self, hz: f32) {
        let hz = sane(hz, 10.0, self.sample_rate * 0.45);
        if (hz - self.freq).abs() < 1e-3 {
            return;
        }
        self.freq = hz;
        let nyq = self.sample_rate * 0.45;
        for k in 0..self.n {
            let f = hz * self.ratios[k];
            let m = &mut self.modes[k];
            if f >= nyq {
                m.gain = 0.0;
                m.a1 = 0.0;
                m.a2 = 0.0;
                continue;
            }
            let theta = TAU * f / self.sample_rate;
            let r = (-6.9 / (self.decays[k] * self.sample_rate)).exp();
            m.a1 = 2.0 * r * theta.cos();
            m.a2 = -(r * r);
            // sin θ normalises the impulse response peak to ~1 regardless of pitch.
            m.gain = self.weights[k] * self.norm * theta.sin();
        }
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        let mut x = 0.0;
        if self.exc_left > 0 {
            let t = self.exc_left as f32 / self.exc_len as f32;
            let white = self.rng.bipolar();
            self.exc_lp += self.exc_coef * (white - self.exc_lp);
            // Mostly impulse, a little coloured noise, fading over the burst.
            x = (if self.exc_left == self.exc_len { 1.0 } else { 0.0 } + self.exc_lp * 0.5) * t * 4.0;
            self.exc_left -= 1;
        }
        let mut out = 0.0;
        for m in self.modes.iter_mut().take(self.n) {
            let y = m.a1 * m.y1 + m.a2 * m.y2 + x * m.gain;
            m.y2 = m.y1;
            m.y1 = y;
            out += y;
        }
        if out.is_finite() {
            out.clamp(-2.0, 2.0)
        } else {
            for m in &mut self.modes {
                m.y1 = 0.0;
                m.y2 = 0.0;
            }
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fundamental by normalised autocorrelation (parabolic peak interpolation),
    /// searching ±30 % around `guess_hz`.
    fn autocorr_freq(buf: &[f32], sr: f32, guess_hz: f32) -> f32 {
        let lo = (sr / (guess_hz * 1.3)) as usize;
        let hi = (sr / (guess_hz * 0.7)) as usize;
        let n = buf.len() - hi - 1;
        let corr = |lag: usize| -> f32 {
            let mut num = 0.0;
            let mut d0 = 0.0;
            let mut d1 = 0.0;
            for i in 0..n {
                num += buf[i] * buf[i + lag];
                d0 += buf[i] * buf[i];
                d1 += buf[i + lag] * buf[i + lag];
            }
            num / (d0 * d1).sqrt().max(1e-12)
        };
        let mut best = lo;
        let mut best_c = f32::MIN;
        for lag in lo..=hi {
            let c = corr(lag);
            if c > best_c {
                best_c = c;
                best = lag;
            }
        }
        let (cm, c0, cp) = (corr(best - 1), corr(best), corr(best + 1));
        let denom = cm - 2.0 * c0 + cp;
        let shift = if denom.abs() > 1e-9 { 0.5 * (cm - cp) / denom } else { 0.0 };
        sr / (best as f32 + shift)
    }

    #[test]
    fn string_pitch_within_one_percent() {
        let sr = 48_000.0;
        for hz in [110.0f32, 440.0, 880.0] {
            let mut ks = KarplusStrong::new(sr);
            ks.trigger(&Model { damping: 0.2, brightness: 0.5, ..Model::default() }, hz, 3);
            let n = (sr * 0.6) as usize;
            let out: Vec<f32> = (0..n).map(|_| ks.tick()).collect();
            assert!(out.iter().all(|s| s.is_finite()));
            // Skip the burst; measure the ring-out.
            let f = autocorr_freq(&out[n / 4..], sr, hz);
            assert!((f - hz).abs() / hz < 0.01, "{hz} Hz string measured {f}");
        }
    }

    #[test]
    fn modal_decays_and_stays_finite() {
        let sr = 48_000.0;
        for stiffness in [0.0f32, 1.0] {
            let mut bank = ModalBank::new(sr);
            bank.trigger(&Model { stiffness, modes: 32, damping: 0.4, ..Model::default() }, 330.0, 5);
            let n = (sr * 3.0) as usize;
            let out: Vec<f32> = (0..n).map(|_| bank.tick()).collect();
            assert!(out.iter().all(|s| s.is_finite()));
            let p = |s: &[f32]| s.iter().fold(0f32, |m, x| m.max(x.abs()));
            let early = p(&out[..n / 10]);
            let late = p(&out[n * 9 / 10..]);
            assert!(early > 0.05 && early <= 2.0, "stiffness {stiffness}: onset peak {early}");
            assert!(late < early * 0.1, "stiffness {stiffness}: did not decay ({early} → {late})");
        }
    }
}
