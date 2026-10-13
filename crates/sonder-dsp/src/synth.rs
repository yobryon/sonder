//! The polyphonic synth: voice pool, per-layer slots and mixer, drum voices,
//! effect buses and master chain. Everything is preallocated in
//! [`Synth::new`]; [`Synth::handle`] and [`Synth::render`] never allocate,
//! lock or panic on bad input.

use sonder_core::{Effects, Instrument, LayerId, LayerParam, Meter, NoteId, SynthCommand, SynthFeedback};

use crate::delay::StereoDelay;
use crate::drums::DrumVoice;
use crate::master::MasterChain;
use crate::math::{onepole_coef, pan_gains, sane};
use crate::reverb::Plate;
use crate::voice::{LayerMod, NoteStart, Patch, Voice};

/// Internal processing block size in frames.
pub const BLOCK: usize = 64;
/// Size of the voice pool.
pub const MAX_VOICES: usize = 64;
/// Size of the percussion voice pool.
pub const DRUM_VOICES: usize = 16;
const FEEDBACK_CAPACITY: usize = 1024;
const RECYCLE_CAPACITY: usize = 512;
const EFFECTS_RECYCLE_CAPACITY: usize = 64;
/// Meter ballistics: per-block decay of the displayed value (~300 ms fall at 48 kHz).
const METER_DECAY_SECONDS: f32 = 0.3;
/// Mute fade time.
const MUTE_FADE_SECONDS: f32 = 0.010;
/// Panic fade time.
const PANIC_FADE_SECONDS: f32 = 0.05;

/// A control-rate value with one-pole smoothing evaluated once per block.
#[derive(Debug, Clone, Copy)]
struct Smoothed {
    cur: f32,
    target: f32,
}

impl Smoothed {
    fn new(v: f32) -> Self {
        Self { cur: v, target: v }
    }
    #[inline]
    fn step(&mut self, coef: f32) -> f32 {
        self.cur += coef * (self.target - self.cur);
        self.cur
    }
}

#[derive(Debug)]
struct Layer {
    instrument: Option<Box<Instrument>>,
    patch: Patch,
    gain: Smoothed,
    pan: Smoothed,
    cutoff_oct: Smoothed,
    resonance: Option<f32>,
    reverb_send: Smoothed,
    reverb_override: Option<f32>,
    delay_send: Smoothed,
    delay_override: Option<f32>,
    texture: Smoothed,
    muted: bool,
    /// 1 = audible, 0 = silent; ramps over [`MUTE_FADE_SECONDS`].
    mute_gain: f32,
    /// Previous block's final stereo gains, for ramping.
    prev_gl: f32,
    prev_gr: f32,
    meter: Meter,
    lfo_phase: f32,
    last_midi: Option<f32>,
}

impl Layer {
    fn new() -> Self {
        let patch = Patch::default();
        Self {
            instrument: None,
            gain: Smoothed::new(1.0),
            pan: Smoothed::new(0.5),
            cutoff_oct: Smoothed::new(0.0),
            resonance: None,
            reverb_send: Smoothed::new(patch.reverb_send),
            reverb_override: None,
            delay_send: Smoothed::new(patch.delay_send),
            delay_override: None,
            texture: Smoothed::new(0.0),
            muted: false,
            mute_gain: 1.0,
            prev_gl: 0.0,
            prev_gr: 0.0,
            meter: Meter::default(),
            lfo_phase: 0.0,
            last_midi: None,
            patch,
        }
    }
}

/// Fixed-capacity FIFO of boxes awaiting the control thread.
#[derive(Debug)]
struct RecycleRing<T> {
    slots: Vec<Option<Box<T>>>,
    head: usize,
    len: usize,
    /// Boxes leaked because the ring was full (never freed on the audio thread).
    leaked: u32,
}

impl<T> RecycleRing<T> {
    fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        Self { slots, head: 0, len: 0, leaked: 0 }
    }
    fn push(&mut self, b: Box<T>) {
        if self.len >= self.slots.len() {
            // Last resort: leaking never frees memory, so it is realtime-safe.
            self.leaked = self.leaked.saturating_add(1);
            let _ = Box::leak(b);
            return;
        }
        let idx = (self.head + self.len) % self.slots.len();
        self.slots[idx] = Some(b);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<Box<T>> {
        if self.len == 0 {
            return None;
        }
        let b = self.slots[self.head].take();
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        b
    }
}

/// Fixed-capacity feedback queue (audio → control).
#[derive(Debug)]
struct FeedbackQueue {
    buf: Vec<SynthFeedback>,
    head: usize,
    len: usize,
    dropped: u32,
}

impl FeedbackQueue {
    fn new(capacity: usize) -> Self {
        Self { buf: vec![SynthFeedback::Underruns(0); capacity], head: 0, len: 0, dropped: 0 }
    }
    fn push(&mut self, f: SynthFeedback) {
        if self.len >= self.buf.len() {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        let idx = (self.head + self.len) % self.buf.len();
        self.buf[idx] = f;
        self.len += 1;
    }
    fn pop(&mut self) -> Option<SynthFeedback> {
        if self.len == 0 {
            return None;
        }
        let f = self.buf[self.head].clone();
        self.head = (self.head + 1) % self.buf.len();
        self.len -= 1;
        Some(f)
    }
}

/// The synthesizer. See the module docs.
#[derive(Debug)]
pub struct Synth {
    sr: f32,
    layers: Vec<Layer>,
    voices: Vec<Voice>,
    drums: Vec<DrumVoice>,
    /// Per-layer scratch bus: `max_layers × BLOCK × 2`.
    layer_bus: Vec<f32>,
    dry: Vec<f32>,
    reverb_bus: Vec<f32>,
    delay_bus: Vec<f32>,
    fx_out: Vec<f32>,
    /// Leftover frames from the last rendered block (for arbitrary output lengths).
    scratch: Vec<f32>,
    scratch_pos: usize,
    reverb: Plate,
    delay: StereoDelay,
    master: MasterChain,
    reverb_level: f32,
    delay_level: f32,
    effects: Option<Box<Effects>>,
    master_meter: Meter,
    feedback: FeedbackQueue,
    recycle: RecycleRing<Instrument>,
    recycle_fx: RecycleRing<Effects>,
    age: u64,
    seed: u32,
    smooth_coef: f32,
    meter_decay: f32,
    mute_step: f32,
    panic_remaining: u32,
    panic_total: u32,
}

impl Synth {
    /// Create a synth for `sample_rate` Hz with `max_layers` layer slots.
    /// Preallocates every buffer the audio thread will ever touch.
    pub fn new(sample_rate: f32, max_layers: usize) -> Self {
        let sr = if sample_rate.is_finite() { sample_rate.clamp(8000.0, 384_000.0) } else { 48_000.0 };
        let max_layers = max_layers.clamp(1, 256);
        let mut voices = Vec::with_capacity(MAX_VOICES);
        voices.resize_with(MAX_VOICES, Voice::default);
        let mut drums = Vec::with_capacity(DRUM_VOICES);
        drums.resize_with(DRUM_VOICES, DrumVoice::default);
        let mut layers = Vec::with_capacity(max_layers);
        layers.resize_with(max_layers, Layer::new);
        let block_rate = sr / BLOCK as f32;
        Self {
            sr,
            layers,
            voices,
            drums,
            layer_bus: vec![0.0; max_layers * BLOCK * 2],
            dry: vec![0.0; BLOCK * 2],
            reverb_bus: vec![0.0; BLOCK * 2],
            delay_bus: vec![0.0; BLOCK * 2],
            fx_out: vec![0.0; BLOCK * 2],
            scratch: vec![0.0; BLOCK * 2],
            scratch_pos: BLOCK * 2,
            reverb: Plate::new(sr),
            delay: StereoDelay::new(sr),
            master: MasterChain::new(sr),
            reverb_level: 0.8,
            delay_level: 0.7,
            effects: None,
            master_meter: Meter::default(),
            feedback: FeedbackQueue::new(FEEDBACK_CAPACITY),
            recycle: RecycleRing::new(RECYCLE_CAPACITY),
            recycle_fx: RecycleRing::new(EFFECTS_RECYCLE_CAPACITY),
            age: 0,
            seed: 0x2545_F491,
            smooth_coef: onepole_coef(0.02, block_rate),
            meter_decay: (-1.0 / (METER_DECAY_SECONDS * block_rate)).exp(),
            mute_step: 1.0 / (MUTE_FADE_SECONDS * sr),
            panic_remaining: 0,
            panic_total: (PANIC_FADE_SECONDS * sr) as u32,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    pub fn max_layers(&self) -> usize {
        self.layers.len()
    }

    /// Number of currently sounding synth voices (drums excluded).
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }

    /// Number of currently sounding drum voices.
    pub fn active_drums(&self) -> usize {
        self.drums.iter().filter(|d| d.active).count()
    }

    pub fn master_meter(&self) -> Meter {
        self.master_meter.clone()
    }

    pub fn layer_meter(&self, layer: LayerId) -> Meter {
        self.layers.get(layer.0 as usize).map(|l| l.meter.clone()).unwrap_or_default()
    }

    /// Pop every queued feedback message.
    pub fn drain_feedback(&mut self, f: &mut dyn FnMut(SynthFeedback)) {
        while let Some(fb) = self.feedback.pop() {
            f(fb);
        }
    }

    /// Instrument boxes the control thread should drop.
    pub fn take_recycled(&mut self) -> Option<Box<Instrument>> {
        self.recycle.pop()
    }

    /// Effects boxes the control thread should drop.
    pub fn take_recycled_effects(&mut self) -> Option<Box<Effects>> {
        self.recycle_fx.pop()
    }

    /// Boxes leaked because the recycle rings overflowed (diagnostic).
    pub fn leaked_boxes(&self) -> u32 {
        self.recycle.leaked + self.recycle_fx.leaked
    }

    /// Feedback messages dropped because the queue overflowed (diagnostic).
    pub fn dropped_feedback(&self) -> u32 {
        self.feedback.dropped
    }

    fn next_seed(&mut self) -> u32 {
        self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.seed
    }

    /// Apply one command. Realtime-safe.
    pub fn handle(&mut self, cmd: SynthCommand) {
        match cmd {
            SynthCommand::NoteOn { layer, note, pitch_hz, velocity, pan } => {
                self.note_on(layer, note, pitch_hz, velocity, pan);
            }
            SynthCommand::NoteOff { layer, note } => {
                for v in self.voices.iter_mut() {
                    if v.active && v.layer == layer && v.note == note && !v.is_released() {
                        v.release();
                    }
                }
            }
            SynthCommand::NotePitch { layer, note, pitch_hz } => {
                let hz = sane(pitch_hz, 1.0, 20_000.0);
                let mut found = false;
                for v in self.voices.iter_mut() {
                    if v.active && v.layer == layer && v.note == note {
                        v.set_pitch(hz);
                        found = true;
                    }
                }
                if found {
                    if let Some(l) = self.layers.get_mut(layer.0 as usize) {
                        l.last_midi = Some(crate::math::hz_to_midi(hz));
                    }
                }
            }
            SynthCommand::Drum { layer, drum, velocity, pan, tone } => {
                if (layer.0 as usize) >= self.layers.len() {
                    return;
                }
                self.age += 1;
                let age = self.age;
                let seed = self.next_seed();
                let sr = self.sr;
                let slot = match self.drums.iter().position(|d| !d.active) {
                    Some(i) => i,
                    None => {
                        let mut best = 0;
                        for (i, d) in self.drums.iter().enumerate() {
                            if d.age < self.drums[best].age {
                                best = i;
                            }
                        }
                        best
                    }
                };
                self.drums[slot].trigger(
                    sr,
                    layer,
                    drum,
                    sane(velocity, 0.0, 1.0),
                    sane(pan, 0.0, 1.0),
                    sane(tone, 0.0, 1.0),
                    age,
                    seed,
                );
            }
            SynthCommand::AllNotesOff { layer } => {
                for v in self.voices.iter_mut() {
                    if v.active && v.layer == layer {
                        v.release();
                    }
                }
            }
            SynthCommand::SetInstrument { layer, instrument } => {
                match self.layers.get_mut(layer.0 as usize) {
                    Some(l) => {
                        l.patch = Patch::from_instrument(&instrument);
                        if let Some(old) = l.instrument.replace(instrument) {
                            self.recycle.push(old);
                        }
                    }
                    None => self.recycle.push(instrument),
                }
            }
            SynthCommand::SetLayerParam { layer, param } => {
                if let Some(l) = self.layers.get_mut(layer.0 as usize) {
                    match param {
                        LayerParam::Gain(g) => l.gain.target = sane(g, 0.0, 4.0),
                        LayerParam::Pan(p) => l.pan.target = sane(p, 0.0, 1.0),
                        LayerParam::CutoffOctaves(o) => l.cutoff_oct.target = sane(o, -8.0, 8.0),
                        LayerParam::Resonance(r) => l.resonance = Some(sane(r, 0.0, 1.0)),
                        LayerParam::ReverbSend(s) => l.reverb_override = Some(sane(s, 0.0, 1.0)),
                        LayerParam::DelaySend(s) => l.delay_override = Some(sane(s, 0.0, 1.0)),
                        LayerParam::Texture(t) => l.texture.target = sane(t, 0.0, 1.0),
                        LayerParam::Mute(m) => l.muted = m,
                    }
                }
            }
            SynthCommand::SetTempo { bpm } => self.delay.set_tempo(sane(bpm, 20.0, 400.0)),
            SynthCommand::SetEffects(fx) => {
                self.reverb.configure(&fx.reverb);
                self.delay.configure(&fx.delay);
                self.master.configure(&fx.master);
                self.reverb_level = sane(fx.reverb.level, 0.0, 2.0);
                self.delay_level = sane(fx.delay.level, 0.0, 2.0);
                if let Some(old) = self.effects.replace(fx) {
                    self.recycle_fx.push(old);
                }
            }
            SynthCommand::Panic => {
                for v in self.voices.iter_mut() {
                    if v.active {
                        v.kill(PANIC_FADE_SECONDS * 0.5);
                    }
                }
                self.panic_remaining = self.panic_total.max(1);
            }
        }
    }

    fn note_on(&mut self, layer: LayerId, note: NoteId, pitch_hz: f32, velocity: f32, pan: f32) {
        let li = layer.0 as usize;
        if li >= self.layers.len() {
            return;
        }
        let pitch_hz = sane(pitch_hz, 1.0, 20_000.0);
        let polyphony = self.layers[li].patch.polyphony;

        // Pick a voice: enforce per-instrument polyphony first, then the global pool.
        let on_layer = self.voices.iter().filter(|v| v.active && v.layer == layer).count();
        let slot = if on_layer >= polyphony {
            Self::steal_candidate(&self.voices, Some(layer))
        } else if let Some(i) = self.voices.iter().position(|v| !v.active) {
            i
        } else {
            Self::steal_candidate(&self.voices, None)
        };
        if self.voices[slot].active {
            let v = &self.voices[slot];
            self.feedback.push(SynthFeedback::NoteStolen { layer: v.layer, note: v.note });
        }

        self.age += 1;
        let age = self.age;
        let seed = self.next_seed();
        let sr = self.sr;
        let l = &mut self.layers[li];
        let glide_from = l.last_midi;
        let start = NoteStart {
            layer,
            note,
            pitch_hz,
            velocity: sane(velocity, 0.0, 1.0),
            pan: sane(pan, 0.0, 1.0),
            patch: &l.patch,
            glide_from_midi: glide_from,
            lfo_phase: l.lfo_phase,
            age,
            seed,
        };
        self.voices[slot].start(sr, start);
        l.last_midi = Some(crate::math::hz_to_midi(pitch_hz));
    }

    /// Prefer released voices with the lowest level, then the oldest.
    fn steal_candidate(voices: &[Voice], layer: Option<LayerId>) -> usize {
        let mut best: Option<usize> = None;
        let mut best_key = (false, f32::MAX, u64::MAX);
        for (i, v) in voices.iter().enumerate() {
            if !v.active {
                continue;
            }
            if let Some(l) = layer {
                if v.layer != l {
                    continue;
                }
            }
            // Sort key: released first (true < false → invert), then lowest level, then oldest.
            let key = (!v.is_released(), if v.is_released() { v.level() } else { f32::MAX }, v.age);
            if best.is_none() || key < best_key {
                best = Some(i);
                best_key = key;
            }
        }
        best.unwrap_or(0)
    }

    /// Render interleaved stereo into `out` (any length; odd trailing sample is zeroed).
    pub fn render(&mut self, out: &mut [f32]) {
        let mut pos = 0;
        let n = out.len() / 2 * 2;
        while pos < n {
            if self.scratch_pos >= BLOCK * 2 {
                self.render_block();
                self.scratch_pos = 0;
            }
            let avail = BLOCK * 2 - self.scratch_pos;
            let take = avail.min(n - pos);
            out[pos..pos + take].copy_from_slice(&self.scratch[self.scratch_pos..self.scratch_pos + take]);
            self.scratch_pos += take;
            pos += take;
        }
        if out.len() > n {
            out[n] = 0.0;
        }
    }

    fn render_block(&mut self) {
        let n_layers = self.layers.len();
        let frames2 = BLOCK * 2;
        self.layer_bus.fill(0.0);
        self.dry.fill(0.0);
        self.reverb_bus.fill(0.0);
        self.delay_bus.fill(0.0);

        // Advance free-running layer LFOs.
        let dt_block = BLOCK as f32 / self.sr;
        for l in &mut self.layers {
            l.lfo_phase += l.patch.lfo.rate_hz.clamp(0.0, 50.0) * dt_block;
            if l.lfo_phase >= 1.0 {
                l.lfo_phase -= l.lfo_phase.floor();
            }
        }

        // Smooth layer params and build per-layer modulation.
        let coef = self.smooth_coef;
        for l in &mut self.layers {
            l.gain.step(coef);
            l.pan.step(coef);
            l.cutoff_oct.step(coef);
            l.texture.step(coef);
            l.reverb_send.target = l.reverb_override.unwrap_or(l.patch.reverb_send);
            l.delay_send.target = l.delay_override.unwrap_or(l.patch.delay_send);
            l.reverb_send.step(coef);
            l.delay_send.step(coef);
        }

        // Voices → layer buses.
        for v in self.voices.iter_mut() {
            if !v.active {
                continue;
            }
            let li = v.layer.0 as usize;
            if li >= n_layers {
                v.active = false;
                continue;
            }
            let l = &self.layers[li];
            let m = LayerMod { cutoff_octaves: l.cutoff_oct.cur, resonance: l.resonance, texture: l.texture.cur };
            let bus = &mut self.layer_bus[li * frames2..(li + 1) * frames2];
            if !v.render(bus, &m) {
                self.feedback.push(SynthFeedback::NoteEnded { layer: v.layer, note: v.note });
            }
        }
        // Drums → layer buses.
        for d in self.drums.iter_mut() {
            if !d.active {
                continue;
            }
            let li = d.layer.0 as usize;
            if li >= n_layers {
                d.active = false;
                continue;
            }
            let bus = &mut self.layer_bus[li * frames2..(li + 1) * frames2];
            d.render(bus);
        }

        // Mixer: layer bus → dry + sends, with ramped gain/pan/mute and metering.
        let inv = 1.0 / BLOCK as f32;
        let mute_step_block = self.mute_step * BLOCK as f32;
        for (li, l) in self.layers.iter_mut().enumerate() {
            let bus = &self.layer_bus[li * frames2..(li + 1) * frames2];
            let mute_start = l.mute_gain;
            let mute_target = if l.muted { 0.0 } else { 1.0 };
            let mute_end = if mute_target > mute_start {
                (mute_start + mute_step_block).min(1.0)
            } else {
                (mute_start - mute_step_block).max(0.0)
            };
            l.mute_gain = mute_end;
            let (bl, br) = pan_gains(l.pan.cur);
            let bal = std::f32::consts::SQRT_2;
            let gl_end = l.gain.cur * bl * bal * mute_end;
            let gr_end = l.gain.cur * br * bal * mute_end;
            let gl_start = l.prev_gl;
            let gr_start = l.prev_gr;
            l.prev_gl = gl_end;
            l.prev_gr = gr_end;
            let silent = gl_start == 0.0 && gl_end == 0.0 && gr_start == 0.0 && gr_end == 0.0;
            let rs = l.reverb_send.cur;
            let ds = l.delay_send.cur;
            let mut peak = 0f32;
            let mut sq = 0f32;
            if !silent {
                for i in 0..BLOCK {
                    let t = i as f32 * inv;
                    let gl = gl_start + (gl_end - gl_start) * t;
                    let gr = gr_start + (gr_end - gr_start) * t;
                    let sl = bus[i * 2] * gl;
                    let sr_ = bus[i * 2 + 1] * gr;
                    self.dry[i * 2] += sl;
                    self.dry[i * 2 + 1] += sr_;
                    self.reverb_bus[i * 2] += sl * rs;
                    self.reverb_bus[i * 2 + 1] += sr_ * rs;
                    self.delay_bus[i * 2] += sl * ds;
                    self.delay_bus[i * 2 + 1] += sr_ * ds;
                    peak = peak.max(sl.abs()).max(sr_.abs());
                    sq += sl * sl + sr_ * sr_;
                }
            }
            let rms = (sq * inv * 0.5).sqrt();
            l.meter.peak = peak.max(l.meter.peak * self.meter_decay);
            l.meter.rms = rms.max(l.meter.rms * self.meter_decay);
            if l.meter.peak < 1e-6 {
                l.meter.peak = 0.0;
            }
            if l.meter.rms < 1e-6 {
                l.meter.rms = 0.0;
            }
        }

        // Effects.
        self.delay.process(&self.delay_bus, &mut self.fx_out);
        let dl = self.delay_level;
        for i in 0..frames2 {
            self.dry[i] += self.fx_out[i] * dl;
        }
        self.reverb.process(&self.reverb_bus, &mut self.fx_out);
        let rl = self.reverb_level;
        for i in 0..frames2 {
            self.dry[i] += self.fx_out[i] * rl;
        }

        // Panic fade.
        if self.panic_remaining > 0 {
            let total = self.panic_total.max(1) as f32;
            for i in 0..BLOCK {
                let g = (self.panic_remaining as f32 / total).clamp(0.0, 1.0);
                self.dry[i * 2] *= g;
                self.dry[i * 2 + 1] *= g;
                self.panic_remaining = self.panic_remaining.saturating_sub(1);
            }
            if self.panic_remaining == 0 {
                self.reverb.clear();
                self.delay.clear();
                for v in self.voices.iter_mut() {
                    if v.active {
                        v.active = false;
                        self.feedback.push(SynthFeedback::NoteEnded { layer: v.layer, note: v.note });
                    }
                }
                for d in self.drums.iter_mut() {
                    d.active = false;
                }
            }
        }

        // Master.
        self.master.process(&mut self.dry);
        let mut peak = 0f32;
        let mut sq = 0f32;
        for &s in self.dry.iter() {
            peak = peak.max(s.abs());
            sq += s * s;
        }
        let rms = (sq / frames2 as f32).sqrt();
        self.master_meter.peak = peak.max(self.master_meter.peak * self.meter_decay);
        self.master_meter.rms = rms.max(self.master_meter.rms * self.meter_decay);
        if self.master_meter.peak < 1e-6 {
            self.master_meter.peak = 0.0;
            self.master_meter.rms = 0.0;
        }
        self.scratch.copy_from_slice(&self.dry);
    }
}
