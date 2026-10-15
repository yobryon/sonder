//! Role generators: one musical personality per [`Role`].

mod arp;
mod bass;
mod drone;
mod hits;
mod lead;
mod pad;
mod perc;
mod texture;

use std::collections::BTreeMap;

use rand_chacha::ChaCha8Rng;
use sonder_core::{map_keys, LayerConfig, LayerEvent, Mood, Role, Seconds};

use crate::conductor::Harmony;
use crate::eventmap::LayerMappings;
use crate::form::{blend, FormTargets};
use crate::motif::MotifMemory;
use crate::notes::NoteSink;
use crate::transport::Transport;

pub use hits::TRIGGER_REASON;
pub use lead::MOTIF_REASON;

/// What the form engine tells every generator this tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cue<'a> {
    /// Current section name ("intro", "A", …), for motif keying.
    pub section: &'a str,
    /// 0..1 how much to thin out (release sections, scaled by form strength).
    pub thin: f64,
    /// Melodic generators stay quiet until this beat (an answer is playing).
    pub rest_until_beat: f64,
    /// 0..1 probability of restating a remembered motif at phrase start.
    pub motif_recall: f64,
}

/// Everything a generator may read or do during one tick.
pub struct GenCtx<'a> {
    pub now: Seconds,
    pub dt: f64,
    pub transport: &'a Transport,
    pub harmony: &'a Harmony,
    pub mood: Mood,
    pub params: Params<'a>,
    pub signals: &'a BTreeMap<String, f64>,
    pub events: &'a [&'a LayerEvent],
    pub mappings: &'a LayerMappings,
    pub cue: Cue<'a>,
    pub motifs: &'a mut MotifMemory,
    pub rng: &'a mut ChaCha8Rng,
    pub sink: NoteSink<'a>,
}

/// A layer's resolved `map` values plus the global density offset and the
/// form's targets (blended in by its strength).
#[derive(Clone, Copy)]
pub struct Params<'a> {
    map: &'a BTreeMap<String, f64>,
    density_offset: f64,
    form: Option<(FormTargets, f64)>,
}

impl<'a> Params<'a> {
    pub fn new(map: &'a BTreeMap<String, f64>, density_offset: f64) -> Self {
        Self {
            map,
            density_offset,
            form: None,
        }
    }
    /// Blend density / register toward the form's targets by `strength`.
    pub fn with_form(mut self, targets: Option<FormTargets>, strength: f64) -> Self {
        self.form = targets.map(|t| (t, strength));
        self
    }
    /// A raw param if the scene mapped it (and it is finite).
    pub fn get(&self, key: &str) -> Option<f64> {
        self.map.get(key).copied().filter(|v| v.is_finite())
    }
    /// A 0..1 param with a default.
    pub fn or(&self, key: &str, default: f64) -> f64 {
        self.get(key).unwrap_or(default).clamp(0.0, 1.0)
    }
    /// Density with the user's global offset applied and the form blended in.
    pub fn density(&self, default: f64) -> f64 {
        let data =
            (self.get(map_keys::DENSITY).unwrap_or(default) + self.density_offset).clamp(0.0, 1.0);
        match self.form {
            Some((t, s)) => blend(data, t.density, s),
            None => data,
        }
    }
    /// Register 0..1 (0.5 = the role's home window), form blended in.
    pub fn register(&self) -> f64 {
        let data = self.or(map_keys::REGISTER, 0.5);
        match self.form {
            Some((t, s)) => blend(data, t.register, s),
            None => data,
        }
    }
}

/// A role's home midi window, shifted by the `register` param (±1 octave).
pub fn register_window(role: Role, register: f64) -> (i32, i32) {
    let (lo, hi) = match role {
        Role::Pad => (48, 72),   // C3–C5
        Role::Drone => (26, 43), // D1–G2
        Role::Bass => (24, 48),  // C1–C3
        Role::Lead => (60, 84),  // C4–C6
        Role::Arp => (48, 72),   // C3–C5
        Role::Pluck => (60, 84), // C4–C6
        Role::Bells => (72, 96), // C5–C7
        Role::Perc => (36, 60),
        Role::Texture => (36, 48), // C2–C3
    };
    let shift = ((register.clamp(0.0, 1.0) - 0.5) * 24.0).round() as i32;
    ((lo + shift).clamp(12, 108), (hi + shift).clamp(12, 108))
}

/// A role generator. Called once per control tick while its layer is audible.
pub trait Generator: Send {
    fn tick(&mut self, ctx: &mut GenCtx<'_>);
    /// All of this layer's notes were just cut (mute / pause): forget them.
    fn reset(&mut self) {}
}

/// Build the generator for a layer's role.
pub fn for_role(cfg: &LayerConfig) -> Box<dyn Generator> {
    match cfg.role {
        Role::Pad => Box::new(pad::Pad::default()),
        Role::Drone => Box::new(drone::Drone::default()),
        Role::Bass => Box::new(bass::Bass::default()),
        Role::Lead => Box::new(lead::Lead::default()),
        Role::Arp => Box::new(arp::Arp::new(&cfg.name)),
        Role::Pluck | Role::Bells => Box::new(hits::Hits::new(cfg.role)),
        Role::Perc => Box::new(perc::Perc::new(cfg.on_event.is_empty())),
        Role::Texture => Box::new(texture::Texture::default()),
    }
}

/// Stable 0..1 hash of a string (FNV-1a), for name-seeded choices.
pub fn name_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
