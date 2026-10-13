//! ADSR envelope with analog-style exponential segments. Click-free: attack
//! starts from the current level, release can begin from any stage, and the
//! minimum segment times are a few milliseconds.

use sonder_core::Adsr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Envelope {
    pub stage: Stage,
    pub level: f32,
    attack_coef: f32,
    decay_coef: f32,
    release_coef: f32,
    sustain: f32,
}

/// Level below which a releasing envelope is considered finished (−84 dB).
const END_THRESHOLD: f32 = 6e-5;
/// Attack overshoots toward this target so it reaches 1.0 in finite time.
const ATTACK_TARGET: f32 = 1.3;

impl Envelope {
    /// Configure from an [`Adsr`] at `sample_rate`.
    pub fn configure(&mut self, a: &Adsr, sample_rate: f32) {
        let att = a.attack.max(0.001);
        let dec = a.decay.max(0.002);
        let rel = a.release.max(0.003);
        // Attack: reach 1.0 (target 1.3) in `att` seconds: 1 - exp(-n·c) = 1/1.3.
        self.attack_coef = 1.0 - (-(1.0 / (att * sample_rate)) * 1.47).exp();
        // Decay/release: ~−60 dB in the given time (time constant = t/6.9).
        self.decay_coef = 1.0 - (-(6.9 / (dec * sample_rate))).exp();
        self.release_coef = 1.0 - (-(6.9 / (rel * sample_rate))).exp();
        self.sustain = a.sustain.clamp(0.0, 1.0);
    }

    pub fn trigger(&mut self) {
        self.stage = Stage::Attack;
    }

    pub fn release(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    /// Switch to a fast release (used by Panic / voice stealing).
    pub fn fast_release(&mut self, seconds: f32, sample_rate: f32) {
        self.release_coef = 1.0 - (-(6.9 / (seconds.max(0.001) * sample_rate))).exp();
        self.stage = Stage::Release;
    }

    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }
    pub fn is_released(&self) -> bool {
        matches!(self.stage, Stage::Release | Stage::Idle)
    }

    /// Advance one sample and return the new level.
    #[inline]
    pub fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Idle => {
                self.level = 0.0;
            }
            Stage::Attack => {
                self.level += self.attack_coef * (ATTACK_TARGET - self.level);
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level += self.decay_coef * (self.sustain - self.level);
                if (self.level - self.sustain).abs() < 1e-4 {
                    self.level = self.sustain;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => {
                self.level = self.sustain;
            }
            Stage::Release => {
                self.level += self.release_coef * (0.0 - self.level);
                if self.level < END_THRESHOLD {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adsr_shape() {
        let sr = 48_000.0;
        let mut e = Envelope::default();
        e.configure(&Adsr { attack: 0.01, decay: 0.1, sustain: 0.5, release: 0.1 }, sr);
        e.trigger();
        let mut peak = 0.0f32;
        for _ in 0..(0.02 * sr) as usize {
            peak = peak.max(e.next());
        }
        assert!((peak - 1.0).abs() < 1e-3, "attack reached {peak}");
        for _ in 0..(0.3 * sr) as usize {
            e.next();
        }
        assert!((e.level - 0.5).abs() < 1e-3, "sustain {}", e.level);
        e.release();
        for _ in 0..(0.3 * sr) as usize {
            e.next();
        }
        assert!(e.is_idle());
    }
}
