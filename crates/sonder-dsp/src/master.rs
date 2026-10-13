//! Master chain: gain → soft saturation → peak limiter → hard clip.

use sonder_core::MasterSettings;

use crate::math::{db_to_lin, onepole_coef, sane, saturate};

#[derive(Debug, Clone)]
pub struct MasterChain {
    gain: f32,
    saturation: f32,
    ceiling: f32,
    env: f32,
    attack_coef: f32,
    release_coef: f32,
}

impl MasterChain {
    pub fn new(sample_rate: f32) -> Self {
        let mut m = Self {
            gain: 0.9,
            saturation: 0.1,
            ceiling: db_to_lin(-1.0),
            env: 0.0,
            attack_coef: onepole_coef(0.0002, sample_rate),
            release_coef: onepole_coef(0.15, sample_rate),
        };
        m.configure(&MasterSettings::default());
        m
    }

    pub fn configure(&mut self, s: &MasterSettings) {
        self.gain = sane(s.gain, 0.0, 4.0);
        self.saturation = sane(s.saturation, 0.0, 1.0);
        self.ceiling = db_to_lin(sane(s.ceiling_db, -30.0, 0.0));
    }

    /// Process interleaved stereo in place. Never lets |x| exceed the ceiling.
    pub fn process(&mut self, buf: &mut [f32]) {
        let n = buf.len() / 2;
        for i in 0..n {
            let mut l = buf[i * 2] * self.gain;
            let mut r = buf[i * 2 + 1] * self.gain;
            if self.saturation > 0.0 {
                l = saturate(l, self.saturation);
                r = saturate(r, self.saturation);
            }
            // Peak follower: instant attack toward the current peak, slow release.
            let peak = l.abs().max(r.abs());
            if peak > self.env {
                self.env += self.attack_coef.max(1.0) * (peak - self.env);
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
        }
    }
}
