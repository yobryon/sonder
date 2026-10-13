//! Standalone demo of the web UI with a fake engine: simulated headlights,
//! notes and events, and a sine/chord tone on `/audio`. Useful for frontend work
//! without the real engine.
//!
//! ```sh
//! cargo run -p sonder-web --example demo -- 127.0.0.1:8787
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use sonder_core::*;
use sonder_web::{serve, AudioSource, WebHandle};
use tokio::sync::{mpsc, watch};

struct Chord {
    phase: [f32; 3],
    rate: f32,
}

impl AudioSource for Chord {
    fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize {
        let frames = (self.rate / 100.0) as usize;
        let freqs = [146.83f32, 220.0, 277.18];
        for _ in 0..frames {
            let mut s = 0.0;
            for (p, f) in self.phase.iter_mut().zip(freqs) {
                s += (*p * std::f32::consts::TAU).sin() * 0.12;
                *p = (*p + f / self.rate).fract();
            }
            out.push(s);
            out.push(s * 0.8);
        }
        frames * 2
    }
}

struct Sim {
    t: f64,
    cars: Vec<(f64, f64, usize, f64, f64)>, // x, y, lane, size, bright
    events: Vec<EventSnapshot>,
    notes: Vec<NoteSnapshot>,
    seed: u64,
}

impl Sim {
    fn rnd(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed % 10_000) as f64 / 10_000.0
    }

    fn step(&mut self, dt: f64) -> EngineSnapshot {
        self.t += dt;
        let lanes = 4usize;
        if self.rnd() < 0.08 {
            let lane = (self.rnd() * lanes as f64) as usize;
            let dir = if lane < 2 { 0.0 } else { 1.0 };
            self.cars.push((dir, (lane as f64 + 0.5) / lanes as f64 * 0.8 + 0.1, lane, 0.3 + self.rnd() * 0.7, 0.5 + self.rnd() * 0.5));
        }
        let mut exited = Vec::new();
        for c in &mut self.cars {
            let v = (0.12 + c.3 * 0.1) * dt;
            if c.2 < 2 { c.0 += v } else { c.0 -= v }
        }
        self.cars.retain(|c| {
            let keep = (0.0..=1.0).contains(&c.0);
            if !keep {
                exited.push(*c);
            }
            keep
        });
        for c in exited {
            let mut attrs = BTreeMap::new();
            attrs.insert("lane".into(), Value::Num(c.2 as f64));
            attrs.insert("x".into(), Value::Num(c.0.clamp(0.0, 1.0)));
            attrs.insert("speed".into(), Value::Num(40.0 + c.3 * 50.0));
            self.events.push(EventSnapshot { at: self.t, layer: LayerId(1), name: "car".into(), intensity: c.3, attrs });
            let midi = 50.0 + [0.0, 4.0, 7.0, 11.0, 14.0][(c.2 + (self.t as usize % 2)) % 5] as f32 + 12.0 * (c.2 % 2) as f32;
            self.notes.push(NoteSnapshot { at: self.t, layer: LayerId(1), midi, velocity: 0.4 + c.3 as f32 * 0.6, pan: c.0 as f32, duration: Some(0.6), active: true });
        }
        if (self.t * 4.0).floor() != ((self.t - dt) * 4.0).floor() && self.rnd() < 0.5 {
            let midi = 62.0 + [0.0, 2.0, 4.0, 6.0, 7.0, 9.0][(self.rnd() * 6.0) as usize] as f32;
            self.notes.push(NoteSnapshot { at: self.t, layer: LayerId(2), midi, velocity: 0.5, pan: 0.5, duration: Some(0.25), active: true });
        }
        if (self.t * 0.25).floor() != ((self.t - dt) * 0.25).floor() {
            for m in [50.0f32, 57.0, 62.0, 66.0] {
                self.notes.push(NoteSnapshot { at: self.t, layer: LayerId(0), midi: m, velocity: 0.35, pan: 0.5, duration: Some(4.0), active: true });
            }
        }
        for n in &mut self.notes {
            n.active = self.t - n.at < n.duration.unwrap_or(1.0);
        }
        self.notes.retain(|n| self.t - n.at < 15.0);
        if self.events.len() > 64 {
            let cut = self.events.len() - 64;
            self.events.drain(..cut);
        }

        let density = 0.5 + 0.4 * (self.t * 0.2).sin();
        let speed = 0.5 + 0.3 * (self.t * 0.13 + 1.0).cos();
        let mut layers = Vec::new();
        for (i, (name, role, inst)) in [("traffic", Role::Pad, "pad.glass"), ("headlights", Role::Pluck, "pluck.nylon"), ("shimmer", Role::Arp, "pluck.glass")].into_iter().enumerate() {
            let mut signals = BTreeMap::new();
            signals.insert("density".into(), density);
            if i == 0 {
                signals.insert("speed".into(), speed);
            }
            let mut params = BTreeMap::new();
            params.insert("density".into(), density);
            params.insert("register".into(), 0.3 + 0.4 * speed);
            params.insert("brightness".into(), speed);
            layers.push(LayerSnapshot {
                id: LayerId(i as u16),
                name: name.into(),
                role,
                instrument: inst.into(),
                muted: false,
                soloed: false,
                signals,
                params,
                meter: Meter { rms: (0.1 + 0.1 * (self.t * (i as f64 + 1.0)).sin().abs()) as f32, peak: 0.4 },
                active_notes: self.notes.iter().filter(|n| n.active && n.layer.0 == i as u16).count() as u16,
                source: Some("traffic".into()),
            });
        }
        let mut readings = BTreeMap::new();
        readings.insert("count".into(), self.cars.len() as f64);
        readings.insert("mean_speed".into(), 40.0 + speed * 50.0);
        readings.insert("flow".into(), density);
        let mut viz = BTreeMap::new();
        viz.insert(
            "traffic".to_string(),
            serde_json::json!({ "cars": self.cars.iter().map(|c| serde_json::json!({"x": c.0, "y": c.1, "lane": c.2, "size": c.3, "bright": c.4})).collect::<Vec<_>>() }),
        );
        let spectrum: Vec<f32> = (0..48).map(|i| (0.5 + 0.5 * ((self.t * 2.0 + i as f64 * 0.3).sin())) as f32 * (1.0 - i as f32 / 60.0) * density as f32).collect();
        let bpm = 62.0;
        let beats = self.t * bpm / 60.0;
        EngineSnapshot {
            scene_name: "Night Freeway".into(),
            viz_kind: "freeway".into(),
            time: self.t,
            paused: false,
            transport: TransportPos { bpm, beats_per_bar: 4, bar: (beats / 4.0) as u64, beat: (beats % 4.0) as u8, phase: beats.fract(), beats },
            harmony: HarmonySnapshot {
                tonic: 2,
                key_name: "D".into(),
                mode: "lydian".into(),
                scale: vec![2, 4, 6, 8, 9, 11, 1],
                chord: vec![2, 6, 9, 1],
                chord_name: "Dmaj7".into(),
                chord_degree: 1,
                beats_to_change: 8.0 - beats % 8.0,
            },
            mood: Mood { energy: density, tension: 0.3, brightness: speed },
            conductor_note: format!("energy {density:.2} (driven by traffic.density) · brightness {speed:.2} → staying Lydian"),
            layers,
            sources: vec![SourceSnapshot {
                name: "traffic".into(),
                kind: "synthetic.freeway".into(),
                state: SourceState::Live,
                message: "simulated, 4 lanes".into(),
                rate: 10.0,
                last_sample_at: Some(self.t),
                readings,
            }],
            events: self.events.clone(),
            notes: self.notes.clone(),
            master: Meter { rms: 0.2, peak: 0.5 },
            spectrum,
            viz,
            underruns: 0,
            audio_backend: "demo".into(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:8787".into()).parse()?;
    let mut sim = Sim { t: 0.0, cars: Vec::new(), events: Vec::new(), notes: Vec::new(), seed: 0x9E3779B97F4A7C15 };
    let (snap_tx, snap_rx) = watch::channel(Arc::new(sim.step(0.0)));
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UserCommand>(32);
    tokio::spawn(async move {
        while let Some(c) = cmd_rx.recv().await {
            eprintln!("command: {c:?}");
        }
    });
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(50));
        loop {
            iv.tick().await;
            if snap_tx.send(Arc::new(sim.step(0.05))).is_err() {
                break;
            }
        }
    });
    eprintln!("demo ui at http://{addr}");
    serve(addr, WebHandle { snapshot: snap_rx, commands: cmd_tx, audio: Some(Box::new(Chord { phase: [0.0; 3], rate: 48000.0 })), sample_rate: 48000 }).await
}
