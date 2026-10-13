//! The beat clock: tempo, meter, swing, humanize, and grid arithmetic.

use sonder_core::{ConductorConfig, TransportPos};

/// Time constant (seconds) for energy-driven tempo drift.
const TEMPO_TAU: f64 = 60.0;
/// Minimum bpm change worth telling the synth about.
const TEMPO_NOTIFY_STEP: f64 = 0.1;

/// Musical time. Advances in beats when ticked; never reads a wall clock.
#[derive(Debug, Clone)]
pub struct Transport {
    bpm: f64,
    scene_tempo: f64,
    range: (f64, f64),
    user_offset: f64,
    beats_per_bar: u8,
    swing: f64,
    humanize: f64,
    beats: f64,
    prev_beats: f64,
    last_sent_bpm: Option<f64>,
}

impl Transport {
    pub fn new(cfg: &ConductorConfig) -> Self {
        let lo = cfg.tempo_range[0].min(cfg.tempo_range[1]).max(1.0);
        let hi = cfg.tempo_range[0].max(cfg.tempo_range[1]).max(lo);
        let tempo = cfg.tempo.clamp(lo, hi);
        Self {
            bpm: tempo,
            scene_tempo: tempo,
            range: (lo, hi),
            user_offset: 0.0,
            beats_per_bar: cfg.beats_per_bar.max(1),
            swing: cfg.swing.clamp(0.0, 1.0),
            humanize: cfg.humanize.clamp(0.0, 1.0),
            beats: 0.0,
            prev_beats: 0.0,
            last_sent_bpm: None,
        }
    }

    /// Advance by `dt` seconds. `energy` (0..1) pulls the tempo slowly toward
    /// the scene tempo at 0.5, the range bounds at 0 and 1. Returns the new bpm
    /// when it has moved enough since the last report to be worth sending.
    pub fn tick(&mut self, dt: f64, energy: f64) -> Option<f64> {
        let e = energy.clamp(0.0, 1.0);
        let target = if e >= 0.5 {
            lerp(self.scene_tempo, self.range.1, (e - 0.5) * 2.0)
        } else {
            lerp(self.range.0, self.scene_tempo, e * 2.0)
        } + self.user_offset;
        let target = target.clamp(self.range.0, self.range.1);
        let alpha = 1.0 - (-dt / TEMPO_TAU).exp();
        self.bpm += (target - self.bpm) * alpha;
        self.bpm = self.bpm.clamp(self.range.0, self.range.1);

        self.prev_beats = self.beats;
        self.beats += dt * self.bpm / 60.0;

        match self.last_sent_bpm {
            Some(last) if (last - self.bpm).abs() <= TEMPO_NOTIFY_STEP => None,
            _ => {
                self.last_sent_bpm = Some(self.bpm);
                Some(self.bpm)
            }
        }
    }

    /// Add to the user tempo offset (bpm); the result stays within the range.
    pub fn nudge(&mut self, delta: f64) {
        let (lo, hi) = self.range;
        self.user_offset =
            (self.user_offset + delta).clamp(lo - self.scene_tempo, hi - self.scene_tempo);
        // Jump immediately: a user nudge should be heard now, not in a minute.
        self.bpm = (self.bpm + delta).clamp(lo, hi);
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }
    pub fn beats_per_bar(&self) -> u8 {
        self.beats_per_bar
    }
    pub fn swing(&self) -> f64 {
        self.swing
    }
    pub fn humanize(&self) -> f64 {
        self.humanize
    }
    /// Elapsed beats at the end of this tick.
    pub fn beats(&self) -> f64 {
        self.beats
    }
    /// Elapsed beats at the end of the previous tick.
    pub fn prev_beats(&self) -> f64 {
        self.prev_beats
    }
    pub fn seconds_per_beat(&self) -> f64 {
        60.0 / self.bpm
    }
    /// Zero-based bar of the current position.
    pub fn bar(&self) -> u64 {
        (self.beats / self.beats_per_bar as f64).floor() as u64
    }

    pub fn pos(&self) -> TransportPos {
        let bpb = self.beats_per_bar as f64;
        let bar = (self.beats / bpb).floor();
        let in_bar = self.beats - bar * bpb;
        let beat = in_bar.floor();
        TransportPos {
            bpm: self.bpm,
            beats_per_bar: self.beats_per_bar,
            bar: bar as u64,
            beat: beat as u8,
            phase: in_bar - beat,
            beats: self.beats,
        }
    }

    /// First multiple of `grid` (in beats) at or after `beats`.
    pub fn next_grid(beats: f64, grid: f64) -> f64 {
        if grid <= 0.0 {
            return beats;
        }
        let k = (beats / grid - 1e-9).ceil();
        k * grid
    }

    /// Grid points `g` with `prev_beats < g <= beats` for a subdivision of
    /// `grid` beats, in order. Bounded so a huge `dt` cannot explode.
    pub fn grid_points(&self, grid: f64) -> Vec<f64> {
        let mut out = Vec::new();
        if grid <= 0.0 {
            return out;
        }
        // The opening downbeat is a grid point too: on the first moving tick
        // start at beat 0 inclusive.
        let mut g = if self.prev_beats <= 0.0 && self.beats > 0.0 {
            0.0
        } else {
            Self::next_grid(self.prev_beats + 1e-9, grid)
        };
        while g <= self.beats + 1e-9 && out.len() < 64 {
            out.push(g);
            g += grid;
        }
        out
    }

    /// Swing: offbeat eighths are pushed late by up to a third of an eighth.
    pub fn swing_offset(&self, beat: f64) -> f64 {
        let frac = beat - beat.floor();
        if (frac - 0.5).abs() < 1e-6 {
            self.swing * (1.0 / 6.0)
        } else {
            0.0
        }
    }

    /// Seconds from now until `target_beat` (swing applied), never negative.
    pub fn delay_to(&self, target_beat: f64) -> f64 {
        let t = target_beat + self.swing_offset(target_beat);
        ((t - self.beats) * self.seconds_per_beat()).max(0.0)
    }

    /// Position of a beat within its bar (0 ≤ x < beats_per_bar).
    pub fn in_bar(&self, beat: f64) -> f64 {
        beat.rem_euclid(self.beats_per_bar as f64)
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ConductorConfig {
        ConductorConfig {
            tempo: 60.0,
            tempo_range: [40.0, 80.0],
            beats_per_bar: 4,
            ..Default::default()
        }
    }

    #[test]
    fn advances_beats_and_reports_tempo_once() {
        let mut t = Transport::new(&cfg());
        assert_eq!(t.tick(0.5, 0.5), Some(60.0));
        assert!((t.beats() - 0.5).abs() < 1e-9);
        assert_eq!(t.tick(0.5, 0.5), None);
        let p = t.pos();
        assert_eq!((p.bar, p.beat), (0, 1));
        assert!(p.phase.abs() < 1e-9);
    }

    #[test]
    fn energy_pulls_tempo_slowly_within_range() {
        let mut t = Transport::new(&cfg());
        for _ in 0..(600 * 50) {
            t.tick(0.02, 1.0);
        }
        assert!(t.bpm() > 79.0 && t.bpm() <= 80.0, "{}", t.bpm());
        let mut t = Transport::new(&cfg());
        t.tick(1.0, 1.0);
        assert!(t.bpm() < 61.0, "tempo should move slowly: {}", t.bpm());
    }

    #[test]
    fn grid_math() {
        assert!((Transport::next_grid(1.01, 0.25) - 1.25).abs() < 1e-9);
        assert!((Transport::next_grid(1.0, 0.25) - 1.0).abs() < 1e-9);
        let mut t = Transport::new(&cfg());
        t.tick(1.0, 0.5); // 0 → 1 beat, including the downbeat itself
        assert_eq!(t.grid_points(0.5), vec![0.0, 0.5, 1.0]);
        t.tick(0.02, 0.5);
        assert!(t.grid_points(0.5).is_empty());
    }

    #[test]
    fn swing_delays_offbeats() {
        let mut c = cfg();
        c.swing = 1.0;
        let t = Transport::new(&c);
        assert!(t.swing_offset(2.5) > 0.1);
        assert_eq!(t.swing_offset(2.0), 0.0);
    }
}
