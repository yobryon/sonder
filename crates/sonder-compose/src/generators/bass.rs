//! Bass: root and fifth on the strong beats; density adds motion.

use rand::RngExt;
use sonder_core::{NoteId, Role};
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};

#[derive(Default)]
pub struct Bass {
    current: Option<NoteId>,
    /// Alternate fifth / octave on the middle beat.
    bar_parity: bool,
}

impl Bass {
    fn hit(&mut self, ctx: &mut GenCtx<'_>, midi: i32, velocity: f64, beats_long: f64, at_beat: f64) {
        if let Some(prev) = self.current.take() {
            ctx.sink.release(prev);
        }
        let delay = ctx.transport.delay_to(at_beat);
        let dur = beats_long * ctx.transport.seconds_per_beat();
        self.current = ctx.sink.play(midi as f64, velocity, 0.5, Some(dur), delay);
    }
}

impl Generator for Bass {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        if h.chord_pcs.is_empty() {
            return;
        }
        let density = ctx.params.density(0.4);
        let energy = ctx.mood.energy;
        let (lo, hi) = register_window(Role::Bass, ctx.params.register());
        let root_pc = h.root_pc();
        let root = Quantizer::in_register(&[root_pc], 0.45, lo, hi);
        let fifth = Quantizer::nearest_in(&h.scale_pcs, root as f64 + 7.0);
        let octave = if root + 12 <= hi { root + 12 } else { root };
        let bpb = ctx.transport.beats_per_bar() as f64;
        let mid = (bpb / 2.0).floor().max(1.0);
        let base_vel = 0.55 + 0.3 * energy;

        for g in ctx.transport.grid_points(0.5) {
            let p = ctx.transport.in_bar(g);
            let on_beat = (p - p.round()).abs() < 1e-6;
            let p_r = p.round();
            if on_beat && p_r == 0.0 {
                self.bar_parity = !self.bar_parity;
                self.hit(ctx, root, base_vel, (mid - 0.1).max(0.4), g);
            } else if on_beat && p_r == mid && bpb >= 2.0 {
                // Rest sometimes when the music is quiet.
                if energy < 0.3 && ctx.rng.random::<f64>() < 0.5 {
                    continue;
                }
                let note = if self.bar_parity { fifth } else { octave };
                self.hit(ctx, note, base_vel * 0.85, (bpb - mid - 0.6).max(0.4), g);
            } else if !on_beat && density > 0.4 {
                let eighth = p - p.floor(); // 0.5
                let _ = eighth;
                // Pickup into the downbeat: approach the root from the scale note below.
                if (p - (bpb - 0.5)).abs() < 1e-6 {
                    let below = Quantizer::nearest_in(&h.scale_pcs, root as f64 - 1.5);
                    let approach = if below == root { Quantizer::nearest_in(&h.scale_pcs, root as f64 - 3.0) } else { below };
                    self.hit(ctx, approach, base_vel * 0.7, 0.45, g);
                } else if density > 0.75 && ctx.rng.random::<f64>() < 0.6 {
                    // Syncopation: offbeat octave / passing tones.
                    let third = Quantizer::nearest_in(&h.scale_pcs, root as f64 + 3.5);
                    let pick = if ctx.rng.random::<f64>() < 0.5 { octave } else { third };
                    self.hit(ctx, pick, base_vel * 0.65, 0.4, g);
                } else if (p - (mid - 0.5)).abs() < 1e-6 && ctx.rng.random::<f64>() < density * 0.5 {
                    // Passing tone walking root → fifth.
                    let third = Quantizer::nearest_in(&h.scale_pcs, root as f64 + 3.5);
                    self.hit(ctx, third, base_vel * 0.6, 0.45, g);
                }
            }
        }
    }

    fn reset(&mut self) {
        self.current = None;
    }
}
