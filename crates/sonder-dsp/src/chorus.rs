//! Per-layer stereo chorus: two or three modulated delay taps (5–25 ms) with
//! LFO phases spread evenly around the cycle and the right channel offset by a
//! quarter turn for width. Cubic (Hermite) interpolation, block-rate parameter
//! smoothing, and a true bypass when depth is zero.

use sonder_core::Chorus as ChorusCfg;

use crate::math::{onepole_coef, sane, sin_turns};
use crate::BLOCK;

/// Centre delay of every tap, ms.
const CENTER_MS: f32 = 15.0;
/// Maximum modulation swing around the centre at depth 1, ms.
const SWING_MS: f32 = 10.0;
/// Buffer headroom in ms beyond centre + swing (interpolation + rounding).
const HEADROOM_MS: f32 = 3.0;
const MAX_TAPS: usize = 3;
/// Depth below which the wet signal is faded out so bypass engages without a click.
const FADE_DEPTH: f32 = 0.05;

#[derive(Debug, Clone, Copy)]
struct Smoothed {
    cur: f32,
    target: f32,
}

impl Smoothed {
    fn new(v: f32) -> Self {
        Self { cur: v, target: v }
    }
    #[inline]
    fn step(&mut self, coef: f32) -> f32 {
        self.cur += coef * (self.target - self.cur);
        if (self.cur - self.target).abs() < 1e-6 {
            self.cur = self.target;
        }
        self.cur
    }
}

#[derive(Debug)]
pub struct Chorus {
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    sr: f32,
    depth: Smoothed,
    rate_hz: Smoothed,
    mix: Smoothed,
    voices: usize,
    /// LFO phase in turns.
    phase: f32,
    /// Previous block's per-tap delays (samples) for L/R, for linear ramping.
    prev_delay: [[f32; MAX_TAPS]; 2],
    /// Previous block's wet gain for ramping.
    prev_wet: f32,
    prev_dry: f32,
    /// Buffer holds signal; cleared on the way into bypass.
    dirty: bool,
    smooth_coef: f32,
}

impl Chorus {
    pub fn new(sample_rate: f32) -> Self {
        let sr = if sample_rate.is_finite() { sample_rate.clamp(8000.0, 384_000.0) } else { 48_000.0 };
        let len = ((CENTER_MS + SWING_MS + HEADROOM_MS) * 0.001 * sr) as usize + 4;
        let mut c = Self {
            buf_l: vec![0.0; len],
            buf_r: vec![0.0; len],
            write: 0,
            sr,
            depth: Smoothed::new(0.0),
            rate_hz: Smoothed::new(0.6),
            mix: Smoothed::new(0.5),
            voices: 2,
            phase: 0.0,
            prev_delay: [[0.0; MAX_TAPS]; 2],
            prev_wet: 0.0,
            prev_dry: 1.0,
            dirty: false,
            smooth_coef: onepole_coef(0.05, sr / BLOCK as f32),
        };
        c.configure(&ChorusCfg::default());
        c.snap();
        c
    }

    /// Set targets from an instrument's chorus settings (smoothed from here).
    pub fn configure(&mut self, cfg: &ChorusCfg) {
        self.depth.target = sane(cfg.depth, 0.0, 1.0);
        self.rate_hz.target = sane(cfg.rate_hz, 0.1, 5.0);
        self.mix.target = sane(cfg.mix, 0.0, 1.0);
        self.voices = (cfg.voices as usize).clamp(2, MAX_TAPS);
    }

    /// Jump smoothed values to their targets (used at construction).
    fn snap(&mut self) {
        self.depth.cur = self.depth.target;
        self.rate_hz.cur = self.rate_hz.target;
        self.mix.cur = self.mix.target;
        let (w, d) = self.gains(self.depth.cur, self.mix.cur);
        self.prev_wet = w;
        self.prev_dry = d;
        for c in 0..2 {
            for t in 0..MAX_TAPS {
                self.prev_delay[c][t] = self.tap_delay(c, t, self.depth.cur);
            }
        }
    }

    /// True when the effect is fully bypassed (nothing to process).
    #[inline]
    pub fn is_bypassed(&self) -> bool {
        self.depth.target <= 0.0 && self.depth.cur <= 0.0 && !self.dirty
    }

    pub fn clear(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.dirty = false;
    }

    #[inline]
    fn gains(&self, depth: f32, mix: f32) -> (f32, f32) {
        let fade = (depth / FADE_DEPTH).clamp(0.0, 1.0);
        let wet = mix * fade;
        (wet, 1.0 - wet)
    }

    /// Delay in samples for tap `tap` on channel `ch` at the current LFO phase.
    #[inline]
    fn tap_delay(&self, ch: usize, tap: usize, depth: f32) -> f32 {
        let offset = tap as f32 / self.voices as f32 + if ch == 1 { 0.25 } else { 0.0 };
        let lfo = sin_turns(self.phase + offset);
        let ms = CENTER_MS + SWING_MS * depth * lfo;
        ms * 0.001 * self.sr
    }

    /// 4-point Hermite read `delay` samples behind the write head.
    #[inline]
    fn read(buf: &[f32], write: usize, delay: f32) -> f32 {
        let len = buf.len();
        let d = delay.clamp(2.0, (len - 3) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        // y1 is `di` samples back, y0 one newer, y2/y3 older.
        let i1 = (write + len - di) % len;
        let i0 = (i1 + 1) % len;
        let i2 = (i1 + len - 1) % len;
        let i3 = (i1 + len - 2) % len;
        let (y0, y1, y2, y3) = (buf[i0], buf[i1], buf[i2], buf[i3]);
        let c0 = y1;
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * frac + c2) * frac + c1) * frac + c0
    }

    /// Process one block of interleaved stereo in place. Does nothing when bypassed.
    pub fn process(&mut self, bus: &mut [f32]) {
        if self.is_bypassed() {
            return;
        }
        let frames = (bus.len() / 2).min(BLOCK);
        if frames == 0 {
            return;
        }
        let coef = self.smooth_coef;
        let depth = self.depth.step(coef);
        let rate = self.rate_hz.step(coef);
        let mix = self.mix.step(coef);

        // Heading into bypass with the buffer empty of anything audible: stop.
        if depth <= 0.0 && self.depth.target <= 0.0 {
            self.clear();
            self.prev_wet = 0.0;
            self.prev_dry = 1.0;
            return;
        }

        // Advance the LFO once per block; delays ramp linearly across the block.
        self.phase += rate * frames as f32 / self.sr;
        if self.phase >= 1.0 {
            self.phase -= self.phase.floor();
        }
        let n = self.voices;
        let mut end_delay = [[0.0f32; MAX_TAPS]; 2];
        for (c, row) in end_delay.iter_mut().enumerate() {
            for (t, d) in row.iter_mut().enumerate().take(n) {
                *d = self.tap_delay(c, t, depth);
            }
        }
        let (wet_end, dry_end) = self.gains(depth, mix);
        let wet_start = self.prev_wet;
        let dry_start = self.prev_dry;
        let start_delay = self.prev_delay;
        self.prev_wet = wet_end;
        self.prev_dry = dry_end;
        self.prev_delay = end_delay;
        let tap_norm = 1.0 / n as f32;
        let len = self.buf_l.len();
        let inv = 1.0 / frames as f32;

        for i in 0..frames {
            let t = i as f32 * inv;
            let in_l = bus[i * 2];
            let in_r = bus[i * 2 + 1];
            self.buf_l[self.write] = sane(in_l, -8.0, 8.0);
            self.buf_r[self.write] = sane(in_r, -8.0, 8.0);
            let mut wl = 0.0;
            let mut wr = 0.0;
            for tap in 0..n {
                let dl = start_delay[0][tap] + (end_delay[0][tap] - start_delay[0][tap]) * t;
                let dr = start_delay[1][tap] + (end_delay[1][tap] - start_delay[1][tap]) * t;
                wl += Self::read(&self.buf_l, self.write, dl);
                wr += Self::read(&self.buf_r, self.write, dr);
            }
            self.write = (self.write + 1) % len;
            let wet = wet_start + (wet_end - wet_start) * t;
            let dry = dry_start + (dry_end - dry_start) * t;
            bus[i * 2] = in_l * dry + wl * tap_norm * wet;
            bus[i * 2 + 1] = in_r * dry + wr * tap_norm * wet;
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signal(seed: u32) -> Vec<f32> {
        let mut rng = crate::math::Rng::new(seed);
        (0..BLOCK * 2).map(|_| rng.bipolar()).collect()
    }

    #[test]
    fn bypass_is_bit_identical() {
        let mut c = Chorus::new(48_000.0);
        assert!(c.is_bypassed());
        for b in 0..50 {
            let input = test_signal(b + 1);
            let mut bus = input.clone();
            c.process(&mut bus);
            assert_eq!(bus, input, "block {b}");
        }
    }

    #[test]
    fn active_chorus_changes_signal_finitely_and_returns_to_bypass() {
        let mut c = Chorus::new(48_000.0);
        c.configure(&ChorusCfg { depth: 0.5, rate_hz: 1.0, mix: 0.5, voices: 3 });
        assert!(!c.is_bypassed());
        let mut changed = false;
        for b in 0..400 {
            let input = test_signal(b + 7);
            let mut bus = input.clone();
            c.process(&mut bus);
            assert!(bus.iter().all(|s| s.is_finite()));
            if bus != input {
                changed = true;
            }
        }
        assert!(changed);
        // Turn it off: after the fade the effect is a true bypass again.
        c.configure(&ChorusCfg::default());
        for _ in 0..4000 {
            let mut bus = test_signal(3);
            c.process(&mut bus);
        }
        assert!(c.is_bypassed());
        let input = test_signal(99);
        let mut bus = input.clone();
        c.process(&mut bus);
        assert_eq!(bus, input);
    }

    #[test]
    fn silence_in_silence_out_and_level_preserved() {
        let mut c = Chorus::new(48_000.0);
        c.configure(&ChorusCfg { depth: 1.0, rate_hz: 5.0, mix: 1.0, voices: 2 });
        for _ in 0..100 {
            let mut bus = vec![0.0f32; BLOCK * 2];
            c.process(&mut bus);
            assert!(bus.iter().all(|&s| s == 0.0));
        }
        // A DC-ish constant through full-wet chorus comes back at about unity.
        let mut last = vec![0.0f32; BLOCK * 2];
        for _ in 0..200 {
            last.fill(0.5);
            c.process(&mut last);
        }
        let mean = last.iter().sum::<f32>() / last.len() as f32;
        assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
    }
}
