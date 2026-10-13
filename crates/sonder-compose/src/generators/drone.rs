//! Drone: tonic pedal, fifth when the texture thickens.

use sonder_core::{map_keys, NoteId, Role};
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};

#[derive(Default)]
pub struct Drone {
    tonic: Option<(NoteId, i32)>,
    fifth: Option<(NoteId, i32)>,
}

/// Hysteresis around the texture threshold that adds the fifth.
fn wants_fifth(texture: f64, has_fifth: bool) -> bool {
    if has_fifth {
        texture > 0.45
    } else {
        texture > 0.55
    }
}

impl Generator for Drone {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        let (lo, hi) = register_window(Role::Drone, ctx.params.register());
        let want_fifth = wants_fifth(ctx.params.or(map_keys::TEXTURE, 0.0), self.fifth.is_some());
        let velocity = 0.5 + 0.3 * ctx.mood.energy;
        let tonic_midi = Quantizer::in_register(&[h.scale.tonic], 0.0, lo, hi);
        let fifth_midi = Quantizer::nearest_in(&h.scale_pcs, tonic_midi as f64 + 7.0);

        let tonic_stale = self
            .tonic
            .is_none_or(|(id, m)| h.change.tonic || m != tonic_midi || !ctx.sink.is_live(id));
        if tonic_stale {
            if let Some((id, _)) = self.tonic.take() {
                ctx.sink.release(id);
            }
            self.tonic = ctx
                .sink
                .play(tonic_midi as f64, velocity, 0.5, None, 0.0)
                .map(|id| (id, tonic_midi));
        }
        match (&self.fifth, want_fifth) {
            (Some((id, _)), false) => {
                ctx.sink.release(*id);
                self.fifth = None;
            }
            (Some((id, m)), true) if *m != fifth_midi || !ctx.sink.is_live(*id) => {
                ctx.sink.release(*id);
                self.fifth = ctx
                    .sink
                    .play(fifth_midi as f64, velocity * 0.8, 0.5, None, 0.05)
                    .map(|id| (id, fifth_midi));
            }
            (None, true) => {
                self.fifth = ctx
                    .sink
                    .play(fifth_midi as f64, velocity * 0.8, 0.5, None, 0.05)
                    .map(|id| (id, fifth_midi));
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.tonic = None;
        self.fifth = None;
    }
}
