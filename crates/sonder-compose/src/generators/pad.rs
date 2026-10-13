//! Pad: sustain the chord, voice-lead across changes.

use rand::RngExt;
use sonder_core::{NoteId, Role};
use sonder_theory::{voice_lead, Voicing};

use super::{register_window, GenCtx, Generator};

#[derive(Default)]
pub struct Pad {
    /// One slot per voice: the sounding note and its midi.
    voices: Vec<Option<(NoteId, i32)>>,
}

impl Generator for Pad {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        if h.chord_pcs.is_empty() {
            return;
        }
        // Voices only re-form on harmonic changes, so a density hovering
        // around a threshold never makes the pad stutter.
        let alive = self.voices.iter().any(|v| v.is_some());
        if !(h.change.any() || !alive) {
            return;
        }
        let density = ctx.params.density(0.5);
        let mut n = 3 + h.chord.extensions() as usize;
        if density > 0.6 {
            n += 1;
        }
        if density < 0.2 {
            n = n.saturating_sub(1);
        }
        let n = n.clamp(2, 6);
        let (lo, hi) = register_window(Role::Pad, ctx.params.register());

        let from = Voicing(
            self.voices
                .iter()
                .map(|v| v.map(|(_, m)| m).unwrap_or(-1))
                .filter(|&m| m >= 0)
                .collect(),
        );
        let from = if from.len() == self.voices.len() {
            from
        } else {
            Voicing::default()
        };
        let target = voice_lead(&from, &h.chord_pcs, lo, hi, n);
        let velocity = 0.3 + 0.4 * ctx.mood.energy;
        let humanize = ctx.transport.humanize();

        let mut next: Vec<Option<(NoteId, i32)>> = Vec::with_capacity(n);
        for (i, &midi) in target.0.iter().enumerate() {
            let old = self.voices.get(i).copied().flatten();
            match old {
                Some((id, m)) if m == midi && ctx.sink.is_live(id) => next.push(Some((id, midi))),
                Some((id, m))
                    if m.rem_euclid(12) == midi.rem_euclid(12) && ctx.sink.is_live(id) =>
                {
                    ctx.sink.glide(id, midi as f64);
                    next.push(Some((id, midi)));
                }
                old => {
                    if let Some((id, _)) = old {
                        ctx.sink.release(id);
                    }
                    let stagger =
                        i as f64 * 0.04 * humanize + ctx.rng.random::<f64>() * 0.02 * humanize;
                    let pan = 0.5 + (i as f64 / n.max(2) as f64 - 0.5) * 0.3;
                    next.push(
                        ctx.sink
                            .play(midi as f64, velocity, pan, None, stagger)
                            .map(|id| (id, midi)),
                    );
                }
            }
        }
        for extra in self.voices.iter().skip(n).flatten() {
            ctx.sink.release(extra.0);
        }
        self.voices = next;
    }

    fn reset(&mut self) {
        self.voices.clear();
    }
}
