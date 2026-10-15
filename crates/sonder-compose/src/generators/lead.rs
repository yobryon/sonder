//! Lead: a random walk through the scale, in phrases — and a memory for them.
//!
//! Every finished phrase of 2–6 notes is offered to the motif memory; at the
//! start of a phrase the lead may instead restate a remembered motif (plain,
//! transposed to a chord tone, inverted, augmented or backwards). Restated
//! notes carry `reason = "motif"`.

use rand::RngExt;
use sonder_core::{map_keys, NoteId, Role};
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};
use crate::motif::{Restatement, Transform};

/// `NoteSnapshot.reason` for restated motif notes.
pub const MOTIF_REASON: &str = "motif";

/// A motif being played back.
struct Playback {
    degrees: Vec<i32>,
    beats: Vec<f64>,
    pos: usize,
    /// Absolute scale index the motif's first note lands on.
    base: i32,
    /// Beat of the first note.
    start: f64,
}

pub struct Lead {
    /// Absolute scale-degree index of the last note (see `Scale::index_of_midi`).
    index: Option<i32>,
    notes_left_in_phrase: u32,
    rest_until_beat: f64,
    current: Option<NoteId>,
    /// Land on a chord tone for the first note after a chord change.
    land_on_chord_tone: bool,
    /// The phrase in progress: (absolute scale index, onset beat).
    phrase: Vec<(i32, f64)>,
    playback: Option<Playback>,
}

impl Default for Lead {
    fn default() -> Self {
        Self {
            index: None,
            notes_left_in_phrase: 0,
            rest_until_beat: 0.0,
            current: None,
            land_on_chord_tone: false,
            phrase: Vec::new(),
            playback: None,
        }
    }
}

impl Lead {
    /// Choose where a restatement starts so the whole shape fits the window.
    fn playback_for(
        &self,
        r: &Restatement,
        ctx: &mut GenCtx<'_>,
        (lo, hi): (i32, i32),
        (lo_i, hi_i): (i32, i32),
        start: f64,
    ) -> Playback {
        let centre = (lo_i + hi_i) / 2;
        let h = ctx.harmony;
        let mut base = match r.transform {
            Transform::Transpose => {
                let tones: Vec<i32> = Quantizer::notes_in_window(&h.chord_pcs, lo, hi)
                    .into_iter()
                    .filter_map(|m| h.scale.index_of_midi(m))
                    .collect();
                if tones.is_empty() {
                    centre
                } else {
                    tones[ctx.rng.random_range(0..tones.len())]
                }
            }
            _ => self.index.unwrap_or(centre),
        };
        let dmin = r.degrees.iter().copied().min().unwrap_or(0);
        let dmax = r.degrees.iter().copied().max().unwrap_or(0);
        let (blo, bhi) = (lo_i - dmin, hi_i - dmax);
        base = if blo <= bhi {
            base.clamp(blo, bhi)
        } else {
            centre
        };
        Playback {
            degrees: r.degrees.clone(),
            beats: r.beats.clone(),
            pos: 0,
            base,
            start,
        }
    }

    fn play(&mut self, ctx: &mut GenCtx<'_>, midi: i32, beats_long: f64, g: f64, reason: &str) {
        if let Some(prev) = self.current.take() {
            ctx.sink.release(prev);
        }
        let vel = 0.4
            + 0.4 * ctx.mood.energy
            + if ctx.transport.in_bar(g).fract() == 0.0 {
                0.08
            } else {
                0.0
            };
        let dur = beats_long * ctx.transport.seconds_per_beat();
        let delay = ctx.transport.delay_to(g);
        self.current = ctx
            .sink
            .play_as(midi as f64, vel, 0.5, Some(dur), delay, reason, None);
    }
}

impl Generator for Lead {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        if h.scale_pcs.is_empty() {
            return;
        }
        let density = ctx.params.density(0.5);
        let step_param = ctx.params.or(map_keys::STEP, 0.4);
        let energy = ctx.mood.energy;
        let (lo, hi) = register_window(Role::Lead, ctx.params.register());
        let sub = if density > 0.7 { 0.25 } else { 0.5 };
        let max_step = 1 + (step_param * 4.0).round() as i32;
        if h.change.chord && ctx.rng.random::<f64>() < 0.6 {
            self.land_on_chord_tone = true;
        }
        let lo_i = (lo..=hi)
            .find_map(|m| h.scale.index_of_midi(m))
            .unwrap_or(0);
        let hi_i = (lo..=hi)
            .rev()
            .find_map(|m| h.scale.index_of_midi(m))
            .unwrap_or(lo_i);
        let centre = (lo_i + hi_i) / 2;
        let section = ctx.cue.section;
        let rest_until = self.rest_until_beat.max(ctx.cue.rest_until_beat);

        for g in ctx.transport.grid_points(sub) {
            if g < rest_until {
                continue;
            }
            if self.playback.is_none() && self.notes_left_in_phrase == 0 {
                // Start a phrase (or decide to keep resting).
                let p_start = 0.15 + 0.6 * density;
                if ctx.rng.random::<f64>() > p_start {
                    self.rest_until_beat = g + 1.0 + ctx.rng.random::<f64>() * 2.0;
                    continue;
                }
                let recall = ctx.cue.motif_recall;
                if recall > 0.0 && !ctx.motifs.is_empty() && ctx.rng.random::<f64>() < recall {
                    if let Some(r) = ctx.motifs.recall(section, ctx.rng) {
                        self.playback = Some(self.playback_for(&r, ctx, (lo, hi), (lo_i, hi_i), g));
                    }
                }
                if self.playback.is_none() {
                    ctx.motifs.clear_current();
                    self.notes_left_in_phrase = 2 + ctx.rng.random_range(0..5);
                    self.phrase.clear();
                }
            }

            if let Some(pb) = &mut self.playback {
                let due = pb.start + (pb.beats[pb.pos] / sub).round() * sub;
                if g + 1e-6 < due {
                    continue;
                }
                let idx = (pb.base + pb.degrees[pb.pos]).clamp(lo_i, hi_i);
                let midi = h.scale.midi_from_index(idx);
                let last = pb.pos + 1 >= pb.degrees.len();
                let beats_long = if last {
                    sub * 3.0
                } else {
                    let gap = ((pb.beats[pb.pos + 1] - pb.beats[pb.pos]) / sub).round() * sub;
                    (gap * 0.95).max(sub * 0.5)
                };
                pb.pos += 1;
                self.index = Some(idx);
                if last {
                    self.playback = None;
                    self.rest_until_beat = g + beats_long + 1.0 + ctx.rng.random::<f64>() * 2.0;
                }
                self.play(ctx, midi, beats_long, g, MOTIF_REASON);
                continue;
            }

            // Probability of a note on this subdivision.
            let p = (0.3 + 0.7 * density) * (0.5 + 0.5 * energy);
            if ctx.rng.random::<f64>() > p {
                continue;
            }

            let mut idx = match self.index {
                Some(i) => {
                    let mut step = ctx.rng.random_range(1..=max_step);
                    // Lean back toward the middle of the window.
                    let up = if i > centre {
                        ctx.rng.random::<f64>() < 0.35
                    } else {
                        ctx.rng.random::<f64>() < 0.65
                    };
                    if !up {
                        step = -step;
                    }
                    (i + step).clamp(lo_i, hi_i)
                }
                None => centre,
            };
            let mut midi = h.scale.midi_from_index(idx);
            if self.land_on_chord_tone {
                self.land_on_chord_tone = false;
                // Nearest chord tone *inside the window* (clamping to the window
                // edge could land on a note outside the scale). Ties go lower.
                let nearest = Quantizer::notes_in_window(&h.chord_pcs, lo, hi)
                    .into_iter()
                    .min_by_key(|&m| ((m - midi).abs(), m));
                if let Some(m) = nearest {
                    midi = m;
                    idx = h.scale.index_of_midi(midi).unwrap_or(idx);
                }
            }
            self.index = Some(idx);
            self.notes_left_in_phrase -= 1;
            self.phrase.push((idx, g));

            let last_of_phrase = self.notes_left_in_phrase == 0;
            let beats_long = if last_of_phrase {
                sub * 3.0
            } else if ctx.rng.random::<f64>() < 0.2 {
                sub * 1.9
            } else {
                sub * 0.95
            };
            if last_of_phrase {
                self.rest_until_beat = g + beats_long + 1.0 + ctx.rng.random::<f64>() * 2.0;
                let indices: Vec<i32> = self.phrase.iter().map(|(i, _)| *i).collect();
                let onsets: Vec<f64> = self.phrase.iter().map(|(_, b)| *b).collect();
                ctx.motifs.remember(&indices, &onsets, section);
                self.phrase.clear();
            }
            let reason = ctx.sink.reason();
            self.play(ctx, midi, beats_long, g, reason);
        }
    }

    fn reset(&mut self) {
        self.current = None;
        self.notes_left_in_phrase = 0;
        self.playback = None;
        self.phrase.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::{Harmony, HarmonyChange};
    use crate::eventmap::LayerMappings;
    use crate::generators::{Cue, Params};
    use crate::motif::MotifMemory;
    use crate::notes::{NoteBook, NoteSink, ParamCache};
    use crate::transport::Transport;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use sonder_core::{hz_to_midi, ConductorConfig, LayerId, Mood, SynthCommand};
    use sonder_theory::{diatonic_chord, mode_by_name, Scale};
    use std::collections::BTreeMap;

    fn harmony() -> Harmony {
        let scale = Scale::new(2, mode_by_name("lydian").unwrap());
        let chord = diatonic_chord(&scale, 6, 0);
        Harmony {
            scale_pcs: scale.pitch_classes(),
            chord_pcs: chord.pitch_classes(&scale),
            scale,
            chord,
            change: HarmonyChange::default(),
            beats_to_change: 4.0,
        }
    }

    #[test]
    fn landing_on_a_chord_tone_stays_in_the_scale() {
        // D lydian, lead window C4–C6: neither boundary is a scale note. Park the
        // lead on the top scale note (B5) and ask it to land on a vii chord tone
        // (C# E G#): the nearest is C#6, above the window.
        let harmony = harmony();
        let mut transport = Transport::new(&ConductorConfig {
            tempo: 120.0,
            tempo_range: [120.0, 120.0],
            ..Default::default()
        });
        transport.tick(0.1, 0.5); // 0 → 0.2 beats: exactly one grid point (the downbeat)
        let (lo, hi) = register_window(Role::Lead, 0.5);
        let top = (lo..=hi)
            .rev()
            .find_map(|m| harmony.scale.index_of_midi(m))
            .unwrap();
        let params_map = BTreeMap::from([(map_keys::DENSITY.to_string(), 1.0)]);
        let mappings = LayerMappings::default();
        let signals = BTreeMap::new();
        let mut bad = Vec::new();
        for seed in 0..64u64 {
            let mut book = NoteBook::new(1);
            let mut cache = ParamCache::default();
            let mut out = Vec::new();
            let mut notes = Vec::new();
            let mut jitter = ChaCha8Rng::seed_from_u64(seed);
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            let mut motifs = MotifMemory::new();
            let mut lead = Lead {
                index: Some(top),
                notes_left_in_phrase: 5,
                land_on_chord_tone: true,
                ..Default::default()
            };
            let mut ctx = GenCtx {
                now: 0.1,
                dt: 0.1,
                transport: &transport,
                harmony: &harmony,
                mood: Mood {
                    energy: 1.0,
                    tension: 0.0,
                    brightness: 0.5,
                },
                params: Params::new(&params_map, 0.0),
                signals: &signals,
                events: &[],
                mappings: &mappings,
                cue: Cue::default(),
                motifs: &mut motifs,
                rng: &mut rng,
                sink: NoteSink::new(
                    &mut book,
                    &mut cache,
                    &mut out,
                    &mut notes,
                    &mut jitter,
                    LayerId(0),
                    0.1,
                    0.0,
                ),
            };
            lead.tick(&mut ctx);
            for c in &out {
                if let SynthCommand::NoteOn { pitch_hz, .. } = c {
                    let midi = hz_to_midi(*pitch_hz as f64).round() as i32;
                    if !harmony.scale.contains(midi) {
                        bad.push(midi);
                    }
                }
            }
        }
        assert!(bad.is_empty(), "lead left the scale: {bad:?}");
    }

    #[test]
    fn restates_a_remembered_motif_with_its_shape() {
        let harmony = harmony();
        let mut transport = Transport::new(&ConductorConfig {
            tempo: 120.0,
            tempo_range: [120.0, 120.0],
            ..Default::default()
        });
        let params_map = BTreeMap::from([(map_keys::DENSITY.to_string(), 0.5)]);
        let mappings = LayerMappings::default();
        let signals = BTreeMap::new();
        let mut motifs = MotifMemory::new();
        // Up two degrees, down one, on consecutive half-beats.
        let start_idx = harmony.scale.index_of_midi(69).unwrap();
        assert!(motifs
            .remember(
                &[start_idx, start_idx + 2, start_idx + 1],
                &[0.0, 0.5, 1.0],
                "A"
            )
            .is_some());
        let mut lead = Lead::default();
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let mut jitter = ChaCha8Rng::seed_from_u64(3);
        let mut book = NoteBook::new(1);
        let mut cache = ParamCache::default();
        let mut out = Vec::new();
        let mut notes = Vec::new();
        for i in 0..400 {
            transport.tick(0.02, 0.5);
            let mut ctx = GenCtx {
                now: i as f64 * 0.02,
                dt: 0.02,
                transport: &transport,
                harmony: &harmony,
                mood: Mood {
                    energy: 0.8,
                    tension: 0.0,
                    brightness: 0.5,
                },
                params: Params::new(&params_map, 0.0),
                signals: &signals,
                events: &[],
                mappings: &mappings,
                cue: Cue {
                    section: "A",
                    motif_recall: 1.0,
                    ..Default::default()
                },
                motifs: &mut motifs,
                rng: &mut rng,
                sink: NoteSink::new(
                    &mut book,
                    &mut cache,
                    &mut out,
                    &mut notes,
                    &mut jitter,
                    LayerId(0),
                    i as f64 * 0.02,
                    0.0,
                )
                .with_reason("lead"),
            };
            lead.tick(&mut ctx);
            book.flush(i as f64 * 0.02, &mut out, &mut notes);
        }
        let motif_notes: Vec<&sonder_core::NoteSnapshot> =
            notes.iter().filter(|n| n.reason == MOTIF_REASON).collect();
        assert!(
            motif_notes.len() >= 3,
            "no restatement in 8 seconds: {}",
            notes.len()
        );
        // The first restatement keeps the shape (plain or transposed, same section).
        let idx = |n: &sonder_core::NoteSnapshot| {
            harmony.scale.index_of_midi(n.midi.round() as i32).unwrap()
        };
        let shape: Vec<i32> = motif_notes[..3]
            .iter()
            .map(|n| idx(n) - idx(motif_notes[0]))
            .collect();
        assert_eq!(shape, vec![0, 2, 1]);
        assert!(motifs.current().starts_with("M1"), "{}", motifs.current());
        assert!(notes
            .iter()
            .all(|n| n.reason == "lead" || n.reason == MOTIF_REASON));
    }
}
