//! Master chain: loudness rider → bus "glue" compressor → gain → soft saturation
//! → peak limiter → hard clip, with a K-weighted-ish short-term loudness meter
//! on the output that drives the rider.

use sonder_core::MasterSettings;

use crate::math::{db_to_lin, lin_to_db, onepole_coef, sane, saturate};

/// Loudness integration window, seconds.
const LUFS_WINDOW_SECONDS: f32 = 3.0;
/// Loudness bins per second (each bin is a mean-square over 10 ms).
const LUFS_BINS_PER_SECOND: usize = 100;
/// Rider speed limit, dB per second.
const RIDER_MAX_DB_PER_SECOND: f32 = 0.5;
/// Rider gain range, dB.
const RIDER_RANGE_DB: f32 = 12.0;
/// Below this short-term loudness the rider holds (silence / fade-outs).
const RIDER_GATE_LUFS: f32 = -50.0;
/// Rider dead band, dB.
const RIDER_DEADBAND_DB: f32 = 0.1;
/// Glue compressor threshold, dBFS (before makeup).
const GLUE_THRESHOLD_DB: f32 = -18.0;
const GLUE_KNEE_DB: f32 = 6.0;
const GLUE_RATIO: f32 = 2.0;
const GLUE_ATTACK_SECONDS: f32 = 0.010;
const GLUE_RELEASE_SECONDS: f32 = 0.300;
/// Reported loudness when nothing has been measured yet.
pub const SILENCE_LUFS: f32 = -100.0;

/// Second-order IIR (transposed direct form II), one channel.
#[derive(Debug, Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Butterworth high-pass.
    fn highpass(fc: f32, sr: f32) -> Self {
        let k = (std::f32::consts::PI * fc / sr).tan();
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let norm = 1.0 / (1.0 + k / q + k * k);
        Self { b0: norm, b1: -2.0 * norm, b2: norm, a1: 2.0 * (k * k - 1.0) * norm, a2: (1.0 - k / q + k * k) * norm, z1: 0.0, z2: 0.0 }
    }

    /// High shelf of `gain_db` above `fc` (RBJ).
    fn highshelf(fc: f32, gain_db: f32, sr: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = std::f32::consts::TAU * fc / sr;
        let (sn, cs) = w0.sin_cos();
        let alpha = sn / 2.0 * std::f32::consts::SQRT_2;
        let sa = 2.0 * a.sqrt() * alpha;
        let b0 = a * ((a + 1.0) + (a - 1.0) * cs + sa);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cs);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cs - sa);
        let a0 = (a + 1.0) - (a - 1.0) * cs + sa;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cs);
        let a2 = (a + 1.0) - (a - 1.0) * cs - sa;
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: 0.0, z2: 0.0 }
    }
}

/// Short-term (3 s) K-weighted-ish loudness meter.
#[derive(Debug, Clone)]
struct LoudnessMeter {
    shelf: [Biquad; 2],
    hp: [Biquad; 2],
    bins: Vec<f32>,
    bin_pos: usize,
    bin_sum: f32,
    bin_count: usize,
    bin_len: usize,
    total: f32,
    filled: usize,
}

impl LoudnessMeter {
    fn new(sr: f32) -> Self {
        let shelf = Biquad::highshelf(1500.0, 4.0, sr);
        let hp = Biquad::highpass(60.0, sr);
        let n = (LUFS_WINDOW_SECONDS * LUFS_BINS_PER_SECOND as f32) as usize;
        Self {
            shelf: [shelf; 2],
            hp: [hp; 2],
            bins: vec![0.0; n],
            bin_pos: 0,
            bin_sum: 0.0,
            bin_count: 0,
            bin_len: ((sr / LUFS_BINS_PER_SECOND as f32) as usize).max(1),
            total: 0.0,
            filled: 0,
        }
    }

    /// Feed one stereo frame; returns `true` when a 10 ms bin completed.
    #[inline]
    fn push(&mut self, l: f32, r: f32) -> bool {
        let l = self.hp[0].process(self.shelf[0].process(l));
        let r = self.hp[1].process(self.shelf[1].process(r));
        self.bin_sum += l * l + r * r;
        self.bin_count += 1;
        if self.bin_count < self.bin_len {
            return false;
        }
        let ms = self.bin_sum / self.bin_count as f32;
        self.bin_sum = 0.0;
        self.bin_count = 0;
        self.total += ms - self.bins[self.bin_pos];
        self.bins[self.bin_pos] = ms;
        self.bin_pos = (self.bin_pos + 1) % self.bins.len();
        self.filled = (self.filled + 1).min(self.bins.len());
        if self.total < 0.0 {
            self.total = 0.0;
        }
        true
    }

    fn lufs(&self) -> f32 {
        if self.filled == 0 {
            return SILENCE_LUFS;
        }
        let ms = self.total / self.filled as f32;
        if ms <= 1e-12 {
            SILENCE_LUFS
        } else {
            (-0.691 + 10.0 * ms.log10()).max(SILENCE_LUFS)
        }
    }

    fn clear(&mut self) {
        self.bins.fill(0.0);
        self.total = 0.0;
        self.filled = 0;
        self.bin_sum = 0.0;
        self.bin_count = 0;
    }
}

#[derive(Debug, Clone)]
pub struct MasterChain {
    gain: f32,
    saturation: f32,
    ceiling: f32,
    env: f32,
    release_coef: f32,
    // Glue compressor.
    glue: f32,
    glue_env: f32,
    glue_attack: f32,
    glue_release: f32,
    glue_makeup: f32,
    // Loudness rider.
    target_lufs: Option<f32>,
    rider_db: f32,
    rider_step_db: f32,
    meter: LoudnessMeter,
}

impl MasterChain {
    pub fn new(sample_rate: f32) -> Self {
        let mut m = Self {
            gain: 0.9,
            saturation: 0.1,
            ceiling: db_to_lin(-1.0),
            env: 0.0,
            release_coef: onepole_coef(0.15, sample_rate),
            glue: 0.0,
            glue_env: 0.0,
            glue_attack: onepole_coef(GLUE_ATTACK_SECONDS, sample_rate),
            glue_release: onepole_coef(GLUE_RELEASE_SECONDS, sample_rate),
            glue_makeup: 1.0,
            target_lufs: None,
            rider_db: 0.0,
            rider_step_db: RIDER_MAX_DB_PER_SECOND / LUFS_BINS_PER_SECOND as f32,
            meter: LoudnessMeter::new(sample_rate),
        };
        m.configure(&MasterSettings::default());
        m
    }

    pub fn configure(&mut self, s: &MasterSettings) {
        self.gain = sane(s.gain, 0.0, 4.0);
        self.saturation = sane(s.saturation, 0.0, 1.0);
        self.ceiling = db_to_lin(sane(s.ceiling_db, -30.0, 0.0));
        self.glue = sane(s.glue, 0.0, 1.0);
        // Makeup: a 2:1 bus compressor at −18 dB takes ~4 dB off a loud mix.
        self.glue_makeup = db_to_lin(4.0 * self.glue);
        let target = s.target_lufs.filter(|t| t.is_finite()).map(|t| t.clamp(-40.0, -6.0));
        if target.is_none() {
            self.rider_db = 0.0;
        }
        self.target_lufs = target;
    }

    /// Short-term loudness of the output, LUFS ([`SILENCE_LUFS`] when silent).
    pub fn lufs(&self) -> f32 {
        self.meter.lufs()
    }

    /// Current rider gain, dB.
    pub fn rider_db(&self) -> f32 {
        self.rider_db
    }

    /// Current glue gain reduction, dB (≥ 0).
    pub fn glue_reduction_db(&self) -> f32 {
        if self.glue <= 0.0 {
            return 0.0;
        }
        let over = lin_to_db(self.glue_env.max(1e-9)) - GLUE_THRESHOLD_DB;
        Self::knee_reduction(over) * self.glue
    }

    /// Soft-knee gain reduction in dB for a level `over` dB above the threshold.
    #[inline]
    fn knee_reduction(over: f32) -> f32 {
        let half = GLUE_KNEE_DB * 0.5;
        if over <= -half {
            0.0
        } else if over >= half {
            over * (1.0 - 1.0 / GLUE_RATIO)
        } else {
            let x = over + half;
            (1.0 - 1.0 / GLUE_RATIO) * x * x / (2.0 * GLUE_KNEE_DB)
        }
    }

    /// Reset meters and dynamics state (panic).
    pub fn clear(&mut self) {
        self.env = 0.0;
        self.glue_env = 0.0;
        self.meter.clear();
    }

    /// Process interleaved stereo in place. Never lets |x| exceed the ceiling.
    pub fn process(&mut self, buf: &mut [f32]) {
        let n = buf.len() / 2;
        let rider = db_to_lin(self.rider_db);
        let glue_on = self.glue > 0.0;
        for i in 0..n {
            let mut l = buf[i * 2] * rider;
            let mut r = buf[i * 2 + 1] * rider;
            if glue_on {
                // Stereo-linked peak detector with 10 ms attack / 300 ms release.
                let peak = l.abs().max(r.abs());
                let coef = if peak > self.glue_env { self.glue_attack } else { self.glue_release };
                self.glue_env += coef * (peak - self.glue_env);
                let over = lin_to_db(self.glue_env.max(1e-6)) - GLUE_THRESHOLD_DB;
                let gr_db = Self::knee_reduction(over) * self.glue;
                let g = db_to_lin(-gr_db) * self.glue_makeup;
                l *= g;
                r *= g;
            }
            l *= self.gain;
            r *= self.gain;
            if self.saturation > 0.0 {
                l = saturate(l, self.saturation);
                r = saturate(r, self.saturation);
            }
            // Peak follower: instant attack toward the current peak, slow release.
            let peak = l.abs().max(r.abs());
            if peak > self.env {
                // Instant attack: no lookahead, so the gain must react on this very sample.
                self.env = peak;
            } else {
                self.env += self.release_coef * (peak - self.env);
            }
            let g = if self.env > self.ceiling { self.ceiling / self.env } else { 1.0 };
            l = (l * g).clamp(-1.0, 1.0);
            r = (r * g).clamp(-1.0, 1.0);
            if !l.is_finite() {
                l = 0.0;
            }
            if !r.is_finite() {
                r = 0.0;
            }
            buf[i * 2] = l;
            buf[i * 2 + 1] = r;
            if self.meter.push(l, r) {
                self.ride();
            }
        }
    }

    /// One rider step (every 10 ms): move the gain toward the target loudness.
    fn ride(&mut self) {
        let Some(target) = self.target_lufs else { return };
        let lufs = self.meter.lufs();
        if lufs < RIDER_GATE_LUFS || self.meter.filled < self.meter.bins.len() / 3 {
            return;
        }
        let err = target - lufs;
        if err.abs() < RIDER_DEADBAND_DB {
            return;
        }
        let step = err.clamp(-self.rider_step_db, self.rider_step_db);
        self.rider_db = (self.rider_db + step).clamp(-RIDER_RANGE_DB, RIDER_RANGE_DB);
    }
}
