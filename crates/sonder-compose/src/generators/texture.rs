//! Texture: a low bed that the instrument turns into noise; shaped by signals.

use sonder_core::{map_keys, LayerParam, NoteId, Role};
use sonder_theory::Quantizer;

use super::{register_window, GenCtx, Generator};

#[derive(Default)]
pub struct Texture {
    notes: Vec<(NoteId, i32)>,
    tonic: Option<u8>,
}

impl Generator for Texture {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        let (lo, hi) = register_window(Role::Texture, ctx.params.register());
        let texture = ctx.params.get(map_keys::TEXTURE).unwrap_or(0.3 + 0.4 * ctx.mood.energy).clamp(0.0, 1.0);
        let want = if texture > 0.5 { 2 } else { 1 };
        let alive = !self.notes.is_empty() && self.notes.iter().all(|(id, _)| ctx.sink.is_live(*id));
        if self.tonic != Some(h.scale.tonic) || !alive || self.notes.len() != want {
            for (id, _) in self.notes.drain(..) {
                ctx.sink.release(id);
            }
            let root = Quantizer::in_register(&[h.scale.tonic], 0.0, lo, hi);
            let vel = 0.3 + 0.3 * ctx.mood.energy;
            if let Some(id) = ctx.sink.play(root as f64, vel, 0.5, None, 0.0) {
                self.notes.push((id, root));
            }
            if want == 2 {
                let fifth = Quantizer::nearest_in(&h.scale_pcs, root as f64 + 7.0);
                if let Some(id) = ctx.sink.play(fifth as f64, vel * 0.8, 0.5, None, 0.1) {
                    self.notes.push((id, fifth));
                }
            }
            self.tonic = Some(h.scale.tonic);
        }

        ctx.sink.param(LayerParam::Texture(texture as f32));
        if ctx.params.get(map_keys::CUTOFF).is_none() {
            let cutoff = (ctx.mood.brightness - 0.5) * 3.0;
            ctx.sink.param(LayerParam::CutoffOctaves(cutoff as f32));
        }
        if ctx.params.get(map_keys::PAN).is_none() {
            // A slow, deterministic drift across the field.
            let pan = 0.5 + 0.3 * (ctx.now * 0.07).sin();
            ctx.sink.param(LayerParam::Pan(pan as f32));
        }
    }

    fn reset(&mut self) {
        self.notes.clear();
        self.tonic = None;
    }
}
