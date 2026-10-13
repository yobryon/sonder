//! Trapezoidal state-variable filter (Andrew Simper / Cytomic). Stable under
//! fast modulation, with LP/HP/BP outputs from one topology.

use sonder_core::FilterKind;

/// SVF coefficients for one cutoff/Q setting.
#[derive(Debug, Clone, Copy, Default)]
pub struct SvfCoefs {
    g: f32,
    k: f32,
}

impl SvfCoefs {
    /// `cutoff_hz` is clamped to 20 Hz..0.45·sr; `resonance` 0..1 maps to Q ≈ 0.5..12.
    #[inline]
    pub fn new(cutoff_hz: f32, resonance: f32, sample_rate: f32) -> Self {
        let fc = cutoff_hz.clamp(20.0, sample_rate * 0.45);
        let g = (std::f32::consts::PI * fc / sample_rate).tan();
        let q = 0.5 * 2f32.powf(resonance.clamp(0.0, 1.0) * 4.6);
        Self { g, k: 1.0 / q }
    }
    #[inline]
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        Self { g: a.g + (b.g - a.g) * t, k: a.k + (b.k - a.k) * t }
    }
}

/// One SVF channel.
#[derive(Debug, Clone, Copy, Default)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
}

impl Svf {
    pub fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    /// Process one sample, returning the requested response. `Off` passes through.
    #[inline]
    pub fn process(&mut self, v0: f32, c: &SvfCoefs, kind: FilterKind) -> f32 {
        if matches!(kind, FilterKind::Off) {
            return v0;
        }
        let a1 = 1.0 / (1.0 + c.g * (c.g + c.k));
        let a2 = c.g * a1;
        let a3 = c.g * a2;
        let v3 = v0 - self.ic2;
        let v1 = a1 * self.ic1 + a2 * v3;
        let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        let out = match kind {
            FilterKind::Lp => v2,
            FilterKind::Bp => v1,
            FilterKind::Hp => v0 - c.k * v1 - v2,
            FilterKind::Off => v0,
        };
        // Keep denormals and runaway state out of the feedback path.
        if !out.is_finite() {
            self.reset();
            return 0.0;
        }
        out
    }

    /// Low-pass shortcut used by effects.
    #[inline]
    pub fn lp(&mut self, x: f32, c: &SvfCoefs) -> f32 {
        self.process(x, c, FilterKind::Lp)
    }
    #[inline]
    pub fn hp(&mut self, x: f32, c: &SvfCoefs) -> f32 {
        self.process(x, c, FilterKind::Hp)
    }
    #[inline]
    pub fn bp(&mut self, x: f32, c: &SvfCoefs) -> f32 {
        self.process(x, c, FilterKind::Bp)
    }
}

/// Simple one-pole low-pass (6 dB/oct), used for damping and smoothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct OnePole {
    z: f32,
}

impl OnePole {
    /// Coefficient for cutoff `hz` at `sr`.
    #[inline]
    pub fn coef(hz: f32, sr: f32) -> f32 {
        let x = (-std::f32::consts::TAU * hz.clamp(1.0, sr * 0.49) / sr).exp();
        1.0 - x
    }
    #[inline]
    pub fn lp(&mut self, x: f32, coef: f32) -> f32 {
        self.z += coef * (x - self.z);
        self.z
    }
    #[inline]
    pub fn hp(&mut self, x: f32, coef: f32) -> f32 {
        x - self.lp(x, coef)
    }
    pub fn reset(&mut self) {
        self.z = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_attenuates_high_frequency() {
        let sr = 48_000.0;
        let c = SvfCoefs::new(500.0, 0.0, sr);
        let mut f = Svf::default();
        let mut peak_lo = 0f32;
        let mut peak_hi = 0f32;
        for i in 0..48_000 {
            let t = i as f32 / sr;
            let lo = (t * 100.0 * std::f32::consts::TAU).sin();
            let y = f.process(lo, &c, FilterKind::Lp);
            if i > 24_000 {
                peak_lo = peak_lo.max(y.abs());
            }
        }
        f.reset();
        for i in 0..48_000 {
            let t = i as f32 / sr;
            let hi = (t * 8000.0 * std::f32::consts::TAU).sin();
            let y = f.process(hi, &c, FilterKind::Lp);
            if i > 24_000 {
                peak_hi = peak_hi.max(y.abs());
            }
        }
        assert!(peak_lo > 0.9, "{peak_lo}");
        assert!(peak_hi < 0.02, "{peak_hi}");
    }
}
