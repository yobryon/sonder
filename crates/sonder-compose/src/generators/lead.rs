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
                midi = Quantizer::nearest_in(&h.chord_pcs, midi as f64).clamp(lo, hi);
                idx = h.scale.index_of_midi(midi).unwrap_or(idx);
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
