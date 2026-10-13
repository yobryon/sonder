//! Tempo-synced stereo delay with smoothly interpolated time changes, damped
//! feedback and optional ping-pong.

use sonder_core::DelaySettings;

use crate::filter::OnePole;
use crate::math::{onepole_coef, sane};
use crate::BLOCK;

/// Maximum delay time in seconds the line is allocated for.
pub const MAX_DELAY_SECONDS: f32 = 4.0;

#[derive(Debug)]
pub struct StereoDelay {
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    /// Current (smoothed) delay in samples and its target.
    time: f32,
    target_time: f32,
    feedback: f32,
    ping_pong: bool,
    damp_coef: f32,
    damp_l: OnePole,
    damp_r: OnePole,
    dc_l: OnePole,
    dc_r: OnePole,
    bpm: f32,
    beats: f32,
    sr: f32,
    smooth_coef: f32,
}

impl StereoDelay {
    pub fn new(sample_rate: f32) -> Self {
        let len = (MAX_DELAY_SECONDS * sample_rate) as usize + 2;
        let mut d = Self {
            buf_l: vec![0.0; len],
            buf_r: vec![0.0; len],
            write: 0,
            time: 0.0,
            target_time: 0.0,
            feedback: 0.35,
            ping_pong: true,
            damp_coef: 1.0,
            damp_l: OnePole::default(),
            damp_r: OnePole::default(),
            dc_l: OnePole::default(),
            dc_r: OnePole::default(),
            bpm: 120.0,
            beats: 0.75,
            sr: sample_rate,
            // Delay time glides over ~150 ms (block rate).
            smooth_coef: onepole_coef(0.15, sample_rate / BLOCK as f32),
        };
        d.configure(&DelaySettings::default());
        d.time = d.target_time;
        d
    }

    pub fn configure(&mut self, s: &DelaySettings) {
        self.beats = sane(s.beats, 0.01, 16.0);
        self.feedback = sane(s.feedback, 0.0, 0.95);
        self.ping_pong = s.ping_pong;
        self.damp_coef = OnePole::coef(sane(s.damp_hz, 200.0, 20_000.0), self.sr);
        self.update_target();
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.bpm = sane(bpm, 20.0, 400.0);
        self.update_target();
    }

    fn update_target(&mut self) {
        let secs = self.beats * 60.0 / self.bpm;
        let max = (self.buf_l.len() - 2) as f32;
        self.target_time = (secs * self.sr).clamp(1.0, max);
    }

    pub fn clear(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.damp_l.reset();
        self.damp_r.reset();
    }

    #[inline]
    fn read(buf: &[f32], write: usize, delay: f32) -> f32 {
        let len = buf.len();
        let d = delay.clamp(1.0, (len - 2) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (write + len - di) % len;
        let i1 = (i0 + len - 1) % len;
        buf[i0] + (buf[i1] - buf[i0]) * frac
    }

    /// Process one block: `input` is the send bus (interleaved), `output` receives
    /// the wet signal (overwritten).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let frames = input.len() / 2;
        let start = self.time;
        self.time += self.smooth_coef * (self.target_time - self.time);
        let end = self.time;
        let len = self.buf_l.len();
        let dc_coef = OnePole::coef(20.0, self.sr);
        for i in 0..frames {
            let t = i as f32 / frames as f32;
            let d = start + (end - start) * t;
            let out_l = Self::read(&self.buf_l, self.write, d);
            let out_r = Self::read(&self.buf_r, self.write, d);
            let in_l = input[i * 2];
            let in_r = input[i * 2 + 1];
            let fb_l = self.dc_l.hp(self.damp_l.lp(out_l, self.damp_coef), dc_coef) * self.feedback;
            let fb_r = self.dc_r.hp(self.damp_r.lp(out_r, self.damp_coef), dc_coef) * self.feedback;
            let (w_l, w_r) = if self.ping_pong {
                ((in_l + in_r) * 0.5 + fb_r, fb_l)
            } else {
                (in_l + fb_l, in_r + fb_r)
            };
            self.buf_l[self.write] = sane(w_l, -4.0, 4.0);
            self.buf_r[self.write] = sane(w_r, -4.0, 4.0);
            self.write = (self.write + 1) % len;
            output[i * 2] = out_l;
            output[i * 2 + 1] = out_r;
        }
    }
}
