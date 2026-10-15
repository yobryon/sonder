//! The form engine: how a piece is shaped over minutes.
//!
//! `Arc` cycles intro → build → peak → release, each cycle roughly
//! `arc_minutes` long (±30 %, seeded) and steered by the mood's energy: a hot
//! room gets through the intro and build faster, a very quiet one lingers in
//! the release. `Sections` alternates A / B / A' / B' of `section_bars`.
//! `Static` is one open-ended section.
//!
//! Every section publishes *targets* in 0..1 (density, register, tension,
//! chord-change rate, dynamics) that the composer blends with the data-driven
//! parameters by `form.strength`.

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sonder_core::{FormConfig, FormSnapshot, FormStyle};

/// Where the energy starts hurrying the intro / build.
const HURRY_ENERGY: f64 = 0.6;
/// Below this energy the release stalls (advances at a quarter speed).
const STALL_ENERGY: f64 = 0.15;
/// Arc cycle length jitter, as a fraction of `arc_minutes`.
const ARC_JITTER: f64 = 0.3;
/// Fractions of an arc spent in intro / build / peak / release.
const ARC_SHAPE: [f64; 4] = [0.2, 0.3, 0.2, 0.3];

/// A named part of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Intro,
    Build,
    Peak,
    Release,
    A,
    B,
    APrime,
    BPrime,
    Static,
}

impl Section {
    pub fn name(self) -> &'static str {
        match self {
            Section::Intro => "intro",
            Section::Build => "build",
            Section::Peak => "peak",
            Section::Release => "release",
            Section::A => "A",
            Section::B => "B",
            Section::APrime => "A'",
            Section::BPrime => "B'",
            Section::Static => "static",
        }
    }

    /// Is this a winding-down section (voices thin out)?
    pub fn is_release(self) -> bool {
        matches!(self, Section::Release)
    }

    fn cycle(style: FormStyle) -> &'static [Section] {
        match style {
            FormStyle::Arc => &[
                Section::Intro,
                Section::Build,
                Section::Peak,
                Section::Release,
            ],
            FormStyle::Sections => &[Section::A, Section::B, Section::APrime, Section::BPrime],
            FormStyle::Static => &[Section::Static],
        }
    }
}

/// What the form asks of the music right now, all in 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FormTargets {
    pub density: f64,
    pub register: f64,
    pub tension: f64,
    /// 0.5 = the scene's `bars_per_chord`; 1 = twice as fast, 0 = half as fast.
    pub chord_rate: f64,
    pub dynamics: f64,
}

impl FormTargets {
    const NEUTRAL: FormTargets = FormTargets {
        density: 0.5,
        register: 0.5,
        tension: 0.5,
        chord_rate: 0.5,
        dynamics: 0.5,
    };

    fn lerp(a: FormTargets, b: FormTargets, t: f64) -> FormTargets {
        let t = t.clamp(0.0, 1.0);
        let l = |x: f64, y: f64| x + (y - x) * t;
        FormTargets {
            density: l(a.density, b.density),
            register: l(a.register, b.register),
            tension: l(a.tension, b.tension),
            chord_rate: l(a.chord_rate, b.chord_rate),
            dynamics: l(a.dynamics, b.dynamics),
        }
    }

    /// Targets for a section at fraction `frac` (0..1) of the way through it.
    fn for_section(section: Section, frac: f64) -> FormTargets {
        let t = |density, register, tension, chord_rate, dynamics| FormTargets {
            density,
            register,
            tension,
            chord_rate,
            dynamics,
        };
        match section {
            Section::Intro => t(0.2, 0.45, 0.2, 0.3, 0.3),
            Section::Build => Self::lerp(
                t(0.4, 0.5, 0.35, 0.5, 0.5),
                t(0.8, 0.6, 0.7, 0.7, 0.8),
                frac,
            ),
            Section::Peak => t(0.9, 0.65, 0.8, 0.8, 0.9),
            Section::Release => Self::lerp(
                t(0.5, 0.5, 0.35, 0.4, 0.5),
                t(0.15, 0.45, 0.1, 0.25, 0.2),
                frac,
            ),
            Section::A => t(0.5, 0.5, 0.4, 0.5, 0.5),
            Section::B => t(0.7, 0.6, 0.6, 0.7, 0.7),
            Section::APrime => t(0.55, 0.5, 0.45, 0.5, 0.6),
            Section::BPrime => t(0.8, 0.65, 0.7, 0.7, 0.8),
            Section::Static => Self::NEUTRAL,
        }
    }
}

/// Blend of a data value with a form target by the form's strength.
pub fn blend(data: f64, target: f64, strength: f64) -> f64 {
    data + (target - data) * strength.clamp(0.0, 1.0)
}

/// Musical form state. Advances by bars; see the module docs.
#[derive(Debug)]
pub struct Form {
    cfg: FormConfig,
    rng: ChaCha8Rng,
    /// Index into the style's section cycle.
    index: usize,
    /// Planned bars for every section of the current cycle (0 = open-ended).
    plan: Vec<u32>,
    /// Bars into the current section, fractional (energy speeds up / stalls).
    progress: f64,
    /// Bars completed in earlier sections of this cycle.
    bars_before: f64,
    last_bar: Option<u64>,
    jump: bool,
    /// Sections completed since the start (for the UI and tests).
    transitions: u32,
}

impl Form {
    pub fn new(cfg: &FormConfig, seed: u64, bpm: f64, beats_per_bar: u8) -> Self {
        let mut cfg = cfg.clone();
        cfg.strength = if cfg.strength.is_finite() {
            cfg.strength.clamp(0.0, 1.0)
        } else {
            0.5
        };
        cfg.motif_recall = if cfg.motif_recall.is_finite() {
            cfg.motif_recall.clamp(0.0, 1.0)
        } else {
            0.4
        };
        cfg.arc_minutes = if cfg.arc_minutes.is_finite() && cfg.arc_minutes > 0.0 {
            cfg.arc_minutes
        } else {
            6.0
        };
        let mut f = Self {
            cfg,
            rng: ChaCha8Rng::seed_from_u64(seed ^ 0x5f0a_b3c1_9e24_77d1),
            index: 0,
            plan: Vec::new(),
            progress: 0.0,
            bars_before: 0.0,
            last_bar: None,
            jump: false,
            transitions: 0,
        };
        f.plan_cycle(bpm, beats_per_bar);
        f
    }

    pub fn style(&self) -> FormStyle {
        self.cfg.style
    }
    /// 0..1 how strongly form overrides data.
    pub fn strength(&self) -> f64 {
        self.cfg.strength
    }
    pub fn motif_recall(&self) -> f64 {
        self.cfg.motif_recall
    }
    pub fn call_and_response(&self) -> bool {
        self.cfg.call_and_response
    }
    /// Sections completed since the start.
    pub fn transitions(&self) -> u32 {
        self.transitions
    }

    pub fn section(&self) -> Section {
        Section::cycle(self.cfg.style)[self.index]
    }

    /// Bars elapsed in the current section.
    pub fn bar_in_section(&self) -> u32 {
        self.progress.floor() as u32
    }

    /// Planned bars for the current section (0 = open-ended).
    pub fn bars_in_section(&self) -> u32 {
        self.plan.get(self.index).copied().unwrap_or(0)
    }

    /// 0..1 position within the current section.
    fn frac(&self) -> f64 {
        let bars = self.bars_in_section();
        if bars == 0 {
            0.0
        } else {
            (self.progress / bars as f64).clamp(0.0, 1.0)
        }
    }

    /// 0..1 position along the whole cycle (intro start → release end).
    pub fn arc(&self) -> f64 {
        let total: f64 = self.plan.iter().map(|&b| b as f64).sum();
        if total <= 0.0 {
            return 0.0;
        }
        ((self.bars_before + self.progress) / total).clamp(0.0, 1.0)
    }

    /// Current targets, or None when the form has nothing to say (`Static`).
    pub fn targets(&self) -> Option<FormTargets> {
        if self.cfg.style == FormStyle::Static {
            return None;
        }
        Some(FormTargets::for_section(self.section(), self.frac()))
    }

    /// Strength-weighted "thin out" amount for release sections, 0..1.
    pub fn thin(&self) -> f64 {
        if self.section().is_release() {
            self.cfg.strength * (0.5 + 0.5 * self.frac())
        } else {
            0.0
        }
    }

    /// Jump to the next section at the next bar.
    pub fn request_next(&mut self) {
        self.jump = true;
    }

    fn plan_cycle(&mut self, bpm: f64, beats_per_bar: u8) {
        self.plan = match self.cfg.style {
            FormStyle::Static => vec![0],
            FormStyle::Sections => vec![self.cfg.section_bars.max(1); 4],
            FormStyle::Arc => {
                let bpm = if bpm.is_finite() && bpm > 0.0 {
                    bpm
                } else {
                    70.0
                };
                let bars_total = self.cfg.arc_minutes * bpm / beats_per_bar.max(1) as f64;
                let scale = 1.0 + ARC_JITTER * (self.rng.random::<f64>() * 2.0 - 1.0);
                ARC_SHAPE
                    .iter()
                    .map(|f| (bars_total * scale * f).round().max(2.0) as u32)
                    .collect()
            }
        };
        self.bars_before = 0.0;
    }

    fn advance_section(&mut self, bpm: f64, beats_per_bar: u8) {
        self.bars_before += self.bars_in_section() as f64;
        self.index += 1;
        self.progress = 0.0;
        self.transitions += 1;
        if self.index >= Section::cycle(self.cfg.style).len() {
            self.index = 0;
            self.plan_cycle(bpm, beats_per_bar);
        }
    }

    /// Advance the form to transport bar `bar`. Call every tick; the form
    /// only moves when the bar changes (or a jump was requested).
    pub fn tick(&mut self, bar: u64, bpm: f64, beats_per_bar: u8, energy: f64) {
        let Some(last) = self.last_bar else {
            self.last_bar = Some(bar);
            return;
        };
        if bar == last {
            return;
        }
        let new_bars = bar.saturating_sub(last).min(8);
        self.last_bar = Some(bar);
        if self.jump {
            self.jump = false;
            if self.cfg.style != FormStyle::Static {
                self.advance_section(bpm, beats_per_bar);
                return;
            }
        }
        let bars = self.bars_in_section();
        if bars == 0 {
            self.progress += new_bars as f64;
            return;
        }
        let e = energy.clamp(0.0, 1.0);
        let rate = match self.section() {
            Section::Intro | Section::Build => {
                1.0 + ((e - HURRY_ENERGY) / (1.0 - HURRY_ENERGY)).max(0.0)
            }
            Section::Release if e < STALL_ENERGY => 0.25,
            _ => 1.0,
        };
        self.progress += rate * new_bars as f64;
        if self.progress + 1e-9 >= bars as f64 {
            self.advance_section(bpm, beats_per_bar);
        }
    }

    /// Fill the form half of a snapshot (motif and note are the composer's).
    pub fn fill(&self, snap: &mut FormSnapshot) {
        snap.section = self.section().name().to_string();
        snap.bar_in_section = self.bar_in_section();
        snap.bars_in_section = self.bars_in_section();
        snap.arc = self.arc();
    }

    /// "build 5/24" or "static" — for the conductor's note.
    pub fn describe(&self) -> String {
        let bars = self.bars_in_section();
        if bars == 0 {
            self.section().name().to_string()
        } else {
            format!(
                "{} {}/{}",
                self.section().name(),
                self.bar_in_section() + 1,
                bars
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(style: FormStyle) -> FormConfig {
        FormConfig {
            style,
            arc_minutes: 2.0,
            section_bars: 4,
            strength: 0.5,
            ..Default::default()
        }
    }

    fn run(form: &mut Form, bars: u64, energy: f64) -> Vec<Section> {
        let mut seen = vec![form.section()];
        for bar in 0..bars {
            form.tick(bar, 60.0, 4, energy);
            if seen.last() != Some(&form.section()) {
                seen.push(form.section());
            }
        }
        seen
    }

    #[test]
    fn arc_cycles_in_order_and_sums_to_about_arc_minutes() {
        // 2 minutes at 60 bpm in 4/4 = 30 bars per cycle, ±30 %.
        let mut f = Form::new(&cfg(FormStyle::Arc), 1, 60.0, 4);
        let total: u32 = f.plan.iter().sum();
        assert!((21..=39).contains(&total), "{total}");
        let seen = run(&mut f, 200, 0.5);
        assert!(seen.len() >= 8);
        let order = [
            Section::Intro,
            Section::Build,
            Section::Peak,
            Section::Release,
        ];
        for (i, s) in seen.iter().enumerate() {
            assert_eq!(*s, order[i % 4], "{seen:?}");
        }
        assert_eq!(f.transitions() as usize, seen.len() - 1);
    }

    #[test]
    fn energy_hurries_and_stalls() {
        let hot = run(&mut Form::new(&cfg(FormStyle::Arc), 1, 60.0, 4), 100, 1.0);
        let cool = run(&mut Form::new(&cfg(FormStyle::Arc), 1, 60.0, 4), 100, 0.5);
        assert!(
            hot.len() > cool.len(),
            "hot {} cool {}",
            hot.len(),
            cool.len()
        );
        // A dead-quiet room never leaves the release once it gets there.
        let mut f = Form::new(&cfg(FormStyle::Arc), 1, 60.0, 4);
        let mut bar = 0;
        while f.section() != Section::Release {
            f.tick(bar, 60.0, 4, 0.5);
            bar += 1;
        }
        let release_bars = f.bars_in_section();
        for _ in 0..(release_bars * 3) {
            f.tick(bar, 60.0, 4, 0.0);
            bar += 1;
        }
        assert_eq!(f.section(), Section::Release, "stalled release");
        assert!(f.arc() > 0.7 && f.arc() <= 1.0);
    }

    #[test]
    fn sections_alternate_and_jump_on_request() {
        let mut f = Form::new(&cfg(FormStyle::Sections), 3, 60.0, 4);
        let seen = run(&mut f, 17, 0.5);
        assert_eq!(
            seen,
            vec![
                Section::A,
                Section::B,
                Section::APrime,
                Section::BPrime,
                Section::A
            ]
        );
        assert_eq!(f.bars_in_section(), 4);
        f.request_next();
        f.tick(16, 60.0, 4, 0.5); // same bar: nothing yet
        assert_eq!(f.section(), Section::A);
        f.tick(17, 60.0, 4, 0.5);
        assert_eq!(f.section(), Section::B);
        assert_eq!(f.bar_in_section(), 0);
    }

    #[test]
    fn static_is_open_ended_and_has_no_targets() {
        let mut f = Form::new(&cfg(FormStyle::Static), 0, 60.0, 4);
        run(&mut f, 50, 0.9);
        assert_eq!(f.section(), Section::Static);
        assert_eq!(f.bars_in_section(), 0);
        assert!(f.targets().is_none());
        assert_eq!(f.describe(), "static");
        f.request_next();
        f.tick(51, 60.0, 4, 0.5);
        assert_eq!(f.section(), Section::Static);
    }

    #[test]
    fn targets_ramp_and_release_thins() {
        let mut f = Form::new(&cfg(FormStyle::Arc), 7, 60.0, 4);
        assert!(f.thin() == 0.0);
        let intro = f.targets().unwrap();
        let mut bar = 0;
        while f.section() != Section::Peak {
            f.tick(bar, 60.0, 4, 0.5);
            bar += 1;
        }
        let peak = f.targets().unwrap();
        assert!(peak.density > intro.density + 0.4);
        assert!(peak.chord_rate > intro.chord_rate);
        while f.section() != Section::Release {
            f.tick(bar, 60.0, 4, 0.5);
            bar += 1;
        }
        let early = f.targets().unwrap();
        f.tick(bar, 60.0, 4, 0.5);
        f.tick(bar + 1, 60.0, 4, 0.5);
        let later = f.targets().unwrap();
        assert!(later.density < early.density);
        assert!(f.thin() > 0.0);
        assert!((blend(0.2, 0.8, 0.5) - 0.5).abs() < 1e-9);
        assert!((blend(0.2, 0.8, 0.0) - 0.2).abs() < 1e-9);
        let mut snap = FormSnapshot::default();
        f.fill(&mut snap);
        assert_eq!(snap.section, "release");
        assert_eq!(snap.bar_in_section, 2);
    }
}
