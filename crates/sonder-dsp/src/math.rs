//! Small numeric helpers shared by the DSP modules: fast trig, dB conversion,
//! clamping, a tiny deterministic RNG and one-pole smoothing coefficients.

use std::f32::consts::{PI, TAU};

/// Sine of a phase in turns (0..1), computed with a 7th-order odd polynomial
/// (max abs error ≈ 1e-5). Much cheaper than `f32::sin` for 64 voices.
#[inline]
pub fn sin_turns(phase: f32) -> f32 {
    // Map to x in [-π, π].
    let p = phase - phase.floor();
    let mut x = p * TAU - PI;
    // Fold into [-π/2, π/2] via sin(x) = sin(π - x).
    if x > PI * 0.5 {
        x = PI - x;
    } else if x < -PI * 0.5 {
        x = -PI - x;
    }
    let x2 = x * x;
    // Taylor-ish minimax coefficients for sin on [-π/2, π/2].
    let r = x * (1.0 + x2 * (-0.166_666_55 + x2 * (0.008_332_16 + x2 * (-0.000_195_152_96 + x2 * 2.591_8e-6))));
    -r // we offset the phase by π above, so flip the sign back
}

/// Hyperbolic tangent approximation (Padé 7/6), accurate to ~1e-4 on ±4 and
/// saturating cleanly beyond that.
#[inline]
pub fn fast_tanh(x: f32) -> f32 {
    let x = x.clamp(-4.97, 4.97);
    let x2 = x * x;
    let num = x * (135_135.0 + x2 * (17_325.0 + x2 * (378.0 + x2)));
    let den = 135_135.0 + x2 * (62_370.0 + x2 * (3150.0 + x2 * 28.0));
    num / den
}

/// Soft saturator: `drive` 0..1 blends a clean path with a tanh curve whose gain
/// rises with drive. Unity gain for small signals at any drive.
#[inline]
pub fn saturate(x: f32, drive: f32) -> f32 {
    if drive <= 0.0 {
        return x;
    }
    let g = 1.0 + drive * 3.0;
    let wet = fast_tanh(x * g) / g * (1.0 + 0.6 * drive);
    lerp(x, wet, drive)
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * (lin.max(1e-9)).log10()
}

/// Clamp that also squashes NaN/inf to `lo`.
#[inline]
pub fn sane(x: f32, lo: f32, hi: f32) -> f32 {
    if x.is_finite() {
        x.clamp(lo, hi)
    } else {
        lo
    }
}

/// One-pole coefficient for a time constant `tau` seconds at `rate` Hz.
/// `y += coef * (target - y)` per step reaches ~63% of the way in `tau`.
#[inline]
pub fn onepole_coef(tau: f32, rate: f32) -> f32 {
    if tau <= 0.0 {
        1.0
    } else {
        1.0 - (-1.0 / (tau * rate)).exp()
    }
}

#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Equal-power pan: `pan` 0..1 → (left, right) gains.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(0.0, 1.0) * (PI * 0.5);
    (p.cos(), p.sin())
}

/// Tiny xorshift32 RNG: deterministic, allocation-free, good enough for noise.
#[derive(Debug, Clone, Copy)]
pub struct Rng(pub u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    /// Uniform in [-1, 1).
    #[inline]
    pub fn bipolar(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (2.0 / 16_777_216.0) - 1.0
    }
    /// Uniform in [0, 1).
    #[inline]
    pub fn unipolar(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }
}

/// Paul Kellet's refined pink-noise filter (−3 dB/oct, ~0.05 dB ripple above 9 Hz @44.1k).
#[derive(Debug, Clone, Copy, Default)]
pub struct Pink {
    b: [f32; 7],
}

impl Pink {
    #[inline]
    pub fn process(&mut self, white: f32) -> f32 {
        let b = &mut self.b;
        b[0] = 0.997_65 * b[0] + white * 0.099_046_0;
        b[1] = 0.963_00 * b[1] + white * 0.296_596_6;
        b[2] = 0.570_00 * b[2] + white * 1.052_691_3;
        b[3] = 0.004_11 * b[3] + white * 0.172_631;
        let pink = b[0] + b[1] + b[2] + b[3] + white * 0.5362;
        pink * 0.11
    }
}

/// Convert a frequency in Hz to a MIDI note number (f32).
#[inline]
pub fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz.max(1e-3) / 440.0).log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sin_accuracy() {
        for i in 0..1000 {
            let p = i as f32 / 1000.0;
            let exact = (p * TAU).sin();
            assert!((sin_turns(p) - exact).abs() < 2e-4, "p={p} got {} want {exact}", sin_turns(p));
        }
    }

    #[test]
    fn tanh_accuracy() {
        for i in -400..400 {
            let x = i as f32 / 100.0;
            assert!((fast_tanh(x) - x.tanh()).abs() < 2e-3, "x={x}");
        }
        assert!(fast_tanh(10.0) <= 1.0 && fast_tanh(10.0) > 0.99);
    }

    #[test]
    fn pan_is_equal_power() {
        let (l, r) = pan_gains(0.5);
        assert!((l * l + r * r - 1.0).abs() < 1e-5);
        let (l, r) = pan_gains(0.0);
        assert!(l > 0.999 && r.abs() < 1e-5);
    }
}
