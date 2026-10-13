//! Note bookkeeping: ids, scheduled releases, deferred triggers, snapshots.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use rand::RngExt;
use rand_chacha::ChaCha8Rng;
use sonder_core::{midi_to_hz, Drum, LayerId, LayerParam, NoteId, NoteSnapshot, Seconds, SynthCommand};

/// Max timing jitter at humanize = 1, in seconds.
const JITTER_S: f64 = 0.015;
/// Max velocity jitter at humanize = 1, as a fraction.
const VEL_JITTER: f64 = 0.10;
/// New triggers a single layer may fire per tick before extras are dropped.
pub const TRIGGERS_PER_TICK: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct When {
    /// Time as ordered bits (times are non-negative).
    t: u64,
    seq: u64,
}

impl When {
    fn new(t: Seconds, seq: u64) -> Self {
        Self { t: t.max(0.0).to_bits(), seq }
    }
    fn time(&self) -> Seconds {
        f64::from_bits(self.t)
    }
}

#[derive(Debug, Clone)]
enum Trigger {
    NoteOn { layer: LayerId, note: NoteId, midi: f64, velocity: f32, pan: f32, duration: Option<f64> },
    Drum { layer: LayerId, drum: Drum, velocity: f32, pan: f32, tone: f32 },
}

#[derive(Debug, Clone, Copy)]
struct Active {
    layer: LayerId,
}

/// Owns note identities and the two time-ordered queues: deferred triggers
/// (humanize / grid quantize) and planned releases.
#[derive(Debug, Default)]
pub struct NoteBook {
    next_id: u64,
    seq: u64,
    active: BTreeMap<NoteId, Active>,
    active_count: Vec<u16>,
    offs: BinaryHeap<Reverse<(When, u64)>>,
    pending: Vec<(When, Trigger)>,
}

impl NoteBook {
    pub fn new(layers: usize) -> Self {
        Self { active_count: vec![0; layers], ..Default::default() }
    }

    fn alloc(&mut self) -> NoteId {
        self.next_id += 1;
        NoteId(self.next_id)
    }

    fn seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Sounding notes on a layer.
    pub fn active_count(&self, layer: LayerId) -> u16 {
        self.active_count.get(layer.0 as usize).copied().unwrap_or(0)
    }

    fn start(&mut self, now: Seconds, trig: Trigger, out: &mut Vec<SynthCommand>, notes: &mut Vec<NoteSnapshot>) {
        match trig {
            Trigger::NoteOn { layer, note, midi, velocity, pan, duration } => {
                self.active.insert(note, Active { layer });
                if let Some(c) = self.active_count.get_mut(layer.0 as usize) {
                    *c = c.saturating_add(1);
                }
                if let Some(d) = duration {
                    let seq = self.seq();
                    self.offs.push(Reverse((When::new(now + d.max(0.0), seq), note.0)));
                }
                out.push(SynthCommand::NoteOn { layer, note, pitch_hz: midi_to_hz(midi) as f32, velocity, pan });
                notes.push(NoteSnapshot { at: now, layer, midi: midi as f32, velocity, pan, duration, active: true });
            }
            Trigger::Drum { layer, drum, velocity, pan, tone } => {
                out.push(SynthCommand::Drum { layer, drum, velocity, pan, tone });
            }
        }
    }

    /// Release a sounding note now. Pending (not yet started) notes are
    /// cancelled silently. Unknown ids are ignored.
    pub fn release(&mut self, note: NoteId, out: &mut Vec<SynthCommand>) {
        if let Some(a) = self.active.remove(&note) {
            if let Some(c) = self.active_count.get_mut(a.layer.0 as usize) {
                *c = c.saturating_sub(1);
            }
            out.push(SynthCommand::NoteOff { layer: a.layer, note });
            return;
        }
        self.pending.retain(|(_, t)| !matches!(t, Trigger::NoteOn { note: n, .. } if *n == note));
    }

    /// Forget everything on a layer (the caller sends `AllNotesOff`).
    pub fn silence_layer(&mut self, layer: LayerId) {
        self.active.retain(|_, a| a.layer != layer);
        if let Some(c) = self.active_count.get_mut(layer.0 as usize) {
            *c = 0;
        }
        self.pending.retain(|(_, t)| match t {
            Trigger::NoteOn { layer: l, .. } | Trigger::Drum { layer: l, .. } => *l != layer,
        });
    }

    /// Forget everything (pause / panic).
    pub fn silence_all(&mut self) {
        self.active.clear();
        self.active_count.iter_mut().for_each(|c| *c = 0);
        self.pending.clear();
        self.offs.clear();
    }

    /// Is this note sounding or about to?
    pub fn is_live(&self, note: NoteId) -> bool {
        self.active.contains_key(&note)
            || self.pending.iter().any(|(_, t)| matches!(t, Trigger::NoteOn { note: n, .. } if *n == note))
    }

    /// Emit everything due at `now`: deferred triggers first, then releases.
    pub fn flush(&mut self, now: Seconds, out: &mut Vec<SynthCommand>, notes: &mut Vec<NoteSnapshot>) {
        if !self.pending.is_empty() {
            self.pending.sort_by_key(|(w, _)| *w);
            let due = self.pending.iter().take_while(|(w, _)| w.time() <= now + 1e-9).count();
            let fire: Vec<(When, Trigger)> = self.pending.drain(..due).collect();
            for (w, t) in fire {
                self.start(w.time().min(now), t, out, notes);
            }
        }
        while let Some(Reverse((w, id))) = self.offs.peek().copied() {
            if w.time() > now + 1e-9 {
                break;
            }
            self.offs.pop();
            let note = NoteId(id);
            if self.active.contains_key(&note) {
                self.release(note, out);
            }
        }
    }
}

/// Per-layer cache so we only send a parameter when it moves audibly.
#[derive(Debug, Default, Clone)]
pub struct ParamCache {
    last: BTreeMap<&'static str, f32>,
}

impl ParamCache {
    fn key(p: &LayerParam) -> &'static str {
        match p {
            LayerParam::Gain(_) => "gain",
            LayerParam::Pan(_) => "pan",
            LayerParam::CutoffOctaves(_) => "cutoff",
            LayerParam::Resonance(_) => "resonance",
            LayerParam::ReverbSend(_) => "reverb",
            LayerParam::DelaySend(_) => "delay",
            LayerParam::Texture(_) => "texture",
            LayerParam::Mute(_) => "mute",
        }
    }
    fn value(p: &LayerParam) -> f32 {
        match p {
            LayerParam::Gain(v)
            | LayerParam::Pan(v)
            | LayerParam::CutoffOctaves(v)
            | LayerParam::Resonance(v)
            | LayerParam::ReverbSend(v)
            | LayerParam::DelaySend(v)
            | LayerParam::Texture(v) => *v,
            LayerParam::Mute(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Send `param` for `layer` if it changed by more than 0.005 (or is new).
    pub fn send(&mut self, layer: LayerId, param: LayerParam, out: &mut Vec<SynthCommand>) {
        let v = Self::value(&param);
        if !v.is_finite() {
            return;
        }
        let k = Self::key(&param);
        if let Some(prev) = self.last.get(k) {
            if (prev - v).abs() <= 0.005 {
                return;
            }
        }
        self.last.insert(k, v);
        out.push(SynthCommand::SetLayerParam { layer, param });
    }
}

/// How a generator emits sound: notes with planned durations, pitch glides,
/// releases, drums and layer parameters. Applies humanize jitter and the
/// per-tick trigger budget.
pub struct NoteSink<'a> {
    book: &'a mut NoteBook,
    params: &'a mut ParamCache,
    out: &'a mut Vec<SynthCommand>,
    notes: &'a mut Vec<NoteSnapshot>,
    jitter: &'a mut ChaCha8Rng,
    layer: LayerId,
    now: Seconds,
    humanize: f64,
    budget: usize,
}

impl<'a> NoteSink<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        book: &'a mut NoteBook,
        params: &'a mut ParamCache,
        out: &'a mut Vec<SynthCommand>,
        notes: &'a mut Vec<NoteSnapshot>,
        jitter: &'a mut ChaCha8Rng,
        layer: LayerId,
        now: Seconds,
        humanize: f64,
    ) -> Self {
        Self { book, params, out, notes, jitter, layer, now, humanize, budget: TRIGGERS_PER_TICK }
    }

    /// Humanized (delay, velocity): timing only ever drifts late.
    fn humanize(&mut self, delay: f64, velocity: f64) -> (f64, f32) {
        if self.humanize <= 0.0 {
            return (delay.max(0.0), velocity.clamp(0.0, 1.0) as f32);
        }
        let tj = (self.jitter.random::<f64>() * 2.0 - 1.0) * JITTER_S * self.humanize;
        let vj = (self.jitter.random::<f64>() * 2.0 - 1.0) * VEL_JITTER * self.humanize;
        ((delay + tj).max(0.0), (velocity * (1.0 + vj)).clamp(0.0, 1.0) as f32)
    }

    fn schedule(&mut self, delay: f64, trig: Trigger) {
        if delay <= 1e-6 {
            self.book.start(self.now, trig, self.out, self.notes);
        } else {
            let seq = self.book.seq();
            self.book.pending.push((When::new(self.now + delay, seq), trig));
        }
    }

    /// Start a note at `midi` (may be fractional) after `delay` seconds, for
    /// `duration` seconds (None = until [`NoteSink::release`]). Returns None
    /// when this tick's trigger budget is spent.
    pub fn play(&mut self, midi: f64, velocity: f64, pan: f64, duration: Option<f64>, delay: f64) -> Option<NoteId> {
        if self.budget == 0 || !midi.is_finite() {
            return None;
        }
        self.budget -= 1;
        let (delay, velocity) = self.humanize(delay, velocity);
        let note = self.book.alloc();
        let pan = if pan.is_finite() { pan.clamp(0.0, 1.0) as f32 } else { 0.5 };
        self.schedule(delay, Trigger::NoteOn { layer: self.layer, note, midi, velocity, pan, duration });
        Some(note)
    }

    /// Trigger a drum hit.
    pub fn drum(&mut self, drum: Drum, velocity: f64, pan: f64, tone: f64, delay: f64) {
        if self.budget == 0 {
            return;
        }
        self.budget -= 1;
        let (delay, velocity) = self.humanize(delay, velocity);
        let pan = if pan.is_finite() { pan.clamp(0.0, 1.0) as f32 } else { 0.5 };
        let tone = if tone.is_finite() { tone.clamp(0.0, 1.0) as f32 } else { 0.5 };
        self.schedule(delay, Trigger::Drum { layer: self.layer, drum, velocity, pan, tone });
    }

    /// Release a held note now.
    pub fn release(&mut self, note: NoteId) {
        self.book.release(note, self.out);
    }

    /// Glide a sounding note to a new pitch.
    pub fn glide(&mut self, note: NoteId, midi: f64) {
        if self.book.active.contains_key(&note) && midi.is_finite() {
            self.out.push(SynthCommand::NotePitch { layer: self.layer, note, pitch_hz: midi_to_hz(midi) as f32 });
        }
    }

    /// Is the note still sounding (or pending)?
    pub fn is_live(&self, note: NoteId) -> bool {
        self.book.is_live(note)
    }

    /// Send a layer parameter (deduplicated).
    pub fn param(&mut self, param: LayerParam) {
        self.params.send(self.layer, param, self.out);
    }

    pub fn now(&self) -> Seconds {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn sink_env() -> (NoteBook, ParamCache, Vec<SynthCommand>, Vec<NoteSnapshot>, ChaCha8Rng) {
        (NoteBook::new(2), ParamCache::default(), Vec::new(), Vec::new(), ChaCha8Rng::seed_from_u64(1))
    }

    #[test]
    fn note_lifecycle_with_planned_release() {
        let (mut book, mut pc, mut out, mut notes, mut rng) = sink_env();
        let id = {
            let mut s = NoteSink::new(&mut book, &mut pc, &mut out, &mut notes, &mut rng, LayerId(1), 0.0, 0.0);
            s.play(60.0, 0.8, 0.5, Some(0.5), 0.0).unwrap()
        };
        assert!(matches!(out[0], SynthCommand::NoteOn { note, .. } if note == id));
        assert_eq!(book.active_count(LayerId(1)), 1);
        assert_eq!(notes.len(), 1);
        book.flush(0.3, &mut out, &mut notes);
        assert_eq!(out.len(), 1);
        book.flush(0.6, &mut out, &mut notes);
        assert!(matches!(out[1], SynthCommand::NoteOff { note, .. } if note == id));
        assert_eq!(book.active_count(LayerId(1)), 0);
        // Releasing again is a no-op.
        book.release(id, &mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn deferred_trigger_fires_later_and_cancels_cleanly() {
        let (mut book, mut pc, mut out, mut notes, mut rng) = sink_env();
        let (a, b) = {
            let mut s = NoteSink::new(&mut book, &mut pc, &mut out, &mut notes, &mut rng, LayerId(0), 1.0, 0.0);
            (s.play(60.0, 0.8, 0.5, None, 0.1).unwrap(), s.play(62.0, 0.8, 0.5, None, 0.2).unwrap())
        };
        assert!(out.is_empty());
        book.release(b, &mut out); // cancelled before it started: no NoteOff
        assert!(out.is_empty());
        book.flush(1.05, &mut out, &mut notes);
        assert!(out.is_empty());
        book.flush(1.15, &mut out, &mut notes);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], SynthCommand::NoteOn { note, .. } if note == a));
        book.flush(2.0, &mut out, &mut notes);
        assert_eq!(out.len(), 1, "b was cancelled and must never sound");
    }

    #[test]
    fn budget_and_param_dedupe() {
        let (mut book, mut pc, mut out, mut notes, mut rng) = sink_env();
        let mut s = NoteSink::new(&mut book, &mut pc, &mut out, &mut notes, &mut rng, LayerId(0), 0.0, 0.0);
        let played = (0..100).filter(|_| s.play(60.0, 0.5, 0.5, Some(1.0), 0.0).is_some()).count();
        assert_eq!(played, TRIGGERS_PER_TICK);
        s.param(LayerParam::Gain(0.5));
        s.param(LayerParam::Gain(0.502));
        s.param(LayerParam::Gain(0.6));
        let sends = out.iter().filter(|c| matches!(c, SynthCommand::SetLayerParam { .. })).count();
        assert_eq!(sends, 2);
    }

    #[test]
    fn humanize_only_delays_and_is_deterministic() {
        let run = || {
            let (mut book, mut pc, mut out, mut notes, mut rng) = sink_env();
            {
                let mut s = NoteSink::new(&mut book, &mut pc, &mut out, &mut notes, &mut rng, LayerId(0), 0.0, 1.0);
                for _ in 0..10 {
                    s.play(60.0, 0.5, 0.5, Some(1.0), 0.0);
                }
            }
            book.flush(0.02, &mut out, &mut notes);
            notes.iter().map(|n| (n.at, n.velocity)).collect::<Vec<_>>()
        };
        let a = run();
        assert_eq!(a, run());
        assert_eq!(a.len(), 10);
        assert!(a.iter().all(|(t, v)| *t >= 0.0 && *t <= 0.02 && (*v - 0.5).abs() <= 0.051));
    }
}
