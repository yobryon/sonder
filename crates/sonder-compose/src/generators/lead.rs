//! Lead: a random walk through the scale, in phrases.

use rand::RngExt;
use sonder_core::{map_keys, NoteId, Role};
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};

pub struct Lead {
    /// Absolute scale-degree index of the last note (see `Scale::index_of_midi`).
    index: Option<i32>,
    notes_left_in_phrase: u32,
    rest_until_beat: f64,
    current: Option<NoteId>,
    /// Land on a chord tone for the first note after a chord change.
    land_on_chord_tone: bool,
}

impl Default for Lead {
    fn default() -> Self {
        Self {
            index: None,
            notes_left_in_phrase: 0,
            rest_until_beat: 0.0,
            current: None,
            land_on_chord_tone: false,
        }
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

        for g in ctx.transport.grid_points(sub) {
            if g < self.rest_until_beat {
                continue;
            }
            if self.notes_left_in_phrase == 0 {
                // Start a phrase (or decide to keep resting).
                let p_start = 0.15 + 0.6 * density;
                if ctx.rng.random::<f64>() > p_start {
                    self.rest_until_beat = g + 1.0 + ctx.rng.random::<f64>() * 2.0;
                    continue;
                }
                self.notes_left_in_phrase = 2 + ctx.rng.random_range(0..5);
            }
            // Probability of a note on this subdivision.
            let p = (0.3 + 0.7 * density) * (0.5 + 0.5 * energy);
            if ctx.rng.random::<f64>() > p {
                continue;
            }

            let lo_i = (lo..=hi)
                .find_map(|m| h.scale.index_of_midi(m))
                .unwrap_or(0);
            let hi_i = (lo..=hi)
                .rev()
                .find_map(|m| h.scale.index_of_midi(m))
                .unwrap_or(lo_i);
            let centre = (lo_i + hi_i) / 2;
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
            }
            if let Some(prev) = self.current.take() {
                ctx.sink.release(prev);
            }
            let vel = 0.4
                + 0.4 * energy
                + if ctx.transport.in_bar(g).fract() == 0.0 {
                    0.08
                } else {
                    0.0
                };
            let dur = beats_long * ctx.transport.seconds_per_beat();
            self.current =
                ctx.sink
                    .play(midi as f64, vel, 0.5, Some(dur), ctx.transport.delay_to(g));
        }
    }

    fn reset(&mut self) {
        self.current = None;
        self.notes_left_in_phrase = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::{Harmony, HarmonyChange};
    use crate::eventmap::LayerMappings;
    use crate::generators::Params;
    use crate::notes::{NoteBook, NoteSink, ParamCache};
    use crate::transport::Transport;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use sonder_core::{hz_to_midi, ConductorConfig, LayerId, Mood, SynthCommand};
    use sonder_theory::{diatonic_chord, mode_by_name, Scale};
    use std::collections::BTreeMap;

    #[test]
    fn landing_on_a_chord_tone_stays_in_the_scale() {
        // D lydian, lead window C4–C6: neither boundary is a scale note. Park the
        // lead on the top scale note (B5) and ask it to land on a vii chord tone
        // (C# E G#): the nearest is C#6, above the window.
        let scale = Scale::new(2, mode_by_name("lydian").unwrap());
        let chord = diatonic_chord(&scale, 6, 0);
        let harmony = Harmony {
            scale_pcs: scale.pitch_classes(),
            chord_pcs: chord.pitch_classes(&scale),
            scale,
            chord,
            change: HarmonyChange::default(),
            beats_to_change: 4.0,
        };
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
}
