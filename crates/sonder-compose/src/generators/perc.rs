//! Perc: event-driven hits and/or a signal-driven Euclidean groove.

use rand::RngExt;
use sonder_core::Drum;
use sonder_theory::euclidean;

use super::{GenCtx, Generator};
use crate::transport::Transport;

pub struct Perc {
    /// No `on_event` tables: run the pattern engine.
    pattern_mode: bool,
}

impl Perc {
    pub fn new(pattern_mode: bool) -> Self {
        Self { pattern_mode }
    }

    fn drum_for(name: &str, intensity: f64) -> Drum {
        let n = name.to_ascii_lowercase();
        if n.contains("kick") || n.contains("quake") || n.contains("bass") || n.contains("boom") {
            Drum::Kick
        } else if n.contains("snare") || n.contains("clap") {
            Drum::Snare
        } else if n.contains("hat") || n.contains("tick") {
            Drum::Hat
        } else if n.contains("click") {
            Drum::Click
        } else if n.contains("tom") {
            Drum::Tom
        } else if n.contains("rim") {
            Drum::Rim
        } else if n.contains("noise") || n.contains("wind") {
            Drum::Noise
        } else if intensity > 0.7 {
            Drum::Kick
        } else {
            Drum::Hat
        }
    }
}

impl Generator for Perc {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        let beats = ctx.transport.beats();
        let energy = ctx.mood.energy;
        for ev in ctx.events {
            let t = ctx.mappings.trigger(ev, ctx.signals, ctx.now, beats);
            if t.chance < 1.0 && ctx.rng.random::<f64>() >= t.chance {
                continue;
            }
            let drum = t
                .drum
                .unwrap_or_else(|| Self::drum_for(&ev.name, ev.intensity));
            let delay = match t.quantize {
                Some(grid) => ctx.transport.delay_to(Transport::next_grid(beats, grid)),
                None => 0.0,
            };
            ctx.sink.drum(drum, t.velocity, t.pan, t.pitch, delay);
        }

        if !self.pattern_mode {
            return;
        }
        let density = ctx.params.density(0.4);
        let bpb = ctx.transport.beats_per_bar() as f64;
        let steps = (bpb * 4.0).round().max(1.0) as usize;
        let pulses = (2.0 + (density * 3.0).round()) as usize;
        let kicks = euclidean(steps, pulses.min(steps), 0);
        let tone = 0.3 + 0.5 * ctx.mood.brightness;
        for g in ctx.transport.grid_points(0.25) {
            let p = ctx.transport.in_bar(g);
            let step = ((p * 4.0).round() as usize) % steps;
            let delay = ctx.transport.delay_to(g);
            if kicks[step] {
                ctx.sink
                    .drum(Drum::Kick, 0.6 + 0.3 * energy, 0.5, tone, delay);
            }
            let in_beat = p - p.floor();
            if (in_beat - 0.5).abs() < 1e-6 {
                ctx.sink
                    .drum(Drum::Hat, 0.25 + 0.35 * energy, 0.55, tone, delay);
            } else if density > 0.6
                && (in_beat - 0.25).abs() < 1e-6
                && ctx.rng.random::<f64>() < (density - 0.6) * 1.5
            {
                ctx.sink
                    .drum(Drum::Hat, 0.15 + 0.25 * energy, 0.45, tone, delay);
            }
            if density > 0.5 && (p - (bpb / 2.0).floor()).abs() < 1e-6 && bpb >= 2.0 {
                ctx.sink
                    .drum(Drum::Snare, 0.4 + 0.3 * energy, 0.5, tone, delay);
            }
        }
    }
}
