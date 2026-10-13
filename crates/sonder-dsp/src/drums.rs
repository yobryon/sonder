//! Synthesized percussion. No samples: every hit is a small recipe of sines,
//! noise, filters and decays. Each [`DrumVoice`] renders mono then pans.

use sonder_core::{Drum, LayerId};

use crate::filter::{OnePole, Svf, SvfCoefs};
use crate::math::{fast_tanh, lerp, pan_gains, sin_turns, Rng};
use crate::BLOCK;

#[derive(Debug, Clone, Copy)]
pub struct DrumVoice {
    pub active: bool,
    pub layer: LayerId,
    pub age: u64,
    kind: Drum,
    /// Samples since trigger.
    t: u32,
    /// Total length in samples (voice ends after this).
    len: u32,
    velocity: f32,
    tone: f32,
    gain_l: f32,
    gain_r: f32,
    phase: [f32; 6],
    rng: Rng,
    svf: Svf,
    svf2: Svf,
    hp: OnePole,
    sr: f32,
}

impl Default for DrumVoice {
    fn default() -> Self {
        Self {
            active: false,
            layer: LayerId(0),
            age: 0,
            kind: Drum::Kick,
            t: 0,
            len: 0,
            velocity: 1.0,
            tone: 0.5,
            gain_l: 0.0,
            gain_r: 0.0,
            phase: [0.0; 6],
            rng: Rng::new(0xD2D2),
            svf: Svf::default(),
            svf2: Svf::default(),
            hp: OnePole::default(),
            sr: 48_000.0,
        }
    }
}

/// Parameters of one percussion hit.
#[derive(Debug, Clone, Copy)]
pub struct DrumHit {
    pub layer: LayerId,
    pub kind: Drum,
    pub velocity: f32,
    pub pan: f32,
    pub tone: f32,
    pub age: u64,
    pub seed: u32,
}

/// Exponential decay reaching −60 dB at `seconds`.
#[inline]
fn decay(t: f32, seconds: f32) -> f32 {
    (-6.9 * t / seconds.max(0.001)).exp()
}

impl DrumVoice {
    pub fn trigger(&mut self, sr: f32, hit: DrumHit) {
        let DrumHit { layer, kind, velocity, pan, tone, age, seed } = hit;
        self.sr = sr;
        self.active = true;
        self.layer = layer;
        self.age = age;
        self.kind = kind;
        self.t = 0;
        self.velocity = velocity.clamp(0.0, 1.0);
        self.tone = tone.clamp(0.0, 1.0);
        let (l, r) = pan_gains(pan.clamp(0.0, 1.0));
        // Perceptual velocity curve.
        let v = 0.15 + 0.85 * self.velocity * self.velocity;
        self.gain_l = l * v;
        self.gain_r = r * v;
        self.phase = [0.0; 6];
        self.rng = Rng::new(seed);
        self.svf.reset();
        self.svf2.reset();
        self.hp.reset();
        let secs = match kind {
            Drum::Kick => 0.6,
            Drum::Snare => 0.35,
            Drum::Hat => lerp(0.4, 0.08, self.tone) + 0.05,
            Drum::Click => 0.03,
            Drum::Noise => 0.08 + 0.6 * self.velocity,
            Drum::Tom => 0.5,
            Drum::Rim => 0.12,
        };
        self.len = (secs * sr) as u32;
    }

    /// Render one block, adding into `out` (interleaved stereo). Returns false when done.
    pub fn render(&mut self, out: &mut [f32]) -> bool {
        let sr = self.sr;
        let inv_sr = 1.0 / sr;
        let tone = self.tone;
        // Precompute per-block filter settings.
        let (c1, c2) = match self.kind {
            Drum::Kick => (SvfCoefs::new(3000.0, 0.1, sr), SvfCoefs::new(60.0, 0.0, sr)),
            Drum::Snare => (SvfCoefs::new(lerp(1500.0, 4500.0, tone), 0.35, sr), SvfCoefs::new(400.0, 0.0, sr)),
            Drum::Hat => (SvfCoefs::new(lerp(5000.0, 9500.0, tone), 0.3, sr), SvfCoefs::new(12_000.0, 0.0, sr)),
            Drum::Click => (SvfCoefs::new(lerp(1800.0, 7000.0, tone), 0.7, sr), SvfCoefs::default()),
            Drum::Noise => (SvfCoefs::new(lerp(400.0, 6000.0, tone), 0.25, sr), SvfCoefs::default()),
            Drum::Tom => (SvfCoefs::new(2500.0, 0.1, sr), SvfCoefs::default()),
            Drum::Rim => (SvfCoefs::new(lerp(1200.0, 3200.0, tone), 0.85, sr), SvfCoefs::default()),
        };
        let hp_coef = OnePole::coef(30.0, sr);
        for i in 0..BLOCK {
            if self.t >= self.len {
                self.active = false;
                return false;
            }
            let t = self.t as f32 * inv_sr;
            let tn = t / (self.len as f32 * inv_sr);
            // Smooth tail so the voice end never clicks.
            let tail = if tn > 0.9 { (1.0 - tn) * 10.0 } else { 1.0 };
            let s = match self.kind {
                Drum::Kick => {
                    let f_end = lerp(38.0, 62.0, tone);
                    let f = f_end + (160.0 - f_end) * (-t / 0.035).exp();
                    self.phase[0] += f * inv_sr;
                    if self.phase[0] >= 1.0 {
                        self.phase[0] -= 1.0;
                    }
                    let body = sin_turns(self.phase[0]) * decay(t, 0.45);
                    let click = self.rng.bipolar() * decay(t, 0.006) * 0.5;
                    let click = self.svf.hp(click, &c1);
                    fast_tanh((body * 1.6 + click) * (1.0 + tone * 0.5))
                }
                Drum::Snare => {
                    for (k, f) in [185.0f32, 330.0].iter().enumerate() {
                        let f = f * (1.0 + 0.5 * (-t / 0.02).exp()) * lerp(0.85, 1.2, tone);
                        self.phase[k] += f * inv_sr;
                        if self.phase[k] >= 1.0 {
                            self.phase[k] -= 1.0;
                        }
                    }
                    let body = (sin_turns(self.phase[0]) + 0.6 * sin_turns(self.phase[1])) * decay(t, 0.13) * 0.5;
                    let noise = self.svf.bp(self.rng.bipolar(), &c1) * decay(t, 0.22) * 1.6;
                    let noise = self.svf2.hp(noise, &c2);
                    fast_tanh(body + noise)
                }
                Drum::Hat => {
                    const RATIOS: [f32; 6] = [1.0, 1.4471, 1.6170, 1.9265, 2.5028, 2.6637];
                    let base = 320.0 * lerp(0.9, 1.15, tone);
                    let mut sum = 0.0;
                    for (ph, ratio) in self.phase.iter_mut().zip(RATIOS.iter()) {
                        *ph += base * ratio * inv_sr;
                        if *ph >= 1.0 {
                            *ph -= 1.0;
                        }
                        sum += if *ph < 0.5 { 1.0 } else { -1.0 };
                    }
                    let metal = self.svf.hp(sum / 6.0, &c1);
                    let metal = self.svf2.lp(metal, &c2);
                    let d = lerp(0.35, 0.06, tone);
                    // Fast initial burst then the main decay.
                    let env = decay(t, d) * 0.8 + decay(t, 0.01) * 0.4;
                    fast_tanh(metal * 3.0 * env)
                }
                Drum::Click => {
                    let imp = if self.t < 2 { 1.0 } else { 0.0 } + self.rng.bipolar() * decay(t, 0.003) * 0.3;
                    self.svf.bp(imp, &c1) * 3.0 * decay(t, 0.02)
                }
                Drum::Noise => {
                    let len_s = self.len as f32 * inv_sr;
                    let n = self.rng.bipolar();
                    let n = self.svf.bp(n, &c1) * 1.5;
                    // Soft attack (5 ms) and decay over the voice length.
                    let att = (t / 0.005).min(1.0);
                    n * att * decay(t, len_s * 0.9)
                }
                Drum::Tom => {
                    let f0 = lerp(70.0, 260.0, tone);
                    let f = f0 * (1.0 + 0.35 * (-t / 0.04).exp());
                    self.phase[0] += f * inv_sr;
                    if self.phase[0] >= 1.0 {
                        self.phase[0] -= 1.0;
                    }
                    self.phase[1] += f * 1.5 * inv_sr;
                    if self.phase[1] >= 1.0 {
                        self.phase[1] -= 1.0;
                    }
                    let body = sin_turns(self.phase[0]) * decay(t, 0.4) + 0.3 * sin_turns(self.phase[1]) * decay(t, 0.15);
                    let skin = self.svf.lp(self.rng.bipolar(), &c1) * decay(t, 0.03) * 0.4;
                    fast_tanh((body + skin) * 1.3)
                }
                Drum::Rim => {
                    for (k, f) in [1715.0f32, 2560.0, 445.0].iter().enumerate() {
                        self.phase[k] += f * lerp(0.9, 1.2, tone) * inv_sr;
                        if self.phase[k] >= 1.0 {
                            self.phase[k] -= 1.0;
                        }
                    }
                    let metal = (sin_turns(self.phase[0]) + 0.7 * sin_turns(self.phase[1])) * decay(t, 0.05) * 0.5
                        + sin_turns(self.phase[2]) * decay(t, 0.09) * 0.5;
                    let crack = self.svf.bp(self.rng.bipolar(), &c1) * decay(t, 0.012) * 2.0;
                    fast_tanh((metal + crack) * 1.4)
                }
            };
            let s = self.hp.hp(s, hp_coef) * tail;
            out[i * 2] += s * self.gain_l;
            out[i * 2 + 1] += s * self.gain_r;
            self.t += 1;
        }
        true
    }
}
