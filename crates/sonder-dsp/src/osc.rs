//! Oscillators. Every wave shape is rendered by [`OscState::render`], which
//! produces a stereo pair per sample so that the supersaw can spread its
//! partials across the field. Phase accumulators are `f64` and wrapped every
//! sample, so they never lose precision.

use sonder_core::{Osc, Table, Wave};

use crate::math::{sin_turns, Pink, Rng};
use crate::wavetable::{mip_for, Tables};

/// Detune curve for the seven supersaw partials, in units of `spread` cents.
const SUPERSAW_OFFSETS: [f32; 7] = [-1.0, -0.64, -0.3, 0.0, 0.3, 0.64, 1.0];
/// Mix weights: centre saw louder, outer saws quieter.
const SUPERSAW_LEVELS: [f32; 7] = [0.55, 0.7, 0.85, 1.0, 0.85, 0.7, 0.55];

/// PolyBLEP residual for a discontinuity at phase 0 with phase step `dt`.
#[inline]
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

#[inline]
fn saw_blep(t: f32, dt: f32) -> f32 {
    (2.0 * t - 1.0) - poly_blep(t, dt)
}

#[inline]
fn pulse_blep(t: f32, dt: f32, pw: f32) -> f32 {
    let naive = if t < pw { 1.0 } else { -1.0 };
    let mut t2 = t - pw;
    if t2 < 0.0 {
        t2 += 1.0;
    }
    // Remove the DC offset a non-50% pulse carries.
    naive + poly_blep(t, dt) - poly_blep(t2, dt) - (2.0 * pw - 1.0)
}

#[inline]
fn wrap(p: &mut f64) {
    if *p >= 1.0 {
        *p -= 1.0;
        if *p >= 1.0 {
            *p -= p.floor();
        }
    }
}

/// Per-voice state of one oscillator slot.
#[derive(Debug, Clone, Copy)]
pub struct OscState {
    /// Up to seven phase accumulators (supersaw); FM uses [0]=carrier, [1]=modulator.
    phase: [f64; 7],
    rng: Rng,
    pink: Pink,
    tri_prev: f32,
}

impl Default for OscState {
    fn default() -> Self {
        Self { phase: [0.0; 7], rng: Rng::new(0x1234_5678), pink: Pink::default(), tri_prev: -1.0 }
    }
}

/// Everything an oscillator needs for one block, resolved by the voice.
#[derive(Debug, Clone, Copy)]
pub struct OscParams {
    /// Base frequency (after octave/semitone/detune offsets and pitch modulation), Hz.
    pub freq_hz: f32,
    pub sample_rate: f32,
    /// Effective pulse width 0.05..0.95.
    pub pulse_width: f32,
    /// Supersaw spread in cents (after texture scaling).
    pub spread_cents: f32,
    /// Stereo width 0..1 for supersaw partial panning.
    pub width: f32,
    /// Effective FM index (after envelope scaling).
    pub fm_index: f32,
    pub fm_ratio: f32,
    /// Table for [`Wave::Wavetable`].
    pub table: Table,
    /// Shared wavetable store (never allocates after startup).
    pub tables: &'static Tables,
}

impl OscState {
    /// Reset phases (on note start), seeding the RNG so each voice differs.
    pub fn reset(&mut self, seed: u32) {
        self.phase = [0.0; 7];
        self.rng = Rng::new(seed);
        self.pink = Pink::default();
        // The integrated square starts at the bottom of its swing (phase 0 → square high).
        self.tri_prev = -1.0;
    }

    /// Random start phases for free-running feel (used by supersaw partials).
    pub fn randomize_phases(&mut self) {
        for p in &mut self.phase {
            *p = self.rng.unipolar() as f64;
        }
    }

    /// Render one sample. Returns `(left, right)`; mono shapes return equal channels.
    #[inline]
    pub fn render(&mut self, wave: Wave, p: &OscParams) -> (f32, f32) {
        let dt = (p.freq_hz / p.sample_rate).clamp(0.0, 0.49) as f64;
        match wave {
            // Placeholders until the v2 granular/physical-model oscillators land: silent.
            Wave::Grain | Wave::String | Wave::Modal => (0.0, 0.0),
            Wave::Sine => {
                let s = sin_turns(self.phase[0] as f32);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
            Wave::Triangle => {
                // Integrated PolyBLEP square → alias-suppressed triangle.
                let t = self.phase[0] as f32;
                let sq = pulse_blep(t, dt as f32, 0.5);
                // Leaky integrator scaled so the peak is ~1 regardless of frequency.
                let k = 4.0 * dt as f32;
                self.tri_prev = (self.tri_prev * 0.9999) + sq * k;
                let s = self.tri_prev.clamp(-1.0, 1.0);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
            Wave::Saw => {
                let s = saw_blep(self.phase[0] as f32, dt as f32);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
            Wave::Square => {
                let s = pulse_blep(self.phase[0] as f32, dt as f32, 0.5);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
            Wave::Pulse => {
                let pw = p.pulse_width.clamp(0.05, 0.95);
                let s = pulse_blep(self.phase[0] as f32, dt as f32, pw);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
            Wave::Supersaw => {
                let mut l = 0.0;
                let mut r = 0.0;
                let w = p.width.clamp(0.0, 1.0) * 0.5;
                for i in 0..7 {
                    let ratio = 2f32.powf(SUPERSAW_OFFSETS[i] * p.spread_cents / 1200.0);
                    let dti = (dt as f32 * ratio).clamp(0.0, 0.49);
                    let s = saw_blep(self.phase[i] as f32, dti) * SUPERSAW_LEVELS[i];
                    // Pan partial i by its detune offset scaled by width.
                    let pan = 0.5 + SUPERSAW_OFFSETS[i] * w;
                    l += s * (1.0 - pan);
                    r += s * pan;
                    self.phase[i] += dti as f64;
                    wrap(&mut self.phase[i]);
                }
                // Normalise: 7 partials with weights summing to 5.2, each channel gets half.
                (l * 0.5, r * 0.5)
            }
            Wave::Fm => {
                let m = sin_turns(self.phase[1] as f32);
                let s = sin_turns(self.phase[0] as f32 + p.fm_index * m * 0.25);
                self.phase[0] += dt;
                self.phase[1] += (dt as f32 * p.fm_ratio.max(0.01)) as f64;
                wrap(&mut self.phase[0]);
                wrap(&mut self.phase[1]);
                (s, s)
            }
            Wave::Noise => {
                let n = self.rng.bipolar();
                (n, n)
            }
            Wave::Pink => {
                let n = self.pink.process(self.rng.bipolar()) * 3.0;
                let n = n.clamp(-1.0, 1.0);
                (n, n)
            }
            Wave::Wavetable => {
                let mip = mip_for(p.freq_hz, p.sample_rate);
                let s = p.tables.sample(p.table, mip, self.phase[0]);
                self.phase[0] += dt;
                wrap(&mut self.phase[0]);
                (s, s)
            }
        }
    }
}

/// Frequency multiplier for an oscillator's static offsets (octave, semitones, cents).
#[inline]
pub fn osc_ratio(o: &Osc) -> f32 {
    2f32.powf(o.octave as f32 + o.semitones / 12.0 + o.detune_cents / 1200.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saw_is_bounded_and_periodic() {
        let mut st = OscState::default();
        let p = OscParams {
            freq_hz: 1000.0,
            sample_rate: 48_000.0,
            pulse_width: 0.5,
            spread_cents: 12.0,
            width: 0.0,
            fm_index: 0.0,
            fm_ratio: 2.0,
            table: Table::Organ,
            tables: crate::wavetable::tables(),
        };
        for w in [Wave::Sine, Wave::Triangle, Wave::Saw, Wave::Square, Wave::Pulse, Wave::Supersaw, Wave::Fm, Wave::Noise, Wave::Pink, Wave::Wavetable] {
            let mut peak = 0f32;
            for _ in 0..48_000 {
                let (l, r) = st.render(w, &p);
                assert!(l.is_finite() && r.is_finite());
                peak = peak.max(l.abs()).max(r.abs());
            }
            assert!(peak <= 1.3, "{w:?} peak {peak}");
            assert!(peak > 0.2, "{w:?} peak {peak}");
        }
    }
}
