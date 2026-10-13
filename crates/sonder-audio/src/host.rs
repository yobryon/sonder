//! The control↔audio bridge: lock-free rings for commands, feedback, recycled
//! boxes and tapped audio, plus atomics for meters and counters.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use sonder_core::{Effects, Instrument, LayerId, Meter, SynthCommand, SynthFeedback};
use sonder_dsp::Synth;

/// Capacity of the command ring (commands, not bytes).
pub const COMMAND_CAPACITY: usize = 8192;
const FEEDBACK_CAPACITY: usize = 4096;
const RECYCLE_CAPACITY: usize = 1024;
/// Commands handled per `render` call at most, to bound audio-thread time.
const MAX_COMMANDS_PER_RENDER: usize = 4096;

/// Configuration of the audio tap ring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapConfig {
    /// Ring capacity in seconds of stereo audio.
    pub capacity_seconds: f32,
}

impl Default for TapConfig {
    fn default() -> Self {
        Self { capacity_seconds: 2.0 }
    }
}

#[derive(Debug)]
struct Shared {
    sample_rate: f32,
    /// `[rms, peak]` bit-cast f32.
    master: [AtomicU32; 2],
    layers: Vec<[AtomicU32; 2]>,
    underruns: AtomicU32,
    drops: AtomicU64,
    feedback_drops: AtomicU64,
    tap_drops: AtomicU64,
    active_voices: AtomicU32,
    blocks_rendered: AtomicU64,
}

#[inline]
fn store_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}
#[inline]
fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// Control-side consumer of tapped audio (for the web stream and the analyzer).
pub struct AudioTap {
    consumer: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AudioTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioTap").field("available", &self.available()).finish()
    }
}

impl AudioTap {
    /// Interleaved channel count of tapped audio.
    pub const CHANNELS: usize = 2;

    pub fn sample_rate(&self) -> f32 {
        self.shared.sample_rate
    }

    /// Samples (not frames) currently waiting in the ring.
    pub fn available(&self) -> usize {
        self.consumer.slots()
    }

    /// Append every available whole frame to `out`. Returns the number of samples appended.
    pub fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize {
        let n = self.consumer.slots() / 2 * 2;
        if n == 0 {
            return 0;
        }
        let start = out.len();
        out.resize(start + n, 0.0);
        let (_, rest) = self.consumer.pop_partial_slice(&mut out[start..]);
        let got = n - rest.len();
        out.truncate(start + got);
        got
    }

    /// Fill `out` with as many samples as available. Returns the count written.
    pub fn pop_into(&mut self, out: &mut [f32]) -> usize {
        let n = (self.consumer.slots().min(out.len())) / 2 * 2;
        let (_, rest) = self.consumer.pop_partial_slice(&mut out[..n]);
        n - rest.len()
    }

    /// Frames the audio thread discarded because nobody drained the tap in time.
    pub fn dropped_frames(&self) -> u64 {
        self.shared.tap_drops.load(Ordering::Relaxed)
    }
}

/// Control-thread handle to the synth.
pub struct SynthController {
    commands: rtrb::Producer<SynthCommand>,
    feedback: rtrb::Consumer<SynthFeedback>,
    recycled: rtrb::Consumer<Box<Instrument>>,
    recycled_fx: rtrb::Consumer<Box<Effects>>,
    tap: Option<AudioTap>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SynthController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynthController").field("sample_rate", &self.shared.sample_rate).field("drops", &self.drops()).finish()
    }
}

impl SynthController {
    /// Queue a command for the audio thread. Never blocks; returns `false` (and
    /// counts a drop) if the ring is full. Also frees any recycled boxes.
    pub fn send(&mut self, cmd: SynthCommand) -> bool {
        self.collect_garbage();
        match self.commands.push(cmd) {
            Ok(()) => true,
            Err(_) => {
                self.shared.drops.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Drop every recycled instrument/effects box waiting from the audio thread.
    pub fn collect_garbage(&mut self) {
        while self.recycled.pop().is_ok() {}
        while self.recycled_fx.pop().is_ok() {}
    }

    /// Free slots in the command ring.
    pub fn capacity_left(&self) -> usize {
        self.commands.slots()
    }

    pub fn sample_rate(&self) -> f32 {
        self.shared.sample_rate
    }

    /// Commands dropped because the ring was full.
    pub fn drops(&self) -> u64 {
        self.shared.drops.load(Ordering::Relaxed)
    }

    /// Buffer underruns reported by the output backend.
    pub fn underruns(&self) -> u32 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Feedback messages lost because the control side was slow to poll.
    pub fn feedback_drops(&self) -> u64 {
        self.shared.feedback_drops.load(Ordering::Relaxed)
    }

    pub fn master_meter(&self) -> Meter {
        Meter { rms: load_f32(&self.shared.master[0]), peak: load_f32(&self.shared.master[1]) }
    }

    pub fn layer_meter(&self, layer: LayerId) -> Meter {
        match self.shared.layers.get(layer.0 as usize) {
            Some(m) => Meter { rms: load_f32(&m[0]), peak: load_f32(&m[1]) },
            None => Meter::default(),
        }
    }

    /// Meters for every layer slot, in order.
    pub fn layer_meters(&self) -> Vec<Meter> {
        (0..self.shared.layers.len() as u16).map(|i| self.layer_meter(LayerId(i))).collect()
    }

    pub fn max_layers(&self) -> usize {
        self.shared.layers.len()
    }

    pub fn active_voices(&self) -> usize {
        self.shared.active_voices.load(Ordering::Relaxed) as usize
    }

    /// Blocks rendered so far by the host (a liveness indicator).
    pub fn blocks_rendered(&self) -> u64 {
        self.shared.blocks_rendered.load(Ordering::Relaxed)
    }

    /// Pop one feedback message, if any.
    pub fn poll_feedback(&mut self) -> Option<SynthFeedback> {
        self.feedback.pop().ok()
    }

    /// Pop every pending feedback message.
    pub fn drain_feedback(&mut self, mut f: impl FnMut(SynthFeedback)) {
        while let Ok(fb) = self.feedback.pop() {
            f(fb);
        }
    }

    /// Take back an instrument box the synth no longer needs (to reuse or drop).
    pub fn take_recycled(&mut self) -> Option<Box<Instrument>> {
        self.recycled.pop().ok()
    }

    /// Take back an effects box the synth no longer needs.
    pub fn take_recycled_effects(&mut self) -> Option<Box<Effects>> {
        self.recycled_fx.pop().ok()
    }

    /// Take the audio tap (once). `None` if no tap was configured or it was already taken.
    pub fn take_tap(&mut self) -> Option<AudioTap> {
        self.tap.take()
    }

    /// Is the audio side still alive?
    pub fn is_connected(&self) -> bool {
        !self.commands.is_abandoned()
    }
}

/// Audio-thread side: owns the [`Synth`] and the ring endpoints.
pub struct SynthHost {
    synth: Synth,
    commands: rtrb::Consumer<SynthCommand>,
    feedback: rtrb::Producer<SynthFeedback>,
    recycled: rtrb::Producer<Box<Instrument>>,
    recycled_fx: rtrb::Producer<Box<Effects>>,
    pending_recycle: Option<Box<Instrument>>,
    pending_recycle_fx: Option<Box<Effects>>,
    tap: Option<rtrb::Producer<f32>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SynthHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SynthHost").field("sample_rate", &self.shared.sample_rate).finish()
    }
}

impl SynthHost {
    pub fn sample_rate(&self) -> f32 {
        self.shared.sample_rate
    }

    pub fn synth(&self) -> &Synth {
        &self.synth
    }

    pub fn synth_mut(&mut self) -> &mut Synth {
        &mut self.synth
    }

    /// Record a backend-reported underrun.
    pub fn report_underrun(&self) {
        self.shared.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Clone of the shared underrun counter for backends that detect them in an
    /// error callback (no `&self` available there).
    pub(crate) fn underrun_counter(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Pop pending commands, render one buffer of interleaved stereo, publish
    /// meters/feedback/recycled boxes and push audio to the tap. Realtime-safe.
    pub fn render(&mut self, out: &mut [f32]) {
        let mut handled = 0;
        while handled < MAX_COMMANDS_PER_RENDER {
            match self.commands.pop() {
                Ok(cmd) => {
                    self.synth.handle(cmd);
                    handled += 1;
                }
                Err(_) => break,
            }
        }
        self.synth.render(out);

        let m = self.synth.master_meter();
        store_f32(&self.shared.master[0], m.rms);
        store_f32(&self.shared.master[1], m.peak);
        for (i, slot) in self.shared.layers.iter().enumerate() {
            let lm = self.synth.layer_meter(LayerId(i as u16));
            store_f32(&slot[0], lm.rms);
            store_f32(&slot[1], lm.peak);
        }
        self.shared.active_voices.store(self.synth.active_voices() as u32, Ordering::Relaxed);
        self.shared.blocks_rendered.fetch_add(1, Ordering::Relaxed);

        if let Some(tap) = &mut self.tap {
            let frames_in = out.len() / 2;
            let room = tap.slots() / 2;
            let take = frames_in.min(room);
            if take > 0 {
                let _ = tap.push_entire_slice(&out[..take * 2]);
            }
            if take < frames_in {
                self.shared.tap_drops.fetch_add((frames_in - take) as u64, Ordering::Relaxed);
            }
        }

        let fb = &mut self.feedback;
        let shared = &self.shared;
        self.synth.drain_feedback(&mut |f| {
            if fb.push(f).is_err() {
                shared.feedback_drops.fetch_add(1, Ordering::Relaxed);
            }
        });

        // Recycled boxes: move to the control side; if the ring is full, park one
        // and retry next time (never drop here).
        if self.pending_recycle.is_none() {
            self.pending_recycle = self.synth.take_recycled();
        }
        while let Some(b) = self.pending_recycle.take() {
            match self.recycled.push(b) {
                Ok(()) => self.pending_recycle = self.synth.take_recycled(),
                Err(rtrb::PushError::Full(b)) => {
                    self.pending_recycle = Some(b);
                    break;
                }
            }
        }
        if self.pending_recycle_fx.is_none() {
            self.pending_recycle_fx = self.synth.take_recycled_effects();
        }
        while let Some(b) = self.pending_recycle_fx.take() {
            match self.recycled_fx.push(b) {
                Ok(()) => self.pending_recycle_fx = self.synth.take_recycled_effects(),
                Err(rtrb::PushError::Full(b)) => {
                    self.pending_recycle_fx = Some(b);
                    break;
                }
            }
        }
    }
}

impl Shared {
    pub(crate) fn add_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build a connected controller/host pair for `sample_rate` Hz and `max_layers`
/// layer slots. With `tap`, rendered audio is also copied into an [`AudioTap`]
/// obtainable via [`SynthController::take_tap`].
pub fn synth_pair(sample_rate: f32, max_layers: usize, tap: Option<TapConfig>) -> (SynthController, SynthHost) {
    let synth = Synth::new(sample_rate, max_layers);
    let sample_rate = synth.sample_rate();
    let max_layers = synth.max_layers();
    let shared = Arc::new(Shared {
        sample_rate,
        master: [AtomicU32::new(0), AtomicU32::new(0)],
        layers: (0..max_layers).map(|_| [AtomicU32::new(0), AtomicU32::new(0)]).collect(),
        underruns: AtomicU32::new(0),
        drops: AtomicU64::new(0),
        feedback_drops: AtomicU64::new(0),
        tap_drops: AtomicU64::new(0),
        active_voices: AtomicU32::new(0),
        blocks_rendered: AtomicU64::new(0),
    });
    let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(COMMAND_CAPACITY);
    let (fb_tx, fb_rx) = rtrb::RingBuffer::new(FEEDBACK_CAPACITY);
    let (rc_tx, rc_rx) = rtrb::RingBuffer::new(RECYCLE_CAPACITY);
    let (fx_tx, fx_rx) = rtrb::RingBuffer::new(RECYCLE_CAPACITY / 4);
    let (tap_tx, tap_rx) = match tap {
        Some(cfg) => {
            let secs = if cfg.capacity_seconds.is_finite() { cfg.capacity_seconds.clamp(0.05, 60.0) } else { 2.0 };
            let samples = ((secs * sample_rate) as usize).max(256) * 2;
            let (p, c) = rtrb::RingBuffer::new(samples);
            (Some(p), Some(c))
        }
        None => (None, None),
    };
    let controller = SynthController {
        commands: cmd_tx,
        feedback: fb_rx,
        recycled: rc_rx,
        recycled_fx: fx_rx,
        tap: tap_rx.map(|consumer| AudioTap { consumer, shared: shared.clone() }),
        shared: shared.clone(),
    };
    let host = SynthHost {
        synth,
        commands: cmd_rx,
        feedback: fb_tx,
        recycled: rc_tx,
        recycled_fx: fx_tx,
        pending_recycle: None,
        pending_recycle_fx: None,
        tap: tap_tx,
        shared,
    };
    (controller, host)
}

pub(crate) type SharedState = Arc<Shared>;
