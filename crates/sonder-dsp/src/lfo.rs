//! Low-frequency oscillator with sample-and-hold random, evaluated once per block.

use sonder_core::LfoShape;

use crate::math::{sin_turns, Rng};

#[derive(Debug, Clone, Copy)]
pub struct Lfo {
    pub phase: f32,
    held: f32,
    rng: Rng,
}

impl Default for Lfo {
    fn default() -> Self {
        Self { phase: 0.0, held: 0.0, rng: Rng::new(0xC0FF_EE11) }
    }
}

impl Lfo {
    pub fn reset(&mut self, phase: f32, seed: u32) {
        self.phase = phase;
        self.rng = Rng::new(seed);
        self.held = self.rng.bipolar();
    }

    /// Advance by `dt_seconds` at `rate_hz` and return the bipolar value (−1..1).
    #[inline]
    pub fn step(&mut self, shape: LfoShape, rate_hz: f32, dt_seconds: f32) -> f32 {
        let inc = (rate_hz.clamp(0.0, 50.0) * dt_seconds).min(0.5);
        self.phase += inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            self.held = self.rng.bipolar();
        }
        let t = self.phase;
        match shape {
            LfoShape::Sine => sin_turns(t),
            LfoShape::Triangle => 1.0 - 4.0 * (t - 0.5).abs(),
            LfoShape::Saw => 1.0 - 2.0 * t,
            LfoShape::Square => {
                if t < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            LfoShape::Random => self.held,
        }
    }
}
