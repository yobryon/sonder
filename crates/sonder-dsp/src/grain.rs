//! Granular live-audio instrument ([`sonder_core::Wave::Grain`]).
//!
//! Each layer slot owns an [`AudioRing`]: a preallocated mono ring that
//! `SynthCommand::AudioChunk` copies into (resampling linearly when the chunk's
//! rate differs from the synth rate). Every voice playing a grain oscillator owns
//! a [`GrainPlayer`] that schedules short Hann-windowed grains reading back into
//! the ring at a playback rate derived from the note and the ring's estimated
//! source pitch. Nothing here allocates after construction.

use sonder_core::Grain;

use crate::math::{pan_gains, sin_turns, Rng};

/// Longest ring any layer gets, seconds (the `ring_seconds` setting is capped here).
pub const MAX_RING_SECONDS: f32 = 8.0;
/// Simultaneous grains per voice.
pub const MAX_GRAINS: usize = 8;
/// Pitch estimate when the ring is empty or unvoiced, Hz.
pub const FALLBACK_PITCH_HZ: f32 = 220.0;
/// Samples examined by the pitch estimator (newest audio).
const PITCH_WINDOW: usize = 2048;
/// Pitch estimates outside this range are treated as unvoiced.
const PITCH_MIN_HZ: f32 = 40.0;
const PITCH_MAX_HZ: f32 = 2000.0;
/// RMS below which the pitch estimator holds its previous value.
const PITCH_GATE: f32 = 1e-3;

/// Preallocated mono ring of live audio for one layer.
#[derive(Debug, Clone)]
pub struct AudioRing {
    buf: Vec<f32>,
    /// Total samples ever written (the ring index is `written % buf.len()`).
    written: u64,
    /// Samples written since the last pitch update.
    since_pitch: u64,
    pitch_hz: f32,
    voiced: bool,
    sample_rate: f32,
    /// Fractional source position carried between resampled chunks.
    resample_frac: f32,
}

impl AudioRing {
    /// A ring holding [`MAX_RING_SECONDS`] at `sample_rate`.
    pub fn new(sample_rate: f32) -> Self {
        let n = ((MAX_RING_SECONDS * sample_rate) as usize).max(1024);
        Self { buf: vec![0.0; n], written: 0, since_pitch: 0, pitch_hz: FALLBACK_PITCH_HZ, voiced: false, sample_rate, resample_frac: 0.0 }
    }

    /// Ring capacity in samples.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Samples currently valid (≤ capacity).
    pub fn filled(&self) -> usize {
        (self.written.min(self.buf.len() as u64)) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    /// Absolute index one past the newest sample.
    pub fn head(&self) -> u64 {
        self.written
    }

    /// Current source pitch estimate, Hz (falls back to 220 Hz when unvoiced).
    pub fn pitch_hz(&self) -> f32 {
        if self.voiced {
            self.pitch_hz
        } else {
            FALLBACK_PITCH_HZ
        }
    }

    #[inline]
    fn push(&mut self, s: f32) {
        let n = self.buf.len();
        let i = (self.written % n as u64) as usize;
        self.buf[i] = if s.is_finite() { s.clamp(-4.0, 4.0) } else { 0.0 };
        self.written += 1;
        self.since_pitch += 1;
    }

    /// Append `samples` recorded at `rate` Hz, resampling linearly to the ring rate.
    pub fn write(&mut self, rate: u32, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let rate = rate as f32;
        if !(rate.is_finite() && rate > 0.0) || (rate - self.sample_rate).abs() < 0.5 {
            for &s in samples {
                self.push(s);
            }
            self.resample_frac = 0.0;
            return;
        }
        let step = rate / self.sample_rate;
        let mut pos = self.resample_frac;
        let last = samples.len() as f32 - 1.0;
        while pos <= last {
            let i = pos as usize;
            let frac = pos - i as f32;
            let a = samples[i];
            let b = samples[(i + 1).min(samples.len() - 1)];
            self.push(a + (b - a) * frac);
            pos += step;
        }
        self.resample_frac = (pos - samples.len() as f32).max(0.0);
    }

    /// Linear-interpolated read at absolute sample position `pos` (0 when outside the
    /// valid region).
    #[inline]
    pub fn read(&self, pos: f64) -> f32 {
        let n = self.buf.len();
        let oldest = self.written.saturating_sub(n as u64) as f64;
        if pos < oldest || pos + 1.0 >= self.written as f64 {
            return 0.0;
        }
        let ip = pos as u64;
        let frac = (pos - ip as f64) as f32;
        let i0 = (ip % n as u64) as usize;
        let i1 = if i0 + 1 == n { 0 } else { i0 + 1 };
        let a = self.buf[i0];
        let b = self.buf[i1];
        a + (b - a) * frac
    }

    /// Re-estimate the source pitch from the newest audio if any arrived since the
    /// last call. Cheap: a one-pole low-pass followed by positive zero-crossing
    /// counting over [`PITCH_WINDOW`] samples.
    pub fn update_pitch(&mut self) {
        if self.since_pitch == 0 || self.written == 0 {
            return;
        }
        self.since_pitch = 0;
        let n = self.buf.len();
        let win = PITCH_WINDOW.min(self.filled());
        if win < 64 {
            return;
        }
        let start = self.written - win as u64;
        // Low-pass around 1.5 kHz so noise does not inflate the crossing count.
        let coef = 1.0 - (-std::f32::consts::TAU * 1500.0 / self.sample_rate).exp();
        let mut lp = 0.0f32;
        let mut prev = 0.0f32;
        let mut crossings = 0u32;
        let mut first: Option<usize> = None;
        let mut last = 0usize;
        let mut sq = 0.0f32;
        for k in 0..win {
            let s = self.buf[((start + k as u64) % n as u64) as usize];
            sq += s * s;
            lp += coef * (s - lp);
            if k > 0 && prev <= 0.0 && lp > 0.0 {
                if first.is_none() {
                    first = Some(k);
                }
                last = k;
                crossings += 1;
            }
            prev = lp;
        }
        let rms = (sq / win as f32).sqrt();
        if rms < PITCH_GATE || crossings < 2 {
            // Keep the previous estimate but mark the ring unvoiced if it is silent.
            if rms < PITCH_GATE {
                self.voiced = false;
            }
            return;
        }
        let span = (last - first.unwrap_or(0)) as f32;
        if span <= 0.0 {
            return;
        }
        let hz = (crossings - 1) as f32 * self.sample_rate / span;
        if (PITCH_MIN_HZ..=PITCH_MAX_HZ).contains(&hz) {
            // Smooth across updates; snap when the estimate jumps far (new source).
            let ratio = hz / self.pitch_hz;
            self.pitch_hz = if self.voiced && (0.5..2.0).contains(&ratio) { self.pitch_hz + 0.3 * (hz - self.pitch_hz) } else { hz };
            self.voiced = true;
        }
    }

    /// Forget everything (panic).
    pub fn clear(&mut self) {
        self.written = 0;
        self.since_pitch = 0;
        self.voiced = false;
        self.pitch_hz = FALLBACK_PITCH_HZ;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct GrainState {
    active: bool,
    /// Absolute read position in the ring, samples.
    pos: f64,
    /// Read increment per output sample.
    rate: f64,
    /// Elapsed / total output samples.
    age: u32,
    len: u32,
    gain_l: f32,
    gain_r: f32,
}

/// Per-voice grain scheduler and renderer.
#[derive(Debug, Clone)]
pub struct GrainPlayer {
    grains: [GrainState; MAX_GRAINS],
    rng: Rng,
    /// Spawn accumulator (grains owed).
    acc: f32,
    /// Per-block parameters.
    len_samples: u32,
    spray_samples: f32,
    rate: f64,
    jitter: f32,
    width: f32,
    norm: f32,
    sample_rate: f32,
    density: f32,
}

impl Default for GrainPlayer {
    fn default() -> Self {
        Self {
            grains: [GrainState::default(); MAX_GRAINS],
            rng: Rng::new(0xBEEF),
            acc: 0.0,
            len_samples: 1,
            spray_samples: 0.0,
            rate: 1.0,
            jitter: 0.0,
            width: 0.0,
            norm: 1.0,
            sample_rate: 48_000.0,
            density: 0.0,
        }
    }
}

impl GrainPlayer {
    pub fn reset(&mut self, seed: u32) {
        self.grains = [GrainState::default(); MAX_GRAINS];
        self.rng = Rng::new(seed);
        // Start with one grain owed so a note speaks immediately.
        self.acc = 1.0;
    }

    /// Resolve the block's parameters from the settings, the note and the ring.
    pub fn configure(&mut self, g: &Grain, note_hz: f32, ring: &AudioRing, sample_rate: f32) {
        self.sample_rate = sample_rate;
        let size_ms = if g.size_ms.is_finite() { g.size_ms.clamp(2.0, 2000.0) } else { 80.0 };
        self.len_samples = ((size_ms * 1e-3 * sample_rate) as u32).max(8);
        self.density = if g.density.is_finite() { g.density.clamp(0.0, 400.0) } else { 20.0 };
        let follow = if g.pitch_follow.is_finite() { g.pitch_follow.clamp(0.0, 1.0) } else { 1.0 };
        let src = ring.pitch_hz().max(1.0);
        let ratio = (note_hz.max(1.0) / src).clamp(0.05, 32.0);
        self.rate = (1.0 + (ratio - 1.0) * follow) as f64;
        self.jitter = if g.jitter_cents.is_finite() { g.jitter_cents.clamp(0.0, 2400.0) } else { 0.0 };
        self.width = if g.width.is_finite() { g.width.clamp(0.0, 1.0) } else { 0.5 };
        // Usable history: the configured reach, within the ring's capacity and fill.
        let reach_s = if g.ring_seconds.is_finite() { g.ring_seconds.clamp(0.05, MAX_RING_SECONDS) } else { 4.0 };
        let reach = (reach_s * sample_rate).min(ring.filled() as f32 * 0.95);
        let spray_s = if g.spray.is_finite() { g.spray.clamp(0.0, MAX_RING_SECONDS) } else { 0.5 };
        let span = self.len_samples as f32 * self.rate as f32;
        self.spray_samples = (spray_s * sample_rate).min((reach - span).max(0.0));
        // Keep the overlap-add level roughly independent of density × size.
        let overlap = (self.density * size_ms * 1e-3).max(1.0);
        self.norm = 1.0 / overlap.sqrt();
    }

    fn spawn(&mut self, ring: &AudioRing) {
        let Some(slot) = self.grains.iter().position(|g| !g.active) else { return };
        let mut rate = self.rate;
        if self.jitter > 0.0 {
            rate *= 2f64.powf((self.rng.bipolar() * self.jitter / 1200.0) as f64);
        }
        let span = self.len_samples as f64 * rate;
        let newest = ring.head() as f64 - 2.0;
        let back = self.rng.unipolar() * self.spray_samples;
        let mut start = newest - span - back as f64;
        let oldest = ring.head().saturating_sub(ring.capacity() as u64) as f64;
        if start < oldest {
            start = oldest;
        }
        if start + span > newest {
            // Grain would overrun the newest sample: play it slower from what exists.
            let avail = (newest - start).max(1.0);
            rate = (rate * avail / span).max(0.01);
        }
        let pan = 0.5 + self.rng.bipolar() * 0.5 * self.width;
        let (gl, gr) = pan_gains(pan);
        self.grains[slot] = GrainState { active: true, pos: start, rate, age: 0, len: self.len_samples, gain_l: gl * self.norm, gain_r: gr * self.norm };
    }

    /// Render one output sample: schedule, then overlap-add every live grain.
    #[inline]
    pub fn tick(&mut self, ring: &AudioRing) -> (f32, f32) {
        if ring.is_empty() {
            return (0.0, 0.0);
        }
        self.acc += self.density / self.sample_rate;
        if self.acc >= 1.0 {
            self.acc -= 1.0;
            if self.acc > 2.0 {
                self.acc = 0.0;
            }
            self.spawn(ring);
        }
        let mut l = 0.0;
        let mut r = 0.0;
        for g in &mut self.grains {
            if !g.active {
                continue;
            }
            // Hann window = sin²(π · age / len).
            let w = sin_turns(g.age as f32 / (2.0 * g.len as f32));
            let s = ring.read(g.pos) * w * w;
            l += s * g.gain_l;
            r += s * g.gain_r;
            g.pos += g.rate;
            g.age += 1;
            if g.age >= g.len {
                g.active = false;
            }
        }
        (l, r)
    }

    pub fn active_grains(&self) -> usize {
        self.grains.iter().filter(|g| g.active).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, sr: f32, secs: f32) -> Vec<f32> {
        (0..(sr * secs) as usize).map(|i| (i as f32 / sr * hz * std::f32::consts::TAU).sin() * 0.5).collect()
    }

    #[test]
    fn pitch_estimate_tracks_a_sine() {
        let sr = 48_000.0;
        let mut ring = AudioRing::new(sr);
        ring.write(48_000, &sine(220.0, sr, 0.5));
        ring.update_pitch();
        assert!((ring.pitch_hz() - 220.0).abs() < 4.0, "{}", ring.pitch_hz());
        // Resampled from 24 kHz: the same tone must give the same pitch.
        let mut ring2 = AudioRing::new(sr);
        ring2.write(24_000, &sine(220.0, 24_000.0, 0.5));
        ring2.update_pitch();
        assert!((ring2.pitch_hz() - 220.0).abs() < 6.0, "{}", ring2.pitch_hz());
        assert!((ring2.filled() as f32 - 0.5 * sr).abs() < 4.0);
    }

    #[test]
    fn empty_ring_is_silent_and_grains_are_bounded() {
        let sr = 48_000.0;
        let mut ring = AudioRing::new(sr);
        let mut p = GrainPlayer::default();
        p.reset(1);
        p.configure(&Grain::default(), 440.0, &ring, sr);
        assert_eq!(p.tick(&ring), (0.0, 0.0));
        ring.write(48_000, &sine(220.0, sr, 1.0));
        ring.update_pitch();
        p.configure(&Grain { density: 200.0, size_ms: 100.0, ..Grain::default() }, 440.0, &ring, sr);
        let mut peak = 0f32;
        for _ in 0..48_000 {
            let (l, r) = p.tick(&ring);
            assert!(l.is_finite() && r.is_finite());
            peak = peak.max(l.abs()).max(r.abs());
            assert!(p.active_grains() <= MAX_GRAINS);
        }
        assert!(peak > 0.05 && peak < 2.0, "{peak}");
    }
}
