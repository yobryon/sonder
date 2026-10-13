//! Arp: cycle the chord tones in a pattern; density sets the subdivision.

use rand::RngExt;
use sonder_core::Role;
use sonder_theory::Quantizer;

use super::{name_hash, register_window, GenCtx, Generator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pattern {
    Up,
    Down,
    UpDown,
    Random,
}

pub struct Arp {
    pattern: Pattern,
    pos: usize,
    descending: bool,
}

impl Arp {
    pub fn new(layer_name: &str) -> Self {
        let pattern = match name_hash(layer_name) % 4 {
            0 => Pattern::Up,
            1 => Pattern::Down,
            2 => Pattern::UpDown,
            _ => Pattern::Random,
        };
        Self { pattern, pos: 0, descending: false }
    }

    fn next_index(&mut self, len: usize, ctx: &mut GenCtx<'_>) -> usize {
        if len == 0 {
            return 0;
        }
        match self.pattern {
            Pattern::Up => {
                let i = self.pos % len;
                self.pos = (self.pos + 1) % len;
                i
            }
            Pattern::Down => {
                let i = len - 1 - (self.pos % len);
                self.pos = (self.pos + 1) % len;
                i
            }
            Pattern::UpDown => {
                let i = self.pos.min(len - 1);
                if len == 1 {
                    0
                } else {
                    if self.descending {
                        if self.pos == 0 {
                            self.descending = false;
                            self.pos = 1;
                        } else {
                            self.pos -= 1;
                        }
                    } else if self.pos + 1 >= len {
                        self.descending = true;
                        self.pos = len - 2;
                    } else {
                        self.pos += 1;
                    }
                    i
                }
            }
            Pattern::Random => ctx.rng.random_range(0..len),
        }
    }
}

impl Generator for Arp {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let h = ctx.harmony;
        if h.chord_pcs.is_empty() {
            return;
        }
        let density = ctx.params.density(0.5);
        let (lo, hi) = register_window(Role::Arp, ctx.params.register());
        let notes = Quantizer::notes_in_window(&h.chord_pcs, lo, hi);
        if notes.is_empty() {
            return;
        }
        let sub = if density < 0.33 {
            1.0
        } else if density < 0.66 {
            0.5
        } else {
            0.25
        };
        let energy = ctx.mood.energy;
        for g in ctx.transport.grid_points(sub) {
            let i = self.next_index(notes.len(), ctx);
            let midi = notes[i];
            let accent = if ctx.transport.in_bar(g).fract() == 0.0 { 0.1 } else { 0.0 };
            let vel = 0.4 + 0.35 * energy + accent;
            let pan = 0.35 + 0.3 * (i as f64 / notes.len().max(2) as f64);
            let dur = sub * 0.8 * ctx.transport.seconds_per_beat();
            ctx.sink.play(midi as f64, vel, pan, Some(dur), ctx.transport.delay_to(g));
        }
    }
}
