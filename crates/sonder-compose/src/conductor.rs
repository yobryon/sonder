//! The conductor: mood, key, mode, chord — and the story of why.

use std::collections::BTreeMap;

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sonder_core::{
    map_keys, parse_pitch_class, ConductorConfig, Drift, HarmonySnapshot, Mood, Progression,
};
use sonder_theory::{
    diatonic_chord, mode_by_name, modes_by_brightness, pitch_class_name, progression, Chord, Scale,
};

use crate::form::FormTargets;
use crate::ComposeError;

/// What changed in the harmony this tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HarmonyChange {
    pub chord: bool,
    pub tonic: bool,
    pub mode: bool,
}

impl HarmonyChange {
    pub fn any(&self) -> bool {
        self.chord || self.tonic || self.mode
    }
}

/// The harmonic state every generator reads.
#[derive(Debug, Clone)]
pub struct Harmony {
    pub scale: Scale,
    pub chord: Chord,
    pub scale_pcs: Vec<u8>,
    pub chord_pcs: Vec<u8>,
    pub change: HarmonyChange,
    pub beats_to_change: f64,
}

impl Harmony {
    fn refresh(&mut self) {
        self.scale_pcs = self.scale.pitch_classes();
        self.chord_pcs = self.chord.pitch_classes(&self.scale);
    }
    /// Root pitch class of the current chord.
    pub fn root_pc(&self) -> u8 {
        self.chord.root_pc(&self.scale)
    }
    pub fn chord_name(&self) -> String {
        self.chord.name(&self.scale)
    }
}

/// One layer's say in the mood, this tick.
pub struct MoodSource<'a> {
    pub name: &'a str,
    pub weight: f64,
    pub params: &'a BTreeMap<String, f64>,
    pub signals: &'a BTreeMap<String, f64>,
}

#[derive(Debug)]
pub struct Conductor {
    cfg: ConductorConfig,
    rng: ChaCha8Rng,
    mood: Mood,
    trend: Mood,
    energy_driver: String,
    pub harmony: Harmony,
    chord_degree: u8,
    /// Beats per chord from the scene.
    base_chord_beats: f64,
    /// Beats per chord in effect (the form's chord-change rate applied).
    chord_beats: f64,
    next_change_beat: f64,
    home_mode: &'static str,
    last_mode_change_beat: f64,
    mode_cooldown_bars: f64,
    force_change: bool,
    pending: HarmonyChange,
    /// The form's targets and its strength, blended into tension and chord rate.
    form: Option<(FormTargets, f64)>,
}

impl Conductor {
    pub fn new(cfg: &ConductorConfig, seed: u64) -> Result<Self, ComposeError> {
        let tonic =
            parse_pitch_class(&cfg.key).ok_or_else(|| ComposeError::UnknownKey(cfg.key.clone()))?;
        let mode =
            mode_by_name(&cfg.mode).ok_or_else(|| ComposeError::UnknownMode(cfg.mode.clone()))?;
        let home_mode = mode.name;
        let scale = Scale::new(tonic, mode);
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let baseline = Mood {
            energy: cfg.mood_baseline[0],
            tension: cfg.mood_baseline[1],
            brightness: cfg.mood_baseline[2],
        };
        let chord = diatonic_chord(&scale, 0, Self::extensions_for(baseline.tension));
        let bpb = cfg.beats_per_bar.max(1) as f64;
        let chord_beats = (cfg.bars_per_chord.max(0.25)) * bpb;
        let mut harmony = Harmony {
            scale,
            chord,
            scale_pcs: Vec::new(),
            chord_pcs: Vec::new(),
            change: HarmonyChange {
                chord: true,
                tonic: true,
                mode: true,
            },
            beats_to_change: chord_beats,
        };
        harmony.refresh();
        let cooldown = Self::roll_cooldown(cfg.drift, &mut rng);
        Ok(Self {
            cfg: cfg.clone(),
            rng,
            mood: baseline,
            trend: baseline,
            energy_driver: String::new(),
            harmony,
            chord_degree: 0,
            base_chord_beats: chord_beats,
            chord_beats,
            next_change_beat: chord_beats,
            home_mode,
            last_mode_change_beat: 0.0,
            mode_cooldown_bars: cooldown,
            force_change: false,
            pending: HarmonyChange::default(),
            form: None,
        })
    }

    /// The mood as the music hears it: tension blended toward the form's target.
    pub fn mood(&self) -> Mood {
        let mut m = self.mood;
        if let Some((t, s)) = self.form {
            m.tension = crate::form::blend(m.tension, t.tension, s);
        }
        m
    }

    /// Tell the conductor what the form wants (None = data only).
    pub fn set_form(&mut self, form: Option<(FormTargets, f64)>) {
        self.form = form;
    }

    /// Beats per chord in effect: the scene's `bars_per_chord` scaled by the
    /// form's chord-change rate (0.5 = as written, 1 = twice as fast, 0 = half).
    fn effective_chord_beats(&self) -> f64 {
        match self.form {
            Some((t, s)) => {
                let factor =
                    2f64.powf((0.5 - t.chord_rate.clamp(0.0, 1.0)) * 2.0 * s.clamp(0.0, 1.0));
                (self.base_chord_beats * factor).max(0.5)
            }
            None => self.base_chord_beats,
        }
    }

    fn extensions_for(tension: f64) -> u8 {
        if tension < 0.35 {
            0
        } else if tension < 0.7 {
            1
        } else {
            2
        }
    }

    fn roll_cooldown(drift: Drift, rng: &mut ChaCha8Rng) -> f64 {
        let (lo, hi) = match drift {
            Drift::Static => (f64::INFINITY, f64::INFINITY),
            Drift::Slow => (8.0, 16.0),
            Drift::Medium => (4.0, 8.0),
            Drift::Fast => (2.0, 4.0),
        };
        if lo.is_infinite() {
            return lo;
        }
        lo + (hi - lo) * rng.random::<f64>()
    }

    /// Aggregate layer contributions into the mood and smooth them.
    pub fn update_mood(&mut self, sources: &[MoodSource<'_>], dt: f64) {
        let mut sum = [0.0f64; 3];
        let mut wsum = [0.0f64; 3];
        let mut best_driver: Option<(f64, String)> = None;
        for s in sources {
            if s.weight <= 0.0 {
                continue;
            }
            for (i, key) in [map_keys::ENERGY, map_keys::TENSION, map_keys::BRIGHTNESS]
                .iter()
                .enumerate()
            {
                if let Some(v) = s.params.get(*key).copied().filter(|v| v.is_finite()) {
                    let v = v.clamp(0.0, 1.0);
                    sum[i] += v * s.weight;
                    wsum[i] += s.weight;
                    if i == 0 {
                        let score = v * s.weight;
                        if best_driver.as_ref().is_none_or(|(b, _)| score > *b) {
                            // Name the signal that feeds `energy` when one matches exactly.
                            let sig = s
                                .signals
                                .iter()
                                .find(|(_, sv)| (**sv - v).abs() < 1e-9)
                                .map(|(k, _)| k.as_str())
                                .unwrap_or("energy");
                            best_driver = Some((score, format!("{}.{}", s.name, sig)));
                        }
                    }
                }
            }
        }
        let target = Mood {
            energy: if wsum[0] > 0.0 {
                sum[0] / wsum[0]
            } else {
                self.cfg.mood_baseline[0]
            },
            tension: if wsum[1] > 0.0 {
                sum[1] / wsum[1]
            } else {
                self.cfg.mood_baseline[1]
            },
            brightness: if wsum[2] > 0.0 {
                sum[2] / wsum[2]
            } else {
                self.cfg.mood_baseline[2]
            },
        };
        self.energy_driver = best_driver.map(|(_, n)| n).unwrap_or_default();
        let tau = self.cfg.mood_smoothing.max(0.05);
        let a = 1.0 - (-dt / tau).exp();
        let a_slow = 1.0 - (-dt / (tau * 4.0)).exp();
        self.mood.energy += (target.energy - self.mood.energy) * a;
        self.mood.tension += (target.tension - self.mood.tension) * a;
        self.mood.brightness += (target.brightness - self.mood.brightness) * a;
        self.trend.energy += (self.mood.energy - self.trend.energy) * a_slow;
        self.trend.tension += (self.mood.tension - self.trend.tension) * a_slow;
        self.trend.brightness += (self.mood.brightness - self.trend.brightness) * a_slow;
    }

    /// Advance harmony to `beats`. Chord changes happen on chord boundaries;
    /// mode/tonic drift rides along with them.
    pub fn tick(&mut self, beats: f64) -> HarmonyChange {
        let mut change = std::mem::take(&mut self.pending);
        if self.force_change || beats + 1e-9 >= self.next_change_beat {
            self.force_change = false;
            self.advance_chord(beats);
            change.chord = true;
            if self.maybe_drift(beats) {
                change.mode = self.harmony.change.mode || change.mode;
                change.tonic = self.harmony.change.tonic || change.tonic;
            }
            // Realign the next boundary to the chord grid (at the form's rate).
            self.chord_beats = self.effective_chord_beats();
            let mut next = self.next_change_beat;
            while next <= beats + 1e-9 {
                next += self.chord_beats;
            }
            self.next_change_beat = next;
        }
        if change.any() {
            self.harmony.chord = diatonic_chord(
                &self.harmony.scale,
                self.chord_degree,
                Self::extensions_for(self.mood().tension),
            );
            self.harmony.refresh();
        }
        self.harmony.change = change;
        self.harmony.beats_to_change = (self.next_change_beat - beats).max(0.0);
        change
    }

    fn advance_chord(&mut self, _beats: f64) {
        let tension = self.mood().tension;
        let n = self.harmony.scale.len() as u8;
        self.chord_degree = match self.cfg.progression {
            Progression::Static => self.chord_degree,
            Progression::Functional => {
                progression::functional_next(self.chord_degree, tension, &mut self.rng) % n.max(1)
            }
            Progression::Modal => {
                progression::modal_next(self.chord_degree, tension, &mut self.rng) % n.max(1)
            }
        };
    }

    /// Mode index (brightness order) the mood is asking for.
    fn desired_mode_index(&self) -> Option<usize> {
        let order = modes_by_brightness();
        let home = order.iter().position(|&m| m == self.home_mode)?;
        let offset = ((self.mood.brightness - 0.5) / 0.25).round() as i32;
        let max = match self.cfg.drift {
            Drift::Fast => 3,
            _ => 2,
        };
        Some((home as i32 + offset.clamp(-max, max)).clamp(0, order.len() as i32 - 1) as usize)
    }

    fn current_mode_index(&self) -> Option<usize> {
        modes_by_brightness()
            .iter()
            .position(|&m| m == self.harmony.scale.mode.name)
    }

    /// Returns true if the mode or tonic changed. Sets `harmony.change` bits.
    fn maybe_drift(&mut self, beats: f64) -> bool {
        self.harmony.change = HarmonyChange::default();
        if self.cfg.drift == Drift::Static || !self.harmony.scale.mode.is_diatonic() {
            return false;
        }
        let bpb = self.cfg.beats_per_bar.max(1) as f64;
        let bars_since = (beats - self.last_mode_change_beat) / bpb;
        if bars_since < self.mode_cooldown_bars {
            return false;
        }
        let (Some(desired), Some(current)) = (self.desired_mode_index(), self.current_mode_index())
        else {
            return false;
        };
        if desired != current {
            if let Some(next) =
                progression::modal_shift(self.harmony.scale.mode.name, desired > current)
            {
                if let Some(mode) = mode_by_name(next) {
                    self.harmony.scale.mode = mode;
                    self.harmony.change.mode = true;
                }
            }
        } else if self.cfg.drift == Drift::Fast && self.rng.random::<f64>() < 0.15 {
            let up = self.rng.random::<f64>() < 0.5;
            let t = self.harmony.scale.tonic as i32 + if up { 7 } else { -7 };
            self.harmony.scale.tonic = t.rem_euclid(12) as u8;
            self.harmony.change.tonic = true;
        } else {
            return false;
        }
        self.last_mode_change_beat = beats;
        self.mode_cooldown_bars = Self::roll_cooldown(self.cfg.drift, &mut self.rng);
        true
    }

    /// Force a chord change on the next tick.
    pub fn next_chord(&mut self) {
        self.force_change = true;
    }

    /// Move the tonic by semitones (takes effect next tick).
    pub fn transpose(&mut self, semitones: i32) {
        let t = self.harmony.scale.tonic as i32 + semitones;
        self.harmony.scale.tonic = t.rem_euclid(12) as u8;
        self.pending.tonic = true;
    }

    /// Step to the next (or previous) diatonic mode in brightness order,
    /// wrapping around. Non-diatonic scales jump to the nearest diatonic one.
    pub fn step_mode(&mut self, backwards: bool) {
        let order = modes_by_brightness();
        let idx = self.current_mode_index().unwrap_or(3);
        let next = if backwards {
            (idx + order.len() - 1) % order.len()
        } else {
            (idx + 1) % order.len()
        };
        if let Some(mode) = mode_by_name(order[next]) {
            self.home_mode = mode.name;
            self.harmony.scale.mode = mode;
            self.pending.mode = true;
        }
    }

    /// Fill the harmony half of a snapshot.
    pub fn fill(&self, snap: &mut HarmonySnapshot) {
        let h = &self.harmony;
        snap.tonic = h.scale.tonic;
        snap.key_name = pitch_class_name(h.scale.tonic, h.scale.prefers_flats()).to_string();
        snap.mode = h.scale.mode.name.to_string();
        snap.scale = h.scale_pcs.clone();
        snap.chord = h.chord_pcs.clone();
        snap.chord_name = h.chord_name();
        snap.chord_degree = self.chord_degree + 1;
        snap.beats_to_change = h.beats_to_change;
    }

    /// A one-line explanation of what the conductor is doing and why.
    pub fn note(&self, bpm: f64) -> String {
        let arrow = |now: f64, trend: f64| {
            if now > trend + 0.01 {
                "↑"
            } else if now < trend - 0.01 {
                "↓"
            } else {
                "→"
            }
        };
        let driver = if self.energy_driver.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.energy_driver)
        };
        let bars = self.chord_beats / self.cfg.beats_per_bar.max(1) as f64;
        let bars_s = if (bars - bars.round()).abs() < 1e-9 {
            format!("{}", bars as i64)
        } else {
            format!("{bars:.1}")
        };
        let mut s = format!(
            "energy {:.2} {}{} → tempo {:.0}; tension {:.2} → {} for {} bar{}",
            self.mood.energy,
            arrow(self.mood.energy, self.trend.energy),
            driver,
            bpm,
            self.mood.tension,
            self.harmony.chord_name(),
            bars_s,
            if bars_s == "1" { "" } else { "s" }
        );
        let mode = self.harmony.scale.mode.name;
        match self.cfg.drift {
            Drift::Static => s.push_str(&format!("; {} pinned (drift static)", mode)),
            _ => match (self.desired_mode_index(), self.current_mode_index()) {
                (Some(d), Some(c)) if d != c => {
                    let bpb = self.cfg.beats_per_bar.max(1) as f64;
                    let bars_since =
                        (self.harmony_beats_estimate() - self.last_mode_change_beat) / bpb;
                    let left = (self.mode_cooldown_bars - bars_since).max(0.0).ceil();
                    let dir = if d > c { "rising" } else { "falling" };
                    let target = modes_by_brightness()[if d > c { c + 1 } else { c - 1 }];
                    if left < 1.0 {
                        s.push_str(&format!("; brightness {dir} → {target} at the next chord"));
                    } else {
                        s.push_str(&format!("; brightness {dir} → {target} in ~{left:.0} bars"));
                    }
                }
                _ => s.push_str(&format!(
                    "; brightness {:.2} {} → {}",
                    self.mood.brightness,
                    arrow(self.mood.brightness, self.trend.brightness),
                    mode
                )),
            },
        }
        s
    }

    /// Beats "now" as far as the conductor knows (the last boundary minus what's left).
    fn harmony_beats_estimate(&self) -> f64 {
        self.next_change_beat - self.harmony.beats_to_change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(drift: Drift, prog: Progression) -> ConductorConfig {
        ConductorConfig {
            key: "D".into(),
            mode: "lydian".into(),
            drift,
            progression: prog,
            bars_per_chord: 1.0,
            beats_per_bar: 4,
            mood_smoothing: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn chords_change_on_boundaries_only() {
        let mut c = Conductor::new(&cfg(Drift::Static, Progression::Functional), 3).unwrap();
        let ch = c.tick(0.0);
        assert!(!ch.chord, "{ch:?}");
        assert!(!c.tick(3.9).chord);
        assert!(c.tick(4.0).chord);
        assert!(!c.tick(4.1).chord);
        assert!((c.harmony.beats_to_change - 3.9).abs() < 1e-9);
        c.next_chord();
        assert!(c.tick(4.2).chord);
        assert!(c
            .harmony
            .chord_pcs
            .iter()
            .all(|pc| c.harmony.scale_pcs.contains(pc)));
    }

    #[test]
    fn static_drift_never_moves_key() {
        let mut c = Conductor::new(&cfg(Drift::Static, Progression::Modal), 1).unwrap();
        let bright = BTreeMap::from([(map_keys::BRIGHTNESS.to_string(), 1.0)]);
        let sig = BTreeMap::new();
        for i in 0..5000 {
            c.update_mood(
                &[MoodSource {
                    name: "a",
                    weight: 1.0,
                    params: &bright,
                    signals: &sig,
                }],
                0.02,
            );
            c.tick(i as f64 * 0.02 * 2.0);
        }
        assert_eq!(c.harmony.scale.mode.name, "lydian");
        assert_eq!(c.harmony.scale.tonic, 2);
    }

    #[test]
    fn brightness_drives_modal_shift_one_step_at_a_time() {
        let mut k = cfg(Drift::Medium, Progression::Modal);
        k.mode = "ionian".into();
        let mut c = Conductor::new(&k, 1).unwrap();
        let dark = BTreeMap::from([(map_keys::BRIGHTNESS.to_string(), 0.0)]);
        let sig = BTreeMap::new();
        let mut seen = vec![c.harmony.scale.mode.name];
        let mut beats = 0.0;
        for _ in 0..20000 {
            c.update_mood(
                &[MoodSource {
                    name: "a",
                    weight: 1.0,
                    params: &dark,
                    signals: &sig,
                }],
                0.02,
            );
            beats += 0.02 * 2.0;
            let ch = c.tick(beats);
            if ch.mode {
                seen.push(c.harmony.scale.mode.name);
            }
        }
        assert_eq!(
            seen,
            vec!["ionian", "mixolydian", "dorian"],
            "two steps darker, one at a time"
        );
    }

    #[test]
    fn mood_aggregation_uses_weights_and_baseline() {
        let mut c = Conductor::new(&cfg(Drift::Slow, Progression::Modal), 1).unwrap();
        let a = BTreeMap::from([(map_keys::ENERGY.to_string(), 1.0)]);
        let b = BTreeMap::from([(map_keys::ENERGY.to_string(), 0.0)]);
        let sig_a = BTreeMap::from([("density".to_string(), 1.0)]);
        let sig = BTreeMap::new();
        for _ in 0..2000 {
            c.update_mood(
                &[
                    MoodSource {
                        name: "loud",
                        weight: 3.0,
                        params: &a,
                        signals: &sig_a,
                    },
                    MoodSource {
                        name: "quiet",
                        weight: 1.0,
                        params: &b,
                        signals: &sig,
                    },
                ],
                0.02,
            );
        }
        assert!((c.mood().energy - 0.75).abs() < 0.01);
        assert!(
            (c.mood().tension - 0.3).abs() < 1e-6,
            "baseline when nobody contributes"
        );
        assert!(c.note(70.0).contains("(loud.density)"), "{}", c.note(70.0));
    }

    #[test]
    fn user_mode_and_transpose() {
        let mut c = Conductor::new(&cfg(Drift::Slow, Progression::Modal), 1).unwrap();
        c.step_mode(true); // darker
        let ch = c.tick(0.5);
        assert!(ch.mode && !ch.chord);
        assert_eq!(c.harmony.scale.mode.name, "ionian");
        c.step_mode(false); // brighter: lydian
        c.step_mode(false); // brighter still: wraps to locrian
        c.tick(0.6);
        assert_eq!(c.harmony.scale.mode.name, "locrian", "wraps around");
        c.transpose(-2);
        assert!(c.tick(0.7).tonic);
        assert_eq!(c.harmony.scale.tonic, 0);
        let mut snap = HarmonySnapshot::default();
        c.fill(&mut snap);
        assert_eq!(snap.key_name, "C");
        assert_eq!(snap.chord_degree, 1);
    }
}
