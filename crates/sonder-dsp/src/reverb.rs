//! Plate reverb after Dattorro (1997): input diffusion into a figure-of-eight
//! tank with modulated allpasses, multi-tap stereo output. Sizes are scaled
//! from the paper's 29.761 kHz reference to the running sample rate.

use sonder_core::ReverbSettings;

use crate::filter::OnePole;
use crate::math::{onepole_coef, sane, sin_turns};
use crate::BLOCK;

const REF_RATE: f32 = 29_761.0;
/// Max predelay the buffer is sized for.
const MAX_PREDELAY_S: f32 = 0.25;
/// Allpass modulation excursion in reference samples.
const EXCURSION: f32 = 16.0;

#[derive(Debug)]
struct Line {
    buf: Vec<f32>,
    write: usize,
}

impl Line {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(2)], write: 0 }
    }
    #[inline]
    fn push(&mut self, v: f32) {
        self.buf[self.write] = v;
        self.write = (self.write + 1) % self.buf.len();
    }
    /// Read `delay` samples back (integer).
    #[inline]
    fn tap(&self, delay: usize) -> f32 {
        let len = self.buf.len();
        let d = delay.clamp(1, len - 1);
        self.buf[(self.write + len - d) % len]
    }
    /// Fractional read.
    #[inline]
    fn tap_frac(&self, delay: f32) -> f32 {
        let len = self.buf.len();
        let d = delay.clamp(1.0, (len - 2) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let a = self.buf[(self.write + len - di) % len];
        let b = self.buf[(self.write + len - di - 1) % len];
        a + (b - a) * frac
    }
    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

/// Schroeder allpass built on a [`Line`].
#[derive(Debug)]
struct Allpass {
    line: Line,
    delay: f32,
    gain: f32,
}

impl Allpass {
    fn new(delay: usize, gain: f32, margin: usize) -> Self {
        Self { line: Line::new(delay + margin + 2), delay: delay as f32, gain }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.process_mod(x, 0.0)
    }
    #[inline]
    fn process_mod(&mut self, x: f32, offset: f32) -> f32 {
        let d = self.line.tap_frac(self.delay + offset);
        let v = x - self.gain * d;
        self.line.push(v);
        d + self.gain * v
    }
    #[inline]
    fn tap(&self, delay: usize) -> f32 {
        self.line.tap(delay)
    }
}

#[derive(Debug)]
pub struct Plate {
    sr: f32,
    scale: f32,
    predelay: Line,
    predelay_samples: f32,
    predelay_target: f32,
    predelay_coef: f32,
    bandwidth: OnePole,
    bandwidth_coef: f32,
    diffusers: [Allpass; 4],
    // Tank, left half then right half.
    ap_l1: Allpass,
    del_l1: Line,
    damp_l: OnePole,
    ap_l2: Allpass,
    del_l2: Line,
    ap_r1: Allpass,
    del_r1: Line,
    damp_r: OnePole,
    ap_r2: Allpass,
    del_r2: Line,
    fb_l: f32,
    fb_r: f32,
    decay: f32,
    damp_coef: f32,
    lowcut_l: OnePole,
    lowcut_r: OnePole,
    lowcut_coef: f32,
    lfo_phase: f32,
    // Output tap positions (already scaled).
    taps_l: [usize; 7],
    taps_r: [usize; 7],
}

impl Plate {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let scale = sr / REF_RATE;
        let s = |n: f32| (n * scale).round() as usize;
        let margin = (EXCURSION * scale) as usize + 4;
        let mut p = Self {
            sr,
            scale,
            predelay: Line::new((MAX_PREDELAY_S * sr) as usize + 2),
            predelay_samples: 0.0,
            predelay_target: 0.0,
            predelay_coef: onepole_coef(0.2, sr / BLOCK as f32),
            bandwidth: OnePole::default(),
            bandwidth_coef: OnePole::coef(10_000.0, sr),
            diffusers: [
                Allpass::new(s(142.0), 0.75, 0),
                Allpass::new(s(107.0), 0.75, 0),
                Allpass::new(s(379.0), 0.625, 0),
                Allpass::new(s(277.0), 0.625, 0),
            ],
            ap_l1: Allpass::new(s(672.0), -0.7, margin),
            del_l1: Line::new(s(4453.0) + 2),
            damp_l: OnePole::default(),
            ap_l2: Allpass::new(s(1800.0), 0.5, 0),
            del_l2: Line::new(s(3720.0) + 2),
            ap_r1: Allpass::new(s(908.0), -0.7, margin),
            del_r1: Line::new(s(4217.0) + 2),
            damp_r: OnePole::default(),
            ap_r2: Allpass::new(s(2656.0), 0.5, 0),
            del_r2: Line::new(s(3163.0) + 2),
            fb_l: 0.0,
            fb_r: 0.0,
            decay: 0.5,
            damp_coef: 0.5,
            lowcut_l: OnePole::default(),
            lowcut_r: OnePole::default(),
            lowcut_coef: OnePole::coef(150.0, sr),
            lfo_phase: 0.0,
            taps_l: [s(266.0), s(2974.0), s(1913.0), s(1996.0), s(1990.0), s(187.0), s(1066.0)],
            taps_r: [s(353.0), s(3627.0), s(1228.0), s(2673.0), s(2111.0), s(335.0), s(121.0)],
        };
        p.configure(&ReverbSettings::default());
        p.predelay_samples = p.predelay_target;
        p
    }

    pub fn configure(&mut self, s: &ReverbSettings) {
        let size = sane(s.size, 0.0, 1.0);
        // size 0 → short room (0.35), 1 → long plate (0.93).
        self.decay = 0.35 + 0.58 * size.powf(0.6);
        let damping = sane(s.damping, 0.0, 1.0);
        // damping 0 → 18 kHz, 1 → 1.2 kHz low-pass in the tank.
        let damp_hz = 18_000.0 * (1200.0f32 / 18_000.0).powf(damping);
        self.damp_coef = OnePole::coef(damp_hz, self.sr);
        self.predelay_target =
            (sane(s.predelay_ms, 0.0, MAX_PREDELAY_S * 1000.0) * 0.001 * self.sr).clamp(1.0, (self.predelay.buf.len() - 2) as f32);
        self.lowcut_coef = OnePole::coef(sane(s.lowcut_hz, 10.0, 2000.0), self.sr);
    }

    pub fn clear(&mut self) {
        self.predelay.clear();
        for d in &mut self.diffusers {
            d.line.clear();
        }
        for l in [&mut self.ap_l1.line, &mut self.del_l1, &mut self.ap_l2.line, &mut self.del_l2, &mut self.ap_r1.line, &mut self.del_r1, &mut self.ap_r2.line, &mut self.del_r2] {
            l.clear();
        }
        self.fb_l = 0.0;
        self.fb_r = 0.0;
        self.bandwidth.reset();
        self.damp_l.reset();
        self.damp_r.reset();
    }

    /// Process one block of the send bus into `output` (overwritten, wet only).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let frames = input.len() / 2;
        self.predelay_samples += self.predelay_coef * (self.predelay_target - self.predelay_samples);
        let exc = EXCURSION * self.scale;
        let lfo_inc = 0.7 / self.sr; // 0.7 Hz modulation
        for i in 0..frames {
            // Mono in, predelay, bandwidth.
            let x = (input[i * 2] + input[i * 2 + 1]) * 0.5;
            self.predelay.push(sane(x, -4.0, 4.0));
            let mut v = self.predelay.tap_frac(self.predelay_samples);
            v = self.bandwidth.lp(v, self.bandwidth_coef);
            for d in &mut self.diffusers {
                v = d.process(v);
            }
            // Tank.
            self.lfo_phase += lfo_inc;
            if self.lfo_phase >= 1.0 {
                self.lfo_phase -= 1.0;
            }
            let m1 = sin_turns(self.lfo_phase) * exc;
            let m2 = sin_turns(self.lfo_phase + 0.25) * exc;

            let l_in = v + self.fb_r;
            let a = self.ap_l1.process_mod(l_in, m1);
            self.del_l1.push(a);
            let mut l = self.del_l1.tap(self.del_l1.buf.len() - 2);
            l = self.damp_l.lp(l, self.damp_coef) * self.decay;
            l = self.ap_l2.process(l);
            self.del_l2.push(l);
            let l_out = self.del_l2.tap(self.del_l2.buf.len() - 2) * self.decay;

            let r_in = v + self.fb_l;
            let b = self.ap_r1.process_mod(r_in, m2);
            self.del_r1.push(b);
            let mut r = self.del_r1.tap(self.del_r1.buf.len() - 2);
            r = self.damp_r.lp(r, self.damp_coef) * self.decay;
            r = self.ap_r2.process(r);
            self.del_r2.push(r);
            let r_out = self.del_r2.tap(self.del_r2.buf.len() - 2) * self.decay;

            self.fb_l = sane(l_out, -8.0, 8.0);
            self.fb_r = sane(r_out, -8.0, 8.0);

            // Output taps (Dattorro Table 2).
            let tl = &self.taps_l;
            let yl = self.del_r1.tap(tl[0]) + self.del_r1.tap(tl[1]) - self.ap_r2.tap(tl[2]) + self.del_r2.tap(tl[3])
                - self.del_l1.tap(tl[4])
                - self.ap_l2.tap(tl[5])
                - self.del_l2.tap(tl[6]);
            let tr = &self.taps_r;
            let yr = self.del_l1.tap(tr[0]) + self.del_l1.tap(tr[1]) - self.ap_l2.tap(tr[2]) + self.del_l2.tap(tr[3])
                - self.del_r1.tap(tr[4])
                - self.ap_r2.tap(tr[5])
                - self.del_r2.tap(tr[6]);
            let yl = self.lowcut_l.hp(yl * 0.6, self.lowcut_coef);
            let yr = self.lowcut_r.hp(yr * 0.6, self.lowcut_coef);
            output[i * 2] = yl;
            output[i * 2 + 1] = yr;
        }
    }
}
