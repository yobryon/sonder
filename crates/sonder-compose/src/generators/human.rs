//! Human: a person in the ensemble.
//!
//! Plays `note { key, velocity }` / `note_off { key }` events (from a `human.midi`
//! source, or from [`sonder_core::UserCommand::HumanNote`] fed in by the composer)
//! and snaps each key to the conductor's harmony on NoteOn:
//!
//! * `snap = "chord"` → nearest chord tone, `"scale"` (default) → nearest scale
//!   tone, `"none"` → the key as played. Read from `on_event.note.snap`; no other
//!   scene config is needed.
//! * The layer's `register` param shifts the result by an octave (±1) when snapping
//!   is on (0 = down, 0.5 = as played, 1 = up).
//! * A note sustains until its `note_off` (or [`SAFETY_BEATS`] beats, whichever
//!   comes first). Up to [`MAX_HELD`] keys sound at once; the oldest is released
//!   to make room. A `pedal { on }` event holds released keys until the pedal lifts.
//! * Velocity → note velocity, unmodified. Humanize jitter is bypassed for this
//!   role by the composer (latency matters more than feel).
//!
//! Notes carry `reason = "human"` and a cause whose attrs include the `key`.
//!
//! [`HumanTracker`] is the composer-side bookkeeping for a human layer: recent
//! activity (the layer's default `energy` contribution to the mood) and phrase
//! detection for call-and-response.

use std::collections::{BTreeMap, VecDeque};

use sonder_core::{map_keys, LayerConfig, LayerEvent, NoteId, Seconds, Value};
use sonder_theory::Quantizer;

use super::{GenCtx, Generator};
use crate::conductor::Harmony;
use crate::eventmap::Snap;
use crate::notes::cause_of;

/// `NoteSnapshot.reason` for notes a person plays.
pub const HUMAN_REASON: &str = "human";
/// Most keys sounding at once; the oldest is released to make room.
pub const MAX_HELD: usize = 10;
/// A held key is released after this many beats even without a `note_off`.
pub const SAFETY_BEATS: f64 = 8.0;
/// Event names the generator listens to.
pub const NOTE_EVENT: &str = "note";
pub const NOTE_OFF_EVENT: &str = "note_off";
pub const PEDAL_EVENT: &str = "pedal";
/// Event name of a detected phrase (the cause of the answer it provokes).
pub const PHRASE_EVENT: &str = "phrase";
/// Notes further apart than this start a new phrase.
pub const PHRASE_GAP_S: f64 = 2.0;
/// A phrase needs at least this many notes to count as a call.
pub const PHRASE_MIN_NOTES: usize = 3;
/// Silence after the last note before a phrase is treated as finished, in beats.
pub const PHRASE_SILENCE_BEATS: f64 = 1.0;
/// Window for the activity measure, in seconds.
pub const ACTIVITY_WINDOW_S: f64 = 4.0;
/// Notes per second that count as full activity (1.0).
pub const ACTIVITY_FULL_RATE: f64 = 8.0;

/// The `snap` policy of a human layer: `on_event.note.snap`, default `scale`.
pub fn snap_of(cfg: &LayerConfig) -> Snap {
    cfg.on_event
        .get(NOTE_EVENT)
        .and_then(|m| m.get("snap"))
        .and_then(|s| Snap::parse(s))
        .unwrap_or(Snap::Scale)
}

/// Where a played key lands: snapped to the harmony and shifted by the register.
pub fn snapped_pitch(snap: Snap, harmony: &Harmony, key: u8, register: f64) -> f64 {
    let key = key.min(127) as f64;
    if snap == Snap::None {
        return key;
    }
    let shift = ((register.clamp(0.0, 1.0) - 0.5) * 2.0).round() * 12.0;
    let pcs = match snap {
        Snap::Chord if !harmony.chord_pcs.is_empty() => &harmony.chord_pcs,
        _ => &harmony.scale_pcs,
    };
    let midi = Quantizer::nearest_in(pcs, key) as f64 + shift;
    midi.clamp(0.0, 127.0)
}

fn attr_f64(ev: &LayerEvent, key: &str) -> Option<f64> {
    ev.attrs.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
}

/// The `key` attribute of a note event, if it is a MIDI number.
pub fn event_key(ev: &LayerEvent) -> Option<u8> {
    let k = attr_f64(ev, "key")?.round();
    if (0.0..=127.0).contains(&k) {
        Some(k as u8)
    } else {
        None
    }
}

pub struct Human {
    snap: Snap,
    /// Sounding keys in the order they were pressed.
    held: Vec<(u8, NoteId)>,
    pedal: bool,
    /// Keys released while the pedal was down.
    sustained: Vec<NoteId>,
}

impl Human {
    pub fn new(cfg: &LayerConfig) -> Self {
        Self::with_snap(snap_of(cfg))
    }

    pub fn with_snap(snap: Snap) -> Self {
        Self {
            snap,
            held: Vec::new(),
            pedal: false,
            sustained: Vec::new(),
        }
    }

    pub fn snap(&self) -> Snap {
        self.snap
    }

    /// Keys currently held (sounding or pedal-sustained), oldest first.
    pub fn held_keys(&self) -> Vec<u8> {
        self.held.iter().map(|(k, _)| *k).collect()
    }

    fn lift(&mut self, key: u8, ctx: &mut GenCtx<'_>) {
        if let Some(i) = self.held.iter().position(|(k, _)| *k == key) {
            let (_, id) = self.held.remove(i);
            if self.pedal {
                self.sustained.push(id);
            } else {
                ctx.sink.release(id);
            }
        }
    }
}

impl Generator for Human {
    fn tick(&mut self, ctx: &mut GenCtx<'_>) {
        // Safety releases fired by the note book: forget those keys.
        let live: Vec<bool> = self.held.iter().map(|(_, id)| ctx.sink.is_live(*id)).collect();
        let mut i = 0;
        self.held.retain(|_| {
            let keep = live[i];
            i += 1;
            keep
        });
        let register = ctx.params.or(map_keys::REGISTER, 0.5);
        let safety = SAFETY_BEATS * ctx.transport.seconds_per_beat();
        let beats = ctx.transport.beats();
        for ev in ctx.events {
            match ev.name.as_str() {
                NOTE_EVENT => {
                    let Some(key) = event_key(ev) else { continue };
                    let velocity = attr_f64(ev, "velocity")
                        .unwrap_or(if ev.intensity.is_finite() { ev.intensity } else { 0.8 })
                        .clamp(0.0, 1.0);
                    if velocity <= 0.0 {
                        // Running-status NoteOn with velocity 0 is a NoteOff.
                        self.lift(key, ctx);
                        continue;
                    }
                    // Re-striking a held key restarts it.
                    if self.held.iter().any(|(k, _)| *k == key) {
                        let was_pedal = self.pedal;
                        self.pedal = false;
                        self.lift(key, ctx);
                        self.pedal = was_pedal;
                    }
                    while self.held.len() >= MAX_HELD {
                        let (_, id) = self.held.remove(0);
                        ctx.sink.release(id);
                    }
                    let pan = ctx.mappings.trigger(ev, ctx.signals, ctx.now, beats).pan;
                    let midi = snapped_pitch(self.snap, ctx.harmony, key, register);
                    if let Some(id) = ctx.sink.play_as(
                        midi,
                        velocity,
                        pan,
                        Some(safety),
                        0.0,
                        HUMAN_REASON,
                        Some(cause_of(ev)),
                    ) {
                        self.held.push((key, id));
                    }
                }
                NOTE_OFF_EVENT => {
                    if let Some(key) = event_key(ev) {
                        self.lift(key, ctx);
                    }
                }
                PEDAL_EVENT => {
                    let on = attr_f64(ev, "on").unwrap_or(ev.intensity) >= 0.5;
                    self.pedal = on;
                    if !on {
                        for id in self.sustained.drain(..) {
                            ctx.sink.release(id);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn reset(&mut self) {
        self.held.clear();
        self.sustained.clear();
        self.pedal = false;
    }
}

/// A finished phrase that deserves an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// Last key of the phrase.
    pub key: u8,
    /// Semitones from the second-to-last key to the last one.
    pub interval: i32,
    /// Onset of the last note.
    pub at: Seconds,
    pub notes: usize,
    /// Mean velocity of the phrase, 0..1.
    pub velocity: f64,
}

impl Call {
    /// A synthetic event the answer can cite as its cause.
    pub fn as_event(&self, layer: sonder_core::LayerId) -> LayerEvent {
        let mut attrs = BTreeMap::new();
        attrs.insert("key".to_string(), Value::Num(self.key as f64));
        attrs.insert("interval".to_string(), Value::Num(self.interval as f64));
        attrs.insert("notes".to_string(), Value::Num(self.notes as f64));
        LayerEvent {
            layer,
            name: PHRASE_EVENT.to_string(),
            at: self.at,
            intensity: self.velocity,
            attrs,
        }
    }
}

/// Composer-side bookkeeping for one human layer: activity and phrases.
#[derive(Debug, Default, Clone)]
pub struct HumanTracker {
    onsets: VecDeque<Seconds>,
    /// The phrase in progress: (key, velocity, onset).
    phrase: Vec<(u8, f64, Seconds)>,
}

impl HumanTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a NoteOn.
    pub fn note_on(&mut self, key: u8, velocity: f64, at: Seconds) {
        self.onsets.push_back(at);
        if let Some(&(_, _, last)) = self.phrase.last() {
            if at - last > PHRASE_GAP_S {
                self.phrase.clear();
            }
        }
        self.phrase.push((key, velocity.clamp(0.0, 1.0), at));
        if self.phrase.len() > 64 {
            self.phrase.remove(0);
        }
    }

    /// Recent activity: notes per second over the last 4 s, normalised so that
    /// 8 notes/s = 1. The layer's default `energy` contribution.
    pub fn activity(&mut self, now: Seconds) -> f64 {
        while let Some(&t) = self.onsets.front() {
            if now - t > ACTIVITY_WINDOW_S {
                self.onsets.pop_front();
            } else {
                break;
            }
        }
        (self.onsets.len() as f64 / ACTIVITY_WINDOW_S / ACTIVITY_FULL_RATE).clamp(0.0, 1.0)
    }

    /// Has the person played anything yet?
    pub fn is_empty(&self) -> bool {
        self.onsets.is_empty() && self.phrase.is_empty()
    }

    /// A phrase of ≥ 3 notes followed by a beat of silence is a call. Returns it
    /// once and forgets the phrase.
    pub fn take_call(&mut self, now: Seconds, seconds_per_beat: f64) -> Option<Call> {
        let &(key, _, last) = self.phrase.last()?;
        if now - last < PHRASE_SILENCE_BEATS * seconds_per_beat {
            return None;
        }
        if self.phrase.len() < PHRASE_MIN_NOTES {
            // A fragment: it may still grow into a phrase (the 2 s gap rule decides).
            return None;
        }
        let n = self.phrase.len();
        let prev = self.phrase[n - 2].0;
        let velocity = self.phrase.iter().map(|(_, v, _)| *v).sum::<f64>() / n as f64;
        self.phrase.clear();
        Some(Call {
            key,
            interval: key as i32 - prev as i32,
            at: last,
            notes: n,
            velocity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::{Harmony, HarmonyChange};
    use crate::eventmap::LayerMappings;
    use crate::generators::{Cue, Params};
    use crate::motif::MotifMemory;
    use crate::notes::{NoteBook, NoteSink, ParamCache};
    use crate::transport::Transport;
    use crate::{Composer, ComposerInput, LayerInput, ANSWER_REASON};
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;
    use sonder_core::{
        hz_to_midi, ConductorConfig, LayerId, Mood, NoteSnapshot, Role, Scene, SynthCommand,
        UserCommand,
    };
    use sonder_theory::{diatonic_chord, mode_by_name, Scale};

    /// D lydian (D E F# G# A B C#), chord I (D F# A).
    fn harmony() -> Harmony {
        let scale = Scale::new(2, mode_by_name("lydian").unwrap());
        let chord = diatonic_chord(&scale, 0, 0);
        Harmony {
            scale_pcs: scale.pitch_classes(),
            chord_pcs: chord.pitch_classes(&scale),
            scale,
            chord,
            change: HarmonyChange::default(),
            beats_to_change: 4.0,
        }
    }

    fn note(key: u8, velocity: f64, at: Seconds) -> LayerEvent {
        let mut attrs = BTreeMap::new();
        attrs.insert("key".into(), Value::Num(key as f64));
        attrs.insert("velocity".into(), Value::Num(velocity));
        LayerEvent {
            layer: LayerId(0),
            name: NOTE_EVENT.into(),
            at,
            intensity: velocity,
            attrs,
        }
    }

    fn note_off(key: u8, at: Seconds) -> LayerEvent {
        let mut attrs = BTreeMap::new();
        attrs.insert("key".into(), Value::Num(key as f64));
        LayerEvent {
            layer: LayerId(0),
            name: NOTE_OFF_EVENT.into(),
            at,
            intensity: 0.0,
            attrs,
        }
    }

    fn pedal(on: bool, at: Seconds) -> LayerEvent {
        let mut attrs = BTreeMap::new();
        attrs.insert("on".into(), Value::Bool(on));
        LayerEvent {
            layer: LayerId(0),
            name: PEDAL_EVENT.into(),
            at,
            intensity: if on { 1.0 } else { 0.0 },
            attrs,
        }
    }

    /// A tiny harness: one human layer, 120 bpm, explicit events per tick.
    struct Rig {
        human: Human,
        harmony: Harmony,
        transport: Transport,
        book: NoteBook,
        cache: ParamCache,
        out: Vec<SynthCommand>,
        notes: Vec<NoteSnapshot>,
        rng: ChaCha8Rng,
        jitter: ChaCha8Rng,
        motifs: MotifMemory,
        params: BTreeMap<String, f64>,
        now: Seconds,
    }

    impl Rig {
        fn new(snap: Snap) -> Self {
            Self {
                human: Human::with_snap(snap),
                harmony: harmony(),
                transport: Transport::new(&ConductorConfig {
                    tempo: 120.0,
                    tempo_range: [120.0, 120.0],
                    ..Default::default()
                }),
                book: NoteBook::new(1),
                cache: ParamCache::default(),
                out: Vec::new(),
                notes: Vec::new(),
                rng: ChaCha8Rng::seed_from_u64(1),
                jitter: ChaCha8Rng::seed_from_u64(1),
                motifs: MotifMemory::new(),
                params: BTreeMap::new(),
                now: 0.0,
            }
        }

        fn tick(&mut self, events: &[LayerEvent]) {
            self.now += 0.02;
            self.transport.tick(0.02, 0.5);
            let evs: Vec<&LayerEvent> = events.iter().collect();
            let mappings = LayerMappings::default();
            let signals = BTreeMap::new();
            let mut ctx = GenCtx {
                now: self.now,
                dt: 0.02,
                transport: &self.transport,
                harmony: &self.harmony,
                mood: Mood::default(),
                params: Params::new(&self.params, 0.0),
                signals: &signals,
                events: &evs,
                mappings: &mappings,
                cue: Cue::default(),
                motifs: &mut self.motifs,
                rng: &mut self.rng,
                sink: NoteSink::new(
                    &mut self.book,
                    &mut self.cache,
                    &mut self.out,
                    &mut self.notes,
                    &mut self.jitter,
                    LayerId(0),
                    self.now,
                    0.0,
                ),
            };
            self.human.tick(&mut ctx);
            self.book.flush(self.now, &mut self.out, &mut self.notes);
        }

        fn ons(&self) -> Vec<i32> {
            self.out
                .iter()
                .filter_map(|c| match c {
                    SynthCommand::NoteOn { pitch_hz, .. } => {
                        Some(hz_to_midi(*pitch_hz as f64).round() as i32)
                    }
                    _ => None,
                })
                .collect()
        }

        fn offs(&self) -> usize {
            self.out
                .iter()
                .filter(|c| matches!(c, SynthCommand::NoteOff { .. }))
                .count()
        }
    }

    #[test]
    fn snap_chord_scale_none() {
        // C4 (60) is not in D lydian: scale → B3 (59, tie goes lower); chord D F# A → D4 (62).
        let h = harmony();
        assert_eq!(snapped_pitch(Snap::Scale, &h, 60, 0.5), 59.0);
        assert_eq!(snapped_pitch(Snap::Chord, &h, 60, 0.5), 62.0);
        assert_eq!(snapped_pitch(Snap::None, &h, 60, 0.5), 60.0);
        // In-scale keys are untouched by scale snap.
        assert_eq!(snapped_pitch(Snap::Scale, &h, 61, 0.5), 61.0);
        // Register shifts by whole octaves only when snapping.
        assert_eq!(snapped_pitch(Snap::Scale, &h, 61, 1.0), 73.0);
        assert_eq!(snapped_pitch(Snap::Scale, &h, 61, 0.0), 49.0);
        assert_eq!(snapped_pitch(Snap::Chord, &h, 60, 0.9), 74.0);
        assert_eq!(snapped_pitch(Snap::None, &h, 60, 1.0), 60.0);
        // Through the generator.
        for (snap, want) in [(Snap::Scale, 59), (Snap::Chord, 62), (Snap::None, 60)] {
            let mut rig = Rig::new(snap);
            rig.tick(&[note(60, 0.7, 0.0)]);
            assert_eq!(rig.ons(), vec![want], "{snap:?}");
            let n = &rig.notes[0];
            assert_eq!(n.reason, HUMAN_REASON);
            assert!((n.velocity - 0.7).abs() < 1e-6);
            let cause = n.cause.as_ref().unwrap();
            assert_eq!(cause.event, NOTE_EVENT);
            assert_eq!(cause.attrs.get("key"), Some(&Value::Num(60.0)));
        }
    }

    #[test]
    fn snap_is_read_from_the_scene_and_defaults_to_scale() {
        let mut cfg = LayerConfig {
            name: "you".into(),
            role: Role::Human,
            ..Default::default()
        };
        assert_eq!(Human::new(&cfg).snap(), Snap::Scale);
        let mut m = BTreeMap::new();
        m.insert("snap".to_string(), "chord".to_string());
        cfg.on_event.insert(NOTE_EVENT.into(), m);
        assert_eq!(Human::new(&cfg).snap(), Snap::Chord);
        cfg.on_event.get_mut(NOTE_EVENT).unwrap().insert("snap".into(), "none".into());
        assert_eq!(Human::new(&cfg).snap(), Snap::None);
    }

    #[test]
    fn sustains_until_note_off_with_an_eight_beat_safety() {
        let mut rig = Rig::new(Snap::None);
        rig.tick(&[note(64, 0.9, 0.0)]);
        assert_eq!(rig.notes.len(), 1);
        // 120 bpm → 8 beats = 4 s.
        assert!((rig.notes[0].duration.unwrap() - 4.0).abs() < 1e-9);
        for _ in 0..50 {
            rig.tick(&[]);
        }
        assert_eq!(rig.offs(), 0, "still held after a second");
        assert_eq!(rig.book.active_count(LayerId(0)), 1);
        rig.tick(&[note_off(64, rig.now)]);
        assert_eq!(rig.offs(), 1);
        assert_eq!(rig.book.active_count(LayerId(0)), 0);
        assert!(rig.human.held_keys().is_empty());
        // A note_off for a key that is not held is ignored.
        rig.tick(&[note_off(64, rig.now)]);
        assert_eq!(rig.offs(), 1);

        // Never released by the player: the safety timer lets go after 8 beats.
        let mut rig = Rig::new(Snap::None);
        rig.tick(&[note(64, 0.9, 0.0)]);
        for _ in 0..190 {
            rig.tick(&[]);
        }
        assert_eq!(rig.offs(), 0, "3.8 s: still sounding");
        for _ in 0..15 {
            rig.tick(&[]);
        }
        assert_eq!(rig.offs(), 1, "4.1 s: safety release");
        assert!(rig.human.held_keys().is_empty(), "the generator forgets it too");
    }

    #[test]
    fn polyphony_is_capped_and_restrikes_restart() {
        let mut rig = Rig::new(Snap::None);
        let evs: Vec<LayerEvent> = (0..12).map(|k| note(48 + k, 0.5, 0.0)).collect();
        rig.tick(&evs);
        assert_eq!(rig.ons().len(), 12);
        assert_eq!(rig.book.active_count(LayerId(0)), MAX_HELD as u16);
        assert_eq!(rig.offs(), 2, "the two oldest were released");
        assert_eq!(rig.human.held_keys(), (50..60).collect::<Vec<u8>>());
        // Striking a held key again releases the old voice and starts a new one.
        rig.tick(&[note(55, 0.5, rig.now)]);
        assert_eq!(rig.offs(), 3);
        assert_eq!(rig.book.active_count(LayerId(0)), MAX_HELD as u16);
        assert_eq!(*rig.human.held_keys().last().unwrap(), 55);
    }

    #[test]
    fn pedal_holds_released_keys() {
        let mut rig = Rig::new(Snap::None);
        rig.tick(&[pedal(true, 0.0), note(60, 0.5, 0.0), note(64, 0.5, 0.0)]);
        rig.tick(&[note_off(60, rig.now), note_off(64, rig.now)]);
        assert_eq!(rig.offs(), 0, "pedal down: nothing released");
        assert!(rig.human.held_keys().is_empty(), "but the keys are up");
        rig.tick(&[pedal(false, rig.now)]);
        assert_eq!(rig.offs(), 2);
    }

    #[test]
    fn velocity_zero_is_a_note_off_and_bad_keys_are_ignored() {
        let mut rig = Rig::new(Snap::None);
        rig.tick(&[note(60, 0.5, 0.0)]);
        rig.tick(&[note(60, 0.0, rig.now)]);
        assert_eq!(rig.offs(), 1);
        let mut bad = note(60, 0.5, 0.0);
        bad.attrs.insert("key".into(), Value::Num(300.0));
        let mut nan = note(60, 0.5, 0.0);
        nan.attrs.insert("key".into(), Value::Num(f64::NAN));
        let mut text = note(60, 0.5, 0.0);
        text.attrs.insert("key".into(), Value::Text("C4".into()));
        rig.tick(&[bad, nan, text]);
        assert_eq!(rig.ons().len(), 1);
    }

    #[test]
    fn tracker_measures_activity_and_detects_phrases() {
        let mut t = HumanTracker::new();
        assert!(t.is_empty());
        assert_eq!(t.activity(0.0), 0.0);
        for i in 0..8 {
            t.note_on(60 + i, 0.5, i as f64 * 0.25);
        }
        // 8 notes in the last 4 s → 2 notes/s → 0.25.
        assert!((t.activity(2.0) - 0.25).abs() < 1e-9);
        assert!((t.activity(6.5) - 0.0).abs() < 1e-9, "window slid past them");

        // Phrase: three notes, then a beat of silence at 120 bpm (0.5 s).
        let mut t = HumanTracker::new();
        t.note_on(60, 0.6, 10.0);
        t.note_on(64, 0.8, 10.3);
        assert_eq!(t.take_call(11.0, 0.5), None, "two notes are not a phrase");
        t.note_on(67, 0.7, 11.1);
        t.note_on(65, 0.7, 11.4);
        assert_eq!(t.take_call(11.6, 0.5), None, "not yet silent");
        let call = t.take_call(12.0, 0.5).expect("a call");
        assert_eq!(call.key, 65);
        assert_eq!(call.interval, -2);
        assert_eq!(call.notes, 4);
        assert!((call.velocity - 0.7).abs() < 1e-9);
        assert_eq!(call.at, 11.4);
        assert_eq!(t.take_call(13.0, 0.5), None, "reported once");
        let ev = call.as_event(LayerId(3));
        assert_eq!(ev.name, PHRASE_EVENT);
        assert_eq!(ev.attrs.get("interval"), Some(&Value::Num(-2.0)));

        // A gap longer than 2 s starts a new phrase.
        let mut t = HumanTracker::new();
        t.note_on(60, 0.5, 0.0);
        t.note_on(62, 0.5, 0.5);
        t.note_on(64, 0.5, 3.0);
        t.note_on(65, 0.5, 3.2);
        assert_eq!(t.take_call(4.0, 0.5), None, "only two notes since the gap");
    }

    fn jam() -> Scene {
        let mut s = Scene {
            name: "jam".into(),
            conductor: ConductorConfig {
                tempo: 120.0,
                tempo_range: [120.0, 120.0],
                humanize: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        s.layers.push(LayerConfig {
            name: "you".into(),
            role: Role::Human,
            ..Default::default()
        });
        s.layers.push(LayerConfig {
            name: "duet".into(),
            role: Role::Lead,
            ..Default::default()
        });
        s
    }

    #[test]
    fn human_note_commands_play_immediately_without_humanize() {
        let s = jam();
        let mut c = Composer::new(&s, 7).unwrap();
        let inputs: Vec<LayerInput> = s.layers.iter().map(|_| LayerInput::default()).collect();
        let input = ComposerInput {
            layers: &inputs,
            events: &[],
        };
        c.tick(0.0, 0.02, &input);
        for k in 0..20u8 {
            c.handle(&UserCommand::HumanNote {
                key: 60 + k % 5,
                velocity: 0.65,
                on: true,
            });
            let now = 0.02 + k as f64 * 0.02;
            let out = c.tick(now, 0.02, &input);
            let mine: Vec<&NoteSnapshot> =
                out.notes.iter().filter(|n| n.layer == LayerId(0)).collect();
            assert_eq!(mine.len(), 1, "tick {k}: {:?}", out.notes);
            assert_eq!(mine[0].at, now, "no timing jitter");
            assert!((mine[0].velocity - 0.65).abs() < 1e-6, "no velocity jitter");
            assert_eq!(mine[0].reason, HUMAN_REASON);
            c.handle(&UserCommand::HumanNote {
                key: 60 + k % 5,
                velocity: 0.0,
                on: false,
            });
        }
        let mut snap = sonder_core::EngineSnapshot::default();
        c.fill_snapshot(&mut snap);
        // Nothing mapped `energy`, so the layer's activity stands in for it.
        let energy = snap.layers[0].params.get(map_keys::ENERGY).copied();
        assert!(energy.is_some_and(|e| e > 0.0), "{:?}", snap.layers[0].params);
    }

    #[test]
    fn a_phrase_is_answered_by_the_lead() {
        let s = jam();
        let mut c = Composer::new(&s, 3).unwrap();
        let mut inputs: Vec<LayerInput> = s.layers.iter().map(|_| LayerInput::default()).collect();
        // Keep the lead's own melody out of the way so the answer is unambiguous.
        inputs[1].params.insert(map_keys::DENSITY.into(), 0.0);
        let mut notes = Vec::new();
        let mut t = 0.0;
        let step = |c: &mut Composer, notes: &mut Vec<NoteSnapshot>, t: &mut f64| {
            *t += 0.02;
            let out = c.tick(
                *t,
                0.02,
                &ComposerInput {
                    layers: &inputs,
                    events: &[],
                },
            );
            notes.extend(out.notes);
        };
        // Four notes a quarter second apart, ending on a rising fifth (60 → 67).
        for (i, key) in [62u8, 64, 60, 67].into_iter().enumerate() {
            c.handle(&UserCommand::HumanNote {
                key,
                velocity: 0.8,
                on: true,
            });
            step(&mut c, &mut notes, &mut t);
            c.handle(&UserCommand::HumanNote {
                key,
                velocity: 0.0,
                on: false,
            });
            for _ in 0..11 {
                step(&mut c, &mut notes, &mut t);
            }
            let _ = i;
        }
        // Then 3 s of silence: the phrase ends, the answer lands within a bar.
        for _ in 0..150 {
            step(&mut c, &mut notes, &mut t);
        }
        let answers: Vec<&NoteSnapshot> = notes.iter().filter(|n| n.reason == ANSWER_REASON).collect();
        assert!(!answers.is_empty(), "no answer to the phrase: {notes:?}");
        assert!(answers.iter().all(|n| n.layer == LayerId(1)));
        let cause = answers[0].cause.as_ref().expect("answers cite the phrase");
        assert_eq!(cause.event, PHRASE_EVENT);
        assert_eq!(cause.attrs.get("interval"), Some(&Value::Num(7.0)));
        // The figure echoes the interval: two notes, the final a fifth-ish above the first.
        assert_eq!(answers.len(), 2, "{answers:?}");
        let iv = answers[1].midi - answers[0].midi;
        assert!((6.0..=8.0).contains(&iv), "echoed interval {iv}");
        assert!(answers[1].at > answers[0].at);
        // Human notes themselves are not treated as generic salient calls.
        assert!(answers.iter().all(|n| n.cause.as_ref().unwrap().event != NOTE_EVENT));
    }
}
