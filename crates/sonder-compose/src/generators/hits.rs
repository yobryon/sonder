//! Pluck / bells: one note per event, pitch quantized to the harmony.

use rand::RngExt;
use sonder_core::Role;
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};
use crate::eventmap::{default_length_beats, Snap};
use crate::transport::Transport;

pub struct Hits {
    role: Role,
}

impl Hits {
    pub fn new(role: Role) -> Self {
        Self { role }
    }
}

impl Generator for Hits {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        let (lo, hi) = register_window(self.role, ctx.params.register());
        let beats = ctx.transport.beats();
        let spb = ctx.transport.seconds_per_beat();
        let chance_scale = ctx.params.density(1.0);
        for ev in ctx.events {
            let t = ctx.mappings.trigger(ev, ctx.signals, ctx.now, beats);
            let p = t.chance * if chance_scale < 1.0 { 0.5 + 0.5 * chance_scale } else { 1.0 };
            if p < 1.0 && ctx.rng.random::<f64>() >= p {
                continue;
            }
            let midi = match t.snap {
                Snap::Chord if !h.chord_pcs.is_empty() => Quantizer::in_register(&h.chord_pcs, t.pitch, lo, hi) as f64,
                Snap::Chord | Snap::Scale => Quantizer::in_register(&h.scale_pcs, t.pitch, lo, hi) as f64,
                Snap::None => lo as f64 + (hi - lo) as f64 * t.pitch,
            };
            let delay = match t.quantize {
                Some(grid) => ctx.transport.delay_to(Transport::next_grid(beats, grid)),
                None => 0.0,
            };
            let length = t.length_beats.unwrap_or_else(|| default_length_beats(self.role)) * spb;
            ctx.sink.play(midi, t.velocity, t.pan, Some(length), delay);
        }
    }
}
