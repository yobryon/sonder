//! The composer: conductor, transport and role generators turn layer
//! parameters and events into [`SynthCommand`]s.
//!
//! ```text
//! params/events ─▶ Composer::tick ─▶ SynthCommands + NoteSnapshots
//!                      │
//!                      ├─ Transport   (tempo, meter, swing, humanize)
//!                      ├─ Conductor   (mood → key / mode / chord, and why)
//!                      ├─ Generators  (one per layer, by Role)
//!                      └─ NoteBook    (note ids, planned releases, deferred triggers)
//! ```
//!
//! Everything is deterministic for a given scene, seed and input sequence.

#![forbid(unsafe_code)]

mod answer;
mod conductor;
mod eventmap;
mod form;
mod generators;
mod motif;
mod notes;
mod transport;

use std::collections::BTreeMap;

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use sonder_core::{
    map_keys, EngineSnapshot, FormSnapshot, LayerConfig, LayerEvent, LayerId, LayerParam,
    LayerSnapshot, NoteSnapshot, Role, Scene, Seconds, SynthCommand, TransportPos, UserCommand,
};

pub use answer::{Answers, Echo, Pending, ANSWER_REASON, NOVELTY_S, SALIENT_INTENSITY};
pub use conductor::{Harmony, HarmonyChange};
pub use eventmap::{Snap, Trigger};
pub use form::{blend, Form, FormTargets, Section};
pub use generators::human::{Call, Human, HumanTracker, HUMAN_REASON, MAX_HELD, SAFETY_BEATS};
pub use generators::{
    register_window, Cue, GenCtx, Generator, Params, MOTIF_REASON, TRIGGER_REASON,
};
pub use motif::{Motif, MotifMemory, Restatement, Transform, MAX_MOTIFS};
pub use notes::{cause_of, NoteSink, CAUSE_ATTRS, TRIGGERS_PER_TICK};
pub use transport::Transport;

use conductor::{Conductor, MoodSource};
use eventmap::LayerMappings;
use notes::{NoteBook, ParamCache};

/// `NoteSnapshot.reason` for notes a role plays on its own clock.
fn reason_for(role: Role) -> &'static str {
    match role {
        Role::Perc => "perc-pattern",
        Role::Pluck | Role::Bells => generators::TRIGGER_REASON,
        other => other.name(),
    }
}

/// Why a scene could not be turned into a composer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComposeError {
    #[error("conductor.key `{0}` is not a pitch class")]
    UnknownKey(String),
    #[error("conductor.mode `{0}` is not a known mode")]
    UnknownMode(String),
    #[error("layer `{layer}`: on_event.{event}.{key}: {message}")]
    EventExpr {
        layer: String,
        event: String,
        key: String,
        message: String,
    },
    #[error("layer `{layer}`: on_event.{event}.drum `{drum}` is not a drum (kick|snare|hat|click|noise|tom|rim)")]
    UnknownDrum {
        layer: String,
        event: String,
        drum: String,
    },
    #[error("layer `{layer}`: on_event.{event}.snap `{snap}` must be chord|scale|none")]
    UnknownSnap {
        layer: String,
        event: String,
        snap: String,
    },
}

/// One layer's resolved inputs for a tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerInput {
    /// Resolved `map` values (keys from [`map_keys`]).
    pub params: BTreeMap<String, f64>,
    /// Current signal values, readable from `on_event` expressions.
    pub signals: BTreeMap<String, f64>,
}

/// Everything the composer needs for one control tick.
#[derive(Debug, Clone, Copy)]
pub struct ComposerInput<'a> {
    /// Indexed by `LayerId.0`. Missing layers read as empty.
    pub layers: &'a [LayerInput],
    /// Events that arrived this tick (from sources and from expr `Fired`).
    pub events: &'a [LayerEvent],
}

/// What a tick produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposerOutput {
    pub commands: Vec<SynthCommand>,
    /// Notes that started this tick.
    pub notes: Vec<NoteSnapshot>,
}

struct Layer {
    id: LayerId,
    cfg: LayerConfig,
    generator: Box<dyn Generator>,
    mappings: LayerMappings,
    rng: ChaCha8Rng,
    jitter: ChaCha8Rng,
    muted: bool,
    soloed: bool,
    /// Whether the synth currently believes this layer is muted.
    muted_sent: bool,
    params: ParamCache,
    /// Last effective params, for the snapshot.
    last_params: BTreeMap<String, f64>,
    /// `Role::Human` only: activity and phrase bookkeeping.
    human: Option<HumanTracker>,
    /// `Role::Human` only: params with `energy` defaulted to the activity, for the mood.
    mood_params: BTreeMap<String, f64>,
}

/// The musical brain. See the crate docs.
pub struct Composer {
    scene_name: String,
    transport: Transport,
    conductor: Conductor,
    form: Form,
    motifs: MotifMemory,
    answers: Answers,
    layers: Vec<Layer>,
    book: NoteBook,
    density_offset: f64,
    paused: bool,
    /// Commands produced by `handle` that go out on the next tick.
    queued: Vec<SynthCommand>,
    /// `HumanNote` commands waiting for the next tick: (key, velocity, on).
    human_queue: Vec<(u8, f32, bool)>,
    last_now: Seconds,
}

impl Composer {
    /// Build a composer for `scene`. Compiles every `on_event` expression, so
    /// a bad scene fails here rather than mid-performance. The caller resolves
    /// the seed (scene seed or a fresh one).
    pub fn new(scene: &Scene, seed: u64) -> Result<Self, ComposeError> {
        let transport = Transport::new(&scene.conductor);
        let conductor = Conductor::new(&scene.conductor, seed)?;
        let form = Form::new(
            &scene.conductor.form,
            seed,
            transport.bpm(),
            transport.beats_per_bar(),
        );
        let mut layers = Vec::with_capacity(scene.layers.len());
        for (i, cfg) in scene.layers.iter().enumerate() {
            let mappings = LayerMappings::compile(cfg)?;
            let stream = seed ^ generators::name_hash(&cfg.name);
            layers.push(Layer {
                id: LayerId(i as u16),
                cfg: cfg.clone(),
                generator: generators::for_role(cfg),
                mappings,
                rng: ChaCha8Rng::seed_from_u64(stream),
                jitter: ChaCha8Rng::seed_from_u64(stream.rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15),
                muted: cfg.muted,
                soloed: false,
                muted_sent: false,
                params: ParamCache::default(),
                last_params: BTreeMap::new(),
                human: (cfg.role == Role::Human).then(HumanTracker::new),
                mood_params: BTreeMap::new(),
            });
        }
        Ok(Self {
            scene_name: scene.name.clone(),
            transport,
            conductor,
            form,
            motifs: MotifMemory::new(),
            answers: Answers::new(seed),
            book: NoteBook::new(scene.layers.len()),
            layers,
            density_offset: 0.0,
            paused: false,
            queued: Vec::new(),
            human_queue: Vec::new(),
            last_now: 0.0,
        })
    }

    /// Effective mute: muted, or somebody else is soloed.
    pub fn layer_muted(&self, id: LayerId) -> bool {
        let any_solo = self.layers.iter().any(|l| l.soloed);
        match self.layers.get(id.0 as usize) {
            Some(l) => l.muted || (any_solo && !l.soloed),
            None => true,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Coarse external-clock alignment: see [`Transport::set_bar_phase`].
    pub fn set_bar_phase(&mut self, phase: f64) {
        self.transport.set_bar_phase(phase);
    }

    pub fn transport(&self) -> TransportPos {
        self.transport.pos()
    }

    /// The current harmonic state.
    pub fn harmony(&self) -> &Harmony {
        &self.conductor.harmony
    }

    /// The form engine (section, arc, targets).
    pub fn form(&self) -> &Form {
        &self.form
    }

    /// What the form is asking for right now (None for `Static`).
    pub fn form_targets(&self) -> Option<FormTargets> {
        self.form.targets()
    }

    /// Remembered motifs.
    pub fn motifs(&self) -> &MotifMemory {
        &self.motifs
    }

    /// Which unmuted layer answers calls: the first lead, else arp, else pad.
    fn answer_layer(&self) -> Option<LayerId> {
        [Role::Lead, Role::Arp, Role::Pad].iter().find_map(|role| {
            self.layers
                .iter()
                .find(|l| l.cfg.role == *role && !self.layer_muted(l.id))
                .map(|l| l.id)
        })
    }

    /// One control tick. `now` is engine time, `dt` the seconds since the last tick.
    pub fn tick(&mut self, now: Seconds, dt: f64, input: &ComposerInput<'_>) -> ComposerOutput {
        let mut out = ComposerOutput {
            commands: std::mem::take(&mut self.queued),
            notes: Vec::new(),
        };
        self.last_now = now;
        if self.paused {
            return out;
        }
        let dt = if dt.is_finite() && dt > 0.0 {
            dt.min(1.0)
        } else {
            0.0
        };
        let empty = LayerInput::default();

        // 0. A person at the keys: `HumanNote` commands become `note` / `note_off`
        //    events on every human layer, as if a `human.midi` source had sent them.
        let human_events = self.take_human_events(now);
        let merged: Vec<LayerEvent>;
        let events: &[LayerEvent] = if human_events.is_empty() {
            input.events
        } else {
            merged = input.events.iter().cloned().chain(human_events).collect();
            &merged
        };
        for l in &mut self.layers {
            let Some(tracker) = l.human.as_mut() else { continue };
            for ev in events.iter().filter(|e| e.layer == l.id && e.name == generators::human::NOTE_EVENT) {
                if let Some(key) = generators::human::event_key(ev) {
                    let v = ev
                        .attrs
                        .get("velocity")
                        .and_then(sonder_core::Value::as_f64)
                        .unwrap_or(ev.intensity);
                    if v.is_finite() && v > 0.0 {
                        tracker.note_on(key, v, ev.at);
                    }
                }
            }
            // `map.energy` defaults to the recent activity (notes/s, 8/s = 1).
            let inp = input.layers.get(l.id.0 as usize).unwrap_or(&empty);
            l.mood_params.clone_from(&inp.params);
            if !inp.params.contains_key(map_keys::ENERGY) {
                l.mood_params
                    .insert(map_keys::ENERGY.to_string(), tracker.activity(now));
            }
        }

        // 1. Mood: who is saying what.
        let sources: Vec<MoodSource<'_>> = self
            .layers
            .iter()
            .filter(|l| !l.muted)
            .map(|l| {
                let inp = input.layers.get(l.id.0 as usize).unwrap_or(&empty);
                MoodSource {
                    name: &l.cfg.name,
                    weight: l.cfg.mood_weight,
                    params: if l.human.is_some() { &l.mood_params } else { &inp.params },
                    signals: &inp.signals,
                }
            })
            .collect();
        let any_solo = self.layers.iter().any(|l| l.soloed);
        let sources: Vec<MoodSource<'_>> = if any_solo {
            sources
                .into_iter()
                .filter(|s| self.layers.iter().any(|l| l.cfg.name == s.name && l.soloed))
                .collect()
        } else {
            sources
        };
        self.conductor.update_mood(&sources, dt);

        // 2. Form: advance by bars, then tell the conductor what it wants.
        let energy = self.conductor.mood().energy;
        self.form.tick(
            self.transport.bar(),
            self.transport.bpm(),
            self.transport.beats_per_bar(),
            energy,
        );
        let strength = self.form.strength();
        let targets = self.form.targets();
        self.conductor.set_form(targets.map(|t| (t, strength)));
        let mood = self.conductor.mood();

        // 3. Clock.
        if let Some(bpm) = self.transport.tick(dt, mood.energy) {
            out.commands
                .push(SynthCommand::SetTempo { bpm: bpm as f32 });
        }

        // 4. Harmony.
        self.conductor.tick(self.transport.beats());

        // 5. Calls: salient events schedule an answer on a melodic layer. A human
        //    layer's call is a finished phrase (≥ 3 notes, then a beat of silence),
        //    answered with a figure that echoes the phrase's last interval.
        if self.form.call_and_response() {
            let answerer = self.answer_layer();
            let spb = self.transport.seconds_per_beat();
            for i in 0..self.layers.len() {
                let id = LayerId(i as u16);
                let muted = self.layer_muted(id);
                let Some(tracker) = self.layers[i].human.as_mut() else { continue };
                let Some(call) = tracker.take_call(now, spb) else { continue };
                if muted {
                    continue;
                }
                if let Some(target) = answerer.filter(|a| *a != id) {
                    let ev = call.as_event(id);
                    let midi = generators::human::snapped_pitch(
                        generators::human::snap_of(&self.layers[i].cfg),
                        &self.conductor.harmony,
                        call.key,
                        0.5,
                    );
                    self.answers.schedule_echo(
                        &ev,
                        Echo { midi, interval: call.interval },
                        target,
                        &self.transport,
                    );
                }
            }
            for ev in events {
                let Some(layer) = self.layers.get(ev.layer.0 as usize) else {
                    continue;
                };
                if layer.human.is_some() || self.layer_muted(layer.id) || !self.answers.is_salient(ev) {
                    continue;
                }
                if let Some(target) = answerer.filter(|a| *a != layer.id) {
                    let inp = input.layers.get(layer.id.0 as usize).unwrap_or(&empty);
                    let pitch = layer
                        .mappings
                        .trigger(ev, &inp.signals, now, self.transport.beats())
                        .pitch;
                    self.answers.schedule(ev, pitch, target, &self.transport);
                }
            }
        }
        let section = self.form.section().name();
        let cue = Cue {
            section,
            thin: self.form.thin(),
            rest_until_beat: 0.0,
            motif_recall: self.form.motif_recall(),
        };

        // 6. Each layer plays.
        for i in 0..self.layers.len() {
            let muted = self.layer_muted(LayerId(i as u16));
            let inp = input.layers.get(i).unwrap_or(&empty);
            let layer = &mut self.layers[i];
            if muted {
                self.answers.forget_layer(layer.id);
                if !layer.muted_sent {
                    self.book.silence_layer(layer.id);
                    out.commands
                        .push(SynthCommand::AllNotesOff { layer: layer.id });
                    layer
                        .params
                        .send(layer.id, LayerParam::Mute(true), &mut out.commands);
                    layer.generator.reset();
                    layer.muted_sent = true;
                }
                continue;
            }
            if layer.muted_sent {
                layer
                    .params
                    .send(layer.id, LayerParam::Mute(false), &mut out.commands);
                layer.muted_sent = false;
            }

            let events: Vec<&LayerEvent> = events
                .iter()
                .filter(|e| e.layer == layer.id)
                .collect();
            let params = Params::new(&inp.params, self.density_offset).with_form(targets, strength);
            // A person's notes are never jittered: latency matters more than feel.
            let humanize = if layer.human.is_some() {
                0.0
            } else {
                self.transport.humanize()
            };
            {
                let mut sink = NoteSink::new(
                    &mut self.book,
                    &mut layer.params,
                    &mut out.commands,
                    &mut out.notes,
                    &mut layer.jitter,
                    layer.id,
                    now,
                    humanize,
                )
                .with_reason(reason_for(layer.cfg.role));
                for p in self.answers.take_due(layer.id, self.transport.beats()) {
                    Answers::play(
                        &p,
                        layer.cfg.role,
                        params.register(),
                        &self.conductor.harmony,
                        &self.transport,
                        &mut sink,
                    );
                }
                let mut ctx = GenCtx {
                    now,
                    dt,
                    transport: &self.transport,
                    harmony: &self.conductor.harmony,
                    mood,
                    params,
                    signals: &inp.signals,
                    events: &events,
                    mappings: &layer.mappings,
                    cue: Cue {
                        rest_until_beat: self.answers.rest_until(layer.id),
                        ..cue
                    },
                    motifs: &mut self.motifs,
                    rng: &mut layer.rng,
                    sink,
                };
                layer.generator.tick(&mut ctx);
            }
            let dynamics = targets.map(|t| blend(1.0, 0.5 + 0.6 * t.dynamics, strength));
            Self::send_common_params(layer, &params, mood.brightness, dynamics, &mut out.commands);
            // The snapshot shows effective values: user offset and form blended in.
            layer.last_params = inp.params.clone();
            if layer.human.is_some() {
                if let Some(e) = layer.mood_params.get(map_keys::ENERGY) {
                    layer.last_params.insert(map_keys::ENERGY.to_string(), *e);
                }
            }
            let has_density = inp.params.contains_key(map_keys::DENSITY);
            if has_density || self.density_offset != 0.0 || targets.is_some() {
                let default = if has_density { 0.0 } else { 0.5 };
                layer
                    .last_params
                    .insert(map_keys::DENSITY.to_string(), params.density(default));
            }
            if targets.is_some() || inp.params.contains_key(map_keys::REGISTER) {
                layer
                    .last_params
                    .insert(map_keys::REGISTER.to_string(), params.register());
            }
        }

        // 7. Deferred triggers and planned releases that are due.
        self.book.flush(now, &mut out.commands, &mut out.notes);
        out
    }

    /// Turn queued `HumanNote` commands into `note` / `note_off` events on every
    /// human layer, appended to this tick's events. Empty when nothing was queued.
    fn take_human_events(&mut self, now: Seconds) -> Vec<LayerEvent> {
        if self.human_queue.is_empty() {
            return Vec::new();
        }
        let queue = std::mem::take(&mut self.human_queue);
        let mut events = Vec::new();
        for l in self.layers.iter().filter(|l| l.human.is_some()) {
            for &(key, velocity, on) in &queue {
                let mut attrs = BTreeMap::new();
                attrs.insert("key".to_string(), sonder_core::Value::Num(key as f64));
                let name = if on {
                    attrs.insert("velocity".to_string(), sonder_core::Value::Num(velocity as f64));
                    generators::human::NOTE_EVENT
                } else {
                    generators::human::NOTE_OFF_EVENT
                };
                events.push(LayerEvent {
                    layer: l.id,
                    name: name.to_string(),
                    at: now,
                    intensity: if on { velocity as f64 } else { 0.0 },
                    attrs,
                });
            }
        }
        events
    }

    /// Mixer-ish params every role shares. Only changed values are sent.
    fn send_common_params(
        layer: &mut Layer,
        params: &Params<'_>,
        brightness: f64,
        dynamics: Option<f64>,
        out: &mut Vec<SynthCommand>,
    ) {
        let id = layer.id;
        let gain = (params.get(map_keys::VOLUME).unwrap_or(1.0) * dynamics.unwrap_or(1.0))
            .clamp(0.0, 1.0) as f32
            * layer.cfg.gain;
        layer.params.send(id, LayerParam::Gain(gain), out);
        if let Some(p) = params.get(map_keys::PAN) {
            layer
                .params
                .send(id, LayerParam::Pan(p.clamp(0.0, 1.0) as f32), out);
        }
        match params.get(map_keys::CUTOFF) {
            Some(c) => layer.params.send(
                id,
                LayerParam::CutoffOctaves(((c.clamp(0.0, 1.0) - 0.5) * 4.0) as f32),
                out,
            ),
            // Sustaining roles breathe with the global brightness.
            None if matches!(layer.cfg.role, Role::Pad | Role::Drone | Role::Bells) => {
                layer.params.send(
                    id,
                    LayerParam::CutoffOctaves(((brightness - 0.5) * 2.0) as f32),
                    out,
                )
            }
            None => {}
        }
        if let Some(r) = params.get(map_keys::RESONANCE) {
            layer
                .params
                .send(id, LayerParam::Resonance(r.clamp(0.0, 1.0) as f32), out);
        }
        if let Some(r) = params.get(map_keys::REVERB) {
            layer
                .params
                .send(id, LayerParam::ReverbSend(r.clamp(0.0, 1.0) as f32), out);
        }
        if let Some(d) = params.get(map_keys::DELAY) {
            layer
                .params
                .send(id, LayerParam::DelaySend(d.clamp(0.0, 1.0) as f32), out);
        }
        if layer.cfg.role != Role::Texture {
            if let Some(t) = params.get(map_keys::TEXTURE) {
                layer
                    .params
                    .send(id, LayerParam::Texture(t.clamp(0.0, 1.0) as f32), out);
            }
        }
    }

    /// Apply a UI command. State changes take effect on the next tick; any
    /// resulting synth commands are emitted then.
    pub fn handle(&mut self, cmd: &UserCommand) {
        match cmd {
            UserCommand::ToggleMute { layer } => {
                if let Some(l) = self.layers.get_mut(layer.0 as usize) {
                    l.muted = !l.muted;
                }
            }
            UserCommand::ToggleSolo { layer } => {
                if let Some(l) = self.layers.get_mut(layer.0 as usize) {
                    let was = l.soloed;
                    for l in &mut self.layers {
                        l.soloed = false;
                    }
                    if !was {
                        self.layers[layer.0 as usize].soloed = true;
                    }
                }
            }
            UserCommand::SetLayerGain { layer, gain } => {
                if let Some(l) = self.layers.get_mut(layer.0 as usize) {
                    if gain.is_finite() {
                        l.cfg.gain = gain.clamp(0.0, 4.0);
                    }
                }
            }
            UserCommand::NudgeTempo { delta } => {
                if delta.is_finite() {
                    self.transport.nudge(*delta);
                }
            }
            UserCommand::NudgeDensity { delta } => {
                if delta.is_finite() {
                    self.density_offset = (self.density_offset + delta).clamp(-1.0, 1.0);
                }
            }
            UserCommand::Transpose { semitones } => self.conductor.transpose(*semitones),
            UserCommand::NextMode { backwards } => self.conductor.step_mode(*backwards),
            UserCommand::NextChord => self.conductor.next_chord(),
            UserCommand::TogglePause => {
                self.paused = !self.paused;
                if self.paused {
                    self.book.silence_all();
                    for l in &mut self.layers {
                        self.queued.push(SynthCommand::AllNotesOff { layer: l.id });
                        l.generator.reset();
                    }
                }
            }
            UserCommand::SaveScene { .. } | UserCommand::LoadScene { .. } | UserCommand::Quit => {}
            UserCommand::NextSection => self.form.request_next(),
            // Replay transport lives in the engine.
            UserCommand::Seek { .. } | UserCommand::SetSpeed { .. } => {}
            // Rooms and the DJ live in the engine.
            UserCommand::Chat { .. } | UserCommand::SetDj { .. } => {}
            UserCommand::HumanNote { key, velocity, on } => {
                if *key <= 127 && velocity.is_finite() && self.human_queue.len() < 256 {
                    self.human_queue
                        .push((*key, velocity.clamp(0.0, 1.0), *on));
                }
            }
        }
    }

    /// Write harmony, mood, transport, the conductor's note and per-layer
    /// state into a snapshot. Layer entries are created if missing; the
    /// engine fills signals, meters and sources.
    pub fn fill_snapshot(&self, snap: &mut EngineSnapshot) {
        if snap.scene_name.is_empty() {
            snap.scene_name = self.scene_name.clone();
        }
        snap.paused = self.paused;
        snap.transport = self.transport.pos();
        snap.mood = self.conductor.mood();
        self.conductor.fill(&mut snap.harmony);
        snap.conductor_note = format!(
            "{}; {}",
            self.conductor.note(self.transport.bpm()),
            self.form.describe()
        );
        self.fill_form(&mut snap.form);
        if snap.layers.len() < self.layers.len() {
            snap.layers
                .resize_with(self.layers.len(), LayerSnapshot::default);
        }
        for l in &self.layers {
            let s = &mut snap.layers[l.id.0 as usize];
            s.id = l.id;
            if s.name.is_empty() {
                s.name = l.cfg.name.clone();
            }
            s.role = l.cfg.role;
            if s.instrument.is_empty() {
                s.instrument = l.cfg.instrument.clone();
            }
            s.muted = self.layer_muted(l.id);
            s.soloed = l.soloed;
            s.active_notes = self.book.active_count(l.id);
            s.params = l.last_params.clone();
        }
    }
}

impl Composer {
    /// Fill the form snapshot: section, bars, arc, motif in play, and a summary.
    fn fill_form(&self, snap: &mut FormSnapshot) {
        self.form.fill(snap);
        snap.motif = self.motifs.current().to_string();
        let mut note = self.form.describe();
        if self.form.style() != sonder_core::FormStyle::Static {
            note.push_str(&format!(" · arc {:.2}", self.form.arc()));
        }
        if !snap.motif.is_empty() {
            note.push_str(&format!(" · motif {}", snap.motif));
        }
        if !self.answers.last_call().is_empty() {
            note.push_str(&format!(" · answering {}", self.answers.last_call()));
        }
        snap.note = note;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{ConductorConfig, Drift, Progression, Value};

    fn scene() -> Scene {
        let mut s = Scene {
            name: "t".into(),
            conductor: ConductorConfig {
                key: "D".into(),
                mode: "lydian".into(),
                tempo: 90.0,
                bars_per_chord: 1.0,
                drift: Drift::Fast,
                progression: Progression::Functional,
                mood_smoothing: 2.0,
                ..Default::default()
            },
            ..Default::default()
        };
        for (name, role) in [
            ("pad", Role::Pad),
            ("drone", Role::Drone),
            ("bass", Role::Bass),
            ("lead", Role::Lead),
            ("arp", Role::Arp),
            ("pluck", Role::Pluck),
            ("bells", Role::Bells),
            ("perc", Role::Perc),
            ("texture", Role::Texture),
        ] {
            s.layers.push(LayerConfig {
                name: name.into(),
                role,
                ..Default::default()
            });
        }
        s
    }

    fn run(scene: &Scene, seed: u64, seconds: f64) -> Vec<SynthCommand> {
        run_full(scene, seed, seconds).0
    }

    fn run_full(scene: &Scene, seed: u64, seconds: f64) -> (Vec<SynthCommand>, Vec<NoteSnapshot>) {
        let mut c = Composer::new(scene, seed).unwrap();
        let mut all = Vec::new();
        let mut notes = Vec::new();
        let n = (seconds / 0.02) as usize;
        let mut inputs: Vec<LayerInput> =
            scene.layers.iter().map(|_| LayerInput::default()).collect();
        for i in 0..n {
            let now = i as f64 * 0.02;
            let e = 0.5 + 0.5 * (now * 0.3).sin();
            inputs[0].params.insert(map_keys::ENERGY.into(), e);
            inputs[0]
                .params
                .insert(map_keys::BRIGHTNESS.into(), 1.0 - e);
            inputs[3].params.insert(map_keys::DENSITY.into(), e);
            let mut events = Vec::new();
            if i % 7 == 0 {
                let mut attrs = BTreeMap::new();
                attrs.insert("lane".into(), Value::Num((i % 4) as f64));
                events.push(LayerEvent {
                    layer: LayerId(5),
                    name: "car".into(),
                    at: now,
                    intensity: e,
                    attrs,
                });
                events.push(LayerEvent {
                    layer: LayerId(7),
                    name: "hit".into(),
                    at: now,
                    intensity: e,
                    attrs: BTreeMap::new(),
                });
            }
            if i == 500 {
                c.handle(&UserCommand::NextMode { backwards: false });
                c.handle(&UserCommand::Transpose { semitones: 2 });
                c.handle(&UserCommand::NudgeDensity { delta: 0.3 });
            }
            if i == 900 {
                c.handle(&UserCommand::TogglePause);
            }
            if i == 950 {
                c.handle(&UserCommand::TogglePause);
            }
            if i == 1200 {
                c.handle(&UserCommand::NextSection);
            }
            let out = c.tick(
                now,
                0.02,
                &ComposerInput {
                    layers: &inputs,
                    events: &events,
                },
            );
            all.extend(out.commands);
            notes.extend(out.notes);
        }
        (all, notes)
    }

    #[test]
    fn deterministic_for_same_seed_and_inputs() {
        let mut s = scene();
        s.conductor.form.style = sonder_core::FormStyle::Sections;
        s.conductor.form.section_bars = 4;
        s.conductor.form.motif_recall = 0.8;
        let (a, na) = run_full(&s, 42, 40.0);
        let (b, nb) = run_full(&s, 42, 40.0);
        assert_eq!(a, b);
        assert_eq!(na, nb, "notes (reasons, causes) must be identical too");
        assert!(a.iter().any(|c| matches!(c, SynthCommand::NoteOn { .. })));
        assert!(a.iter().any(|c| matches!(c, SynthCommand::Drum { .. })));
        let c = run(&s, 43, 40.0);
        assert_ne!(a, c, "a different seed should give a different performance");

        // Every note says why it happened; event notes say what caused them.
        assert!(
            na.iter().all(|n| !n.reason.is_empty()),
            "a note without a reason"
        );
        let trig: Vec<&NoteSnapshot> = na.iter().filter(|n| n.reason == TRIGGER_REASON).collect();
        assert!(!trig.is_empty());
        for n in &trig {
            let cause = n.cause.as_ref().expect("event note without a cause");
            assert!(matches!(cause.event.as_str(), "car" | "hit"), "unexpected cause {}", cause.event);
            assert!(cause.label.is_empty(), "the engine fills labels");
            if cause.event == "car" {
                assert!(cause.attrs.contains_key("lane"));
            }
            assert!(cause.at <= n.at + 1e-9);
        }
        for role in ["pad", "bass", "lead", "arp", "drone"] {
            assert!(na.iter().any(|n| n.reason == role), "no {role} note");
        }
        assert!(na
            .iter()
            .filter(|n| n.reason != TRIGGER_REASON)
            .all(|n| n.cause.is_none() || n.reason == ANSWER_REASON));
        assert!(
            na.iter().any(|n| n.reason == ANSWER_REASON),
            "no answer in 40 s of loud events"
        );
        assert!(na
            .iter()
            .filter(|n| n.reason == ANSWER_REASON)
            .all(|n| n.cause.is_some()));
    }

    #[test]
    fn next_section_jumps_at_the_bar() {
        let mut s = scene();
        s.conductor.form.style = sonder_core::FormStyle::Sections;
        s.conductor.form.section_bars = 16;
        let mut c = Composer::new(&s, 1).unwrap();
        let inputs: Vec<LayerInput> = s.layers.iter().map(|_| LayerInput::default()).collect();
        let input = ComposerInput {
            layers: &inputs,
            events: &[],
        };
        let mut snap = EngineSnapshot::default();
        for i in 0..100 {
            c.tick(i as f64 * 0.02, 0.02, &input);
        }
        c.fill_snapshot(&mut snap);
        assert_eq!(snap.form.section, "A");
        assert_eq!(snap.form.bars_in_section, 16);
        assert!(
            snap.conductor_note.contains("A 1/16"),
            "{}",
            snap.conductor_note
        );
        c.handle(&UserCommand::NextSection);
        // 90 bpm, 4/4: a bar is 2.67 s; the jump lands on the next bar line.
        let mut t = 2.0;
        let mut jumped_at = None;
        while t < 6.0 {
            c.tick(t, 0.02, &input);
            c.fill_snapshot(&mut snap);
            if snap.form.section == "B" {
                jumped_at = Some(t);
                break;
            }
            t += 0.02;
        }
        let t = jumped_at.expect("section should have changed");
        assert!((2.6..2.8).contains(&t), "jumped at {t}");
        assert_eq!(snap.form.bar_in_section, 0);
        assert!(snap.form.note.starts_with("B 1/16"), "{}", snap.form.note);
    }

    #[test]
    fn adding_a_layer_does_not_change_the_others() {
        let s = scene();
        let a = run(&s, 7, 20.0);
        let mut s2 = s.clone();
        s2.layers.push(LayerConfig {
            name: "extra".into(),
            role: Role::Arp,
            ..Default::default()
        });
        let b = run(&s2, 7, 20.0);
        let strip = |v: &[SynthCommand]| -> Vec<SynthCommand> {
            v.iter()
                .filter(|c| match c {
                    SynthCommand::NoteOn { layer, .. }
                    | SynthCommand::NoteOff { layer, .. }
                    | SynthCommand::NotePitch { layer, .. }
                    | SynthCommand::Drum { layer, .. }
                    | SynthCommand::AllNotesOff { layer }
                    | SynthCommand::SetLayerParam { layer, .. } => layer.0 < 9,
                    _ => true,
                })
                .cloned()
                .collect()
        };
        // Note ids differ (shared counter) but pitches and timing must not.
        let shape = |v: Vec<SynthCommand>| -> Vec<String> {
            v.into_iter()
                .map(|c| match c {
                    SynthCommand::NoteOn {
                        layer,
                        pitch_hz,
                        velocity,
                        ..
                    } => format!("on {layer} {pitch_hz:.3} {velocity:.3}"),
                    SynthCommand::NoteOff { layer, .. } => format!("off {layer}"),
                    SynthCommand::Drum { layer, drum, .. } => format!("drum {layer} {drum:?}"),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        assert_eq!(shape(a), shape(strip(&b)));
    }

    #[test]
    fn mute_solo_and_pause_behave() {
        let s = scene();
        let mut c = Composer::new(&s, 1).unwrap();
        let inputs: Vec<LayerInput> = s.layers.iter().map(|_| LayerInput::default()).collect();
        let input = ComposerInput {
            layers: &inputs,
            events: &[],
        };
        c.tick(0.0, 0.02, &input);
        assert!(!c.layer_muted(LayerId(0)));
        c.handle(&UserCommand::ToggleSolo { layer: LayerId(2) });
        assert!(c.layer_muted(LayerId(0)) && !c.layer_muted(LayerId(2)));
        let out = c.tick(0.02, 0.02, &input);
        let offs = out
            .commands
            .iter()
            .filter(|c| matches!(c, SynthCommand::AllNotesOff { .. }))
            .count();
        assert_eq!(offs, 8, "every non-soloed layer is silenced once");
        let out = c.tick(0.04, 0.02, &input);
        assert!(!out
            .commands
            .iter()
            .any(|c| matches!(c, SynthCommand::AllNotesOff { .. })));
        c.handle(&UserCommand::ToggleSolo { layer: LayerId(2) });
        assert!(!c.layer_muted(LayerId(0)));
        c.handle(&UserCommand::TogglePause);
        assert!(c.is_paused());
        let out = c.tick(0.06, 0.02, &input);
        assert_eq!(
            out.commands
                .iter()
                .filter(|c| matches!(c, SynthCommand::AllNotesOff { .. }))
                .count(),
            9
        );
        assert!(out.notes.is_empty());
        let pos = c.transport();
        let out = c.tick(1.0, 0.94, &input);
        assert!(out.commands.is_empty());
        assert_eq!(c.transport(), pos, "transport frozen while paused");
        let mut snap = EngineSnapshot::default();
        c.fill_snapshot(&mut snap);
        assert!(snap.paused);
        assert_eq!(snap.layers.len(), 9);
        assert_eq!(snap.harmony.key_name, "D");
        assert!(snap.conductor_note.contains("tempo"));
    }

    #[test]
    fn bad_scenes_fail_loudly() {
        let mut s = scene();
        s.conductor.mode = "zorblax".into();
        assert!(matches!(
            Composer::new(&s, 0),
            Err(ComposeError::UnknownMode(_))
        ));
        let mut s = scene();
        s.conductor.key = "H".into();
        assert!(matches!(
            Composer::new(&s, 0),
            Err(ComposeError::UnknownKey(_))
        ));
    }
}
