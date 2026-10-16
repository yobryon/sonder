//! The control loop: one tick = drain samples → run features → compose → push synth
//! commands → (every few ticks) publish a snapshot. Shared by live and offline modes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

#[cfg(feature = "native")]
use anyhow::Context;
use sonder_audio::{AudioTap, SynthController};
use sonder_compose::{Composer, ComposerInput, LayerInput};
use sonder_core::*;
use sonder_dsp::Analyzer;
use sonder_expr::LayerPipeline;

use crate::captions::Captioner;
use crate::replay::Replay;
use crate::{SNAPSHOT_EVERY_TICKS, TICK_SECONDS};

/// Observer of every ingested sample (data-log recording).
pub type SampleSink = Box<dyn FnMut(&Sample) + Send>;

/// Bounded history lengths for the snapshot.
const MAX_EVENTS: usize = 64;
const MAX_NOTES: usize = 256;

/// Per-source bookkeeping for the snapshot.
struct SourceStatus {
    kind: String,
    state: SourceState,
    message: String,
    readings: BTreeMap<String, f64>,
    last_sample_at: Option<Seconds>,
    /// Timestamps of recent samples for the rate display.
    recent: VecDeque<Seconds>,
}

/// Everything the control loop owns.
pub struct ControlLoop {
    scene: Scene,
    seed: u64,
    composer: Composer,
    pipelines: Vec<Option<LayerPipeline>>,
    /// Source each layer reads from (None = free-running generator).
    layer_source: Vec<Option<SourceId>>,
    /// Source → layers that read it.
    readers: HashMap<SourceId, Vec<usize>>,
    sources: BTreeMap<SourceId, SourceStatus>,
    instrument_names: Vec<String>,
    synth: SynthController,
    analyzer: Analyzer,
    tap: Option<AudioTap>,
    tap_buf: Vec<f32>,
    web_audio: Option<tokio::sync::broadcast::Sender<Arc<Vec<f32>>>>,
    events: VecDeque<EventSnapshot>,
    notes: VecDeque<NoteSnapshot>,
    viz: BTreeMap<String, serde_json::Value>,
    pending_events: Vec<LayerEvent>,
    layer_inputs: Vec<LayerInput>,
    last_now: Option<Seconds>,
    tick_count: u32,
    underruns: u32,
    audio_backend: String,
    snapshot_tx: tokio::sync::watch::Sender<Arc<EngineSnapshot>>,
    quit_requested: bool,
    save_requests: Vec<String>,
    sinks: Vec<Box<dyn crate::sink::CommandSink>>,
    /// Scratch for [`CommandSink::drain_requests`].
    sink_requests: Vec<crate::sink::SinkRequest>,
    mappings: HashMap<SourceId, crate::mapping::SourceMapping>,
    captioner: Captioner,
    /// `LoadScene` TOML waiting for the live runner (which owns the sources).
    load_request: Option<String>,
    /// Recycled audio-chunk boxes (allocation and all), reused for `Datum::Audio` routing.
    #[allow(clippy::vec_box)]
    audio_pool: Vec<Box<AudioChunk>>,
    sample_sinks: Vec<SampleSink>,
    /// The log being played, if any (time travel). While set, the scene's own sources
    /// are not run; the log feeds whatever source names the scene has.
    replay: Option<Replay>,
    /// A `Seek` waiting for the next tick (needs `now`).
    pending_seek: Option<f64>,
    /// What the DJ is doing, for the snapshot (None: no DJ running).
    dj: Option<DjSnapshot>,
    /// `SetDj` toggles waiting for the live runner (which owns the DJ).
    dj_requests: Vec<bool>,
    /// Shared-room state (chat ring, listeners) when hosting or joined; see `set_room`.
    room: Option<RoomSnapshot>,
    /// Chat lines said here, waiting for the room publisher.
    outbound_chat: Vec<ChatMessage>,
    /// `/moment` requests waiting for the live runner.
    moment_requests: u32,
}

/// Chat lines kept in [`RoomSnapshot::chat`].
pub const ROOM_CHAT_RING: usize = 32;
/// Importance of a chat caption.
pub const CHAT_CAPTION_IMPORTANCE: f64 = 0.6;
/// Chat text that asks the host to save the last minutes as a data log.
pub const MOMENT_COMMAND: &str = "/moment";

/// The per-scene state `new` and `rebuild` both construct before touching the synth.
struct Parts {
    composer: Composer,
    pipelines: Vec<Option<LayerPipeline>>,
    layer_source: Vec<Option<SourceId>>,
    readers: HashMap<SourceId, Vec<usize>>,
    sources: BTreeMap<SourceId, SourceStatus>,
    mappings: HashMap<SourceId, crate::mapping::SourceMapping>,
    captioner: Captioner,
    instruments: Vec<(Instrument, String)>,
}

impl Parts {
    fn build(scene: &Scene, seed: u64) -> anyhow::Result<Self> {
        let composer = Composer::new(scene, seed).map_err(|e| anyhow::anyhow!("composer: {e}"))?;
        let mut pipelines = Vec::with_capacity(scene.layers.len());
        let mut layer_source = Vec::with_capacity(scene.layers.len());
        let mut readers: HashMap<SourceId, Vec<usize>> = HashMap::new();
        let mut sources = BTreeMap::new();
        let mut instruments = Vec::with_capacity(scene.layers.len());
        let mut mappings: HashMap<SourceId, crate::mapping::SourceMapping> = HashMap::new();
        let mut expanded: HashMap<SourceId, SourceSpec> = HashMap::new();

        for (i, layer) in scene.layers.iter().enumerate() {
            let pipeline = LayerPipeline::compile(&layer.signals, &layer.map, seed ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15))
                .map_err(|e| anyhow::anyhow!("layer `{}`: {e}", layer.name))?;
            pipelines.push(Some(pipeline));
            let owner = scene.source_owner(layer);
            let sid = owner.map(|o| SourceId::new(&o.name));
            if let (Some(sid), Some(owner)) = (&sid, owner) {
                readers.entry(sid.clone()).or_default().push(i);
                if let LayerSource::Spec(spec) = &owner.source {
                    if !mappings.contains_key(sid) {
                        let ex = crate::recipes::expand(spec).map_err(|e| anyhow::anyhow!("source `{sid}`: {e}"))?;
                        if let Some(m) =
                            crate::mapping::SourceMapping::from_spec(&ex).map_err(|e| anyhow::anyhow!("source `{sid}`: {e}"))?
                        {
                            mappings.insert(sid.clone(), m);
                        }
                        expanded.insert(sid.clone(), ex);
                    }
                    sources.entry(sid.clone()).or_insert_with(|| SourceStatus {
                        kind: spec.kind.clone(),
                        state: SourceState::Connecting,
                        message: String::new(),
                        readings: BTreeMap::new(),
                        last_sample_at: None,
                        recent: VecDeque::new(),
                    });
                }
            }
            layer_source.push(sid);
            instruments.push(crate::instruments::resolve(scene, layer));
        }
        let captioner = Captioner::new(scene, |i| layer_source[i].as_ref().and_then(|sid| expanded.get(sid).cloned()));
        Ok(Self { composer, pipelines, layer_source, readers, sources, mappings, captioner, instruments })
    }

    /// Push instruments, effects and tempo to the synth.
    fn send_setup(&self, scene: &Scene, synth: &mut SynthController) -> Vec<String> {
        let mut names = Vec::with_capacity(self.instruments.len());
        for (i, (inst, name)) in self.instruments.iter().enumerate() {
            synth.send(SynthCommand::SetInstrument { layer: LayerId(i as u16), instrument: Box::new(inst.clone()) });
            names.push(name.clone());
        }
        synth.send(SynthCommand::SetEffects(Box::new(scene.effects.clone())));
        synth.send(SynthCommand::SetTempo { bpm: scene.conductor.tempo as f32 });
        names
    }
}

impl ControlLoop {
    /// Build the loop. Sends initial instrument/effects/tempo commands to the synth.
    pub fn new(
        scene: Scene,
        seed: u64,
        mut synth: SynthController,
        tap: Option<AudioTap>,
        web_audio: Option<tokio::sync::broadcast::Sender<Arc<Vec<f32>>>>,
        audio_backend: String,
        snapshot_tx: tokio::sync::watch::Sender<Arc<EngineSnapshot>>,
    ) -> anyhow::Result<Self> {
        let parts = Parts::build(&scene, seed)?;
        let instrument_names = parts.send_setup(&scene, &mut synth);
        let analyzer = Analyzer::new(synth.sample_rate(), 48);
        let n = scene.layers.len();
        let replay = Self::replay_from_scene(&scene)?;
        let mut sources = parts.sources;
        if replay.is_some() {
            mark_replayed(&mut sources);
        }
        Ok(Self {
            scene,
            seed,
            composer: parts.composer,
            pipelines: parts.pipelines,
            layer_source: parts.layer_source,
            readers: parts.readers,
            sources,
            instrument_names,
            synth,
            analyzer,
            tap,
            tap_buf: Vec::with_capacity(8192),
            web_audio,
            events: VecDeque::with_capacity(MAX_EVENTS),
            notes: VecDeque::with_capacity(MAX_NOTES),
            viz: BTreeMap::new(),
            pending_events: Vec::new(),
            layer_inputs: (0..n).map(|_| LayerInput { params: BTreeMap::new(), signals: BTreeMap::new() }).collect(),
            last_now: None,
            tick_count: 0,
            underruns: 0,
            audio_backend,
            snapshot_tx,
            quit_requested: false,
            save_requests: Vec::new(),
            sinks: Vec::new(),
            sink_requests: Vec::new(),
            mappings: parts.mappings,
            captioner: parts.captioner,
            load_request: None,
            audio_pool: Vec::new(),
            sample_sinks: Vec::new(),
            replay,
            pending_seek: None,
            dj: None,
            dj_requests: Vec::new(),
            room: None,
            outbound_chat: Vec::new(),
            moment_requests: 0,
        })
    }

    /// A `replay.log` source declared in the scene, loaded from its `path` (native only;
    /// in the browser the bytes arrive through `set_replay`).
    fn replay_from_scene(scene: &Scene) -> anyhow::Result<Option<Replay>> {
        let Some(layer) = scene.layers.iter().find(|l| matches!(&l.source, LayerSource::Spec(s) if s.kind == crate::replay::KIND)) else {
            return Ok(None);
        };
        let LayerSource::Spec(spec) = &layer.source else { return Ok(None) };
        let cfg = crate::replay::ReplayConfig::from_spec(spec).map_err(|e| anyhow::anyhow!("source `{}`: {e}", layer.name))?;
        #[cfg(feature = "native")]
        if let Some(path) = spec.get_str("path") {
            let log = crate::datalog::DataLog::open(std::path::Path::new(path)).map_err(|e| anyhow::anyhow!("source `{}`: {e}", layer.name))?;
            let title = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
            return Ok(Some(Replay::new(Arc::new(log), title, cfg)));
        }
        let _ = cfg;
        Ok(None)
    }

    /// Play a data log instead of live sources (or stop playing one with `None`). The
    /// caller stops/starts the scene's own sources accordingly. Playback starts (or
    /// resumes) from the replay's `from` on the next tick, after a warm pass.
    pub fn set_replay(&mut self, replay: Option<Replay>) {
        let from = replay.as_ref().map(|r| r.config().from);
        self.replay = replay;
        if let Some(from) = from {
            mark_replayed(&mut self.sources);
            self.pending_seek = Some(from);
        }
    }

    pub fn replay(&self) -> Option<&Replay> {
        self.replay.as_ref()
    }

    /// Attach an observer of every ingested sample (the data-log recorder).
    pub fn add_sample_sink(&mut self, sink: SampleSink) {
        self.sample_sinks.push(sink);
    }

    /// Scene source names the replay's log actually carries (after `rename`).
    pub fn replay_fed_sources(&self) -> Vec<String> {
        let Some(r) = &self.replay else { return Vec::new() };
        r.log().source_names().into_iter().map(|n| r.rename(&SourceId::new(&n)).to_string()).filter(|n| self.sources.contains_key(&SourceId::new(n))).collect()
    }

    /// Replace the running scene in place, keeping the synth, the tap, the snapshot
    /// channel and the command sinks. Every old layer gets `AllNotesOff`; instruments,
    /// effects and tempo are re-sent. Fails (leaving the old scene running) when the new
    /// scene does not build or needs more layers than the synth has slots for. The caller
    /// must stop the old sources and start `source_specs(&scene)` itself.
    pub fn rebuild(&mut self, scene: Scene, seed: u64) -> anyhow::Result<()> {
        if let Err(errs) = crate::validate(&scene) {
            anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
        }
        let max = self.synth.max_layers();
        if scene.layers.len() > max {
            anyhow::bail!("scene `{}` has {} layers but this engine was started with {max} slots", scene.name, scene.layers.len());
        }
        let parts = Parts::build(&scene, seed)?;
        let declared = Self::replay_from_scene(&scene)?;
        let caption = format!("scene: {}", scene.name);
        self.install(scene, seed, parts, caption);
        if declared.is_some() {
            self.set_replay(declared);
        } else if let Some(r) = self.replay.as_mut() {
            // Keep playing the same log into the new scene, from where we were.
            let pos = r.position();
            mark_replayed(&mut self.sources);
            self.pending_seek = Some(pos);
        }
        Ok(())
    }

    /// Silence every layer and swap in freshly built per-scene state (shared by scene
    /// reloads and seeks).
    fn install(&mut self, scene: Scene, seed: u64, parts: Parts, caption: String) {
        for i in 0..self.scene.layers.len() {
            let cmd = SynthCommand::AllNotesOff { layer: LayerId(i as u16) };
            for sink in &mut self.sinks {
                sink.on_command(&cmd);
            }
            self.synth.send(cmd);
        }
        for sink in &mut self.sinks {
            sink.flush();
        }
        self.instrument_names = parts.send_setup(&scene, &mut self.synth);
        let n = scene.layers.len();
        let mut captioner = parts.captioner;
        captioner.inherit(&self.captioner);
        captioner.push(self.last_now.unwrap_or(0.0), None, caption, 0.7);
        self.scene = scene;
        self.seed = seed;
        self.composer = parts.composer;
        self.pipelines = parts.pipelines;
        self.layer_source = parts.layer_source;
        self.readers = parts.readers;
        self.sources = parts.sources;
        self.mappings = parts.mappings;
        self.captioner = captioner;
        self.events.clear();
        self.notes.clear();
        self.viz.clear();
        self.pending_events.clear();
        self.layer_inputs = (0..n).map(|_| LayerInput { params: BTreeMap::new(), signals: BTreeMap::new() }).collect();
    }

    /// Jump the replay to `to` log seconds: rebuild the musical state with the same seed
    /// (all notes off), move the log cursor and warm the features on the preceding
    /// [`crate::replay::WARM_SECONDS`] of samples — ticking the pipelines, rendering nothing.
    fn do_seek(&mut self, to: f64, now: Seconds) {
        let Some(replay) = self.replay.as_mut() else { return };
        let to = to.clamp(0.0, replay.length());
        let parts = match Parts::build(&self.scene, self.seed) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("seek: rebuilding scene state failed: {e:#}");
                return;
            }
        };
        let scene = self.scene.clone();
        let seed = self.seed;
        let caption = format!("⟲ {}", mmss(to));
        self.install(scene, seed, parts, caption);
        mark_replayed(&mut self.sources);
        let replay = self.replay.as_mut().expect("replay");
        replay.seek(to);
        let speed = if replay.speed() > 0.0 { replay.speed() } else { 1.0 };
        let entries: Vec<(Seconds, SourceId, Datum)> = replay.warm_entries(to).iter().map(|e| (e.t, replay.rename(&e.source), e.datum.clone())).collect();
        // Warm pass: step log time in ticks up to `to`, stamping samples in engine time
        // as if they had arrived before `now`.
        let start = (to - crate::replay::WARM_SECONDS).max(0.0).max(to - now * speed);
        let transport = self.composer.transport();
        let mut i = 0;
        let mut tau = start;
        while tau < to {
            let next = (tau + TICK_SECONDS * speed).min(to);
            let warm_now = now - (to - next) / speed;
            while i < entries.len() && entries[i].0 < next {
                let (_, source, datum) = &entries[i];
                self.warm_feed(Sample { source: source.clone(), at: warm_now, datum: datum.clone() }, transport.beats);
                i += 1;
            }
            for p in self.pipelines.iter_mut().flatten() {
                let _ = p.tick(warm_now, TICK_SECONDS, transport.beats, transport.bar as f64, transport.bpm);
            }
            tau = next;
        }
        self.pending_events.clear();
    }

    /// Feed a sample to the features only (no events, captions or synth): the warm pass.
    fn warm_feed(&mut self, sample: Sample, beat: f64) {
        let samples: Vec<Sample> = match self.mappings.get(&sample.source) {
            Some(m) if matches!(sample.datum, Datum::Event { .. }) => m.apply(sample, beat),
            _ => vec![sample],
        };
        for s in samples {
            let Some(readers) = self.readers.get(&s.source) else { continue };
            match &s.datum {
                Datum::Reading { values } => {
                    if let Some(st) = self.sources.get_mut(&s.source) {
                        st.readings.extend(values.iter().map(|(k, v)| (k.clone(), *v)));
                    }
                    for &i in readers {
                        if let Some(p) = self.pipelines[i].as_mut() {
                            p.push_reading(values);
                        }
                    }
                }
                Datum::Event { name, intensity, attrs } => {
                    for &i in readers {
                        if let Some(p) = self.pipelines[i].as_mut() {
                            p.push_event(name, *intensity, attrs, s.at);
                        }
                    }
                }
                Datum::Viz { payload } => {
                    self.viz.insert(s.source.to_string(), payload.clone());
                }
                Datum::Status { .. } | Datum::Audio { .. } => {}
            }
        }
    }

    /// The scene currently running (as loaded, without runtime tweaks).
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// TOML handed to `LoadScene` since the last call, if any (newest wins).
    pub fn take_load_request(&mut self) -> Option<String> {
        self.load_request.take()
    }

    /// `SetDj { enabled }` values received since the last call, in order.
    pub fn take_dj_requests(&mut self) -> Vec<bool> {
        std::mem::take(&mut self.dj_requests)
    }

    /// Publish the DJ's state in the snapshot (the live runner updates it every tick).
    pub fn set_dj(&mut self, dj: Option<DjSnapshot>) {
        self.dj = dj;
    }

    /// Add a caption at the current engine time (DJ switches, narration).
    pub fn push_caption(&mut self, text: impl Into<String>, importance: f64) {
        self.captioner.push(self.last_now.unwrap_or(0.0), None, text, importance);
    }

    /// The names of the sources this scene needs started, with their specs — recipes
    /// expanded to their transport specs, so sources/adapters never see recipe kinds.
    pub fn source_specs(scene: &Scene) -> Vec<(SourceId, SourceSpec)> {
        scene
            .layers
            .iter()
            .filter_map(|l| match &l.source {
                LayerSource::Spec(spec) if spec.kind == crate::replay::KIND => None,
                LayerSource::Spec(spec) => {
                    Some((SourceId::new(&l.name), crate::recipes::expand(spec).unwrap_or_else(|_| spec.clone())))
                }
                _ => None,
            })
            .collect()
    }

    /// Attach an observer of the command stream (e.g. MIDI out). Sinks see every
    /// command the composer emits from now on (setup commands already went to the synth).
    pub fn add_sink(&mut self, sink: Box<dyn crate::sink::CommandSink>) {
        self.sinks.push(sink);
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// Scene with runtime tweaks (mutes) folded in, for `SaveScene`.
    pub fn current_scene(&self) -> Scene {
        let mut s = self.scene.clone();
        for (i, l) in s.layers.iter_mut().enumerate() {
            l.muted = self.composer.layer_muted(LayerId(i as u16));
        }
        s
    }

    /// Paths requested by `SaveScene` commands since last call.
    pub fn take_save_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.save_requests)
    }

    /// Current seed (the one `rebuild` last applied).
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Join (or host) a room: `EngineSnapshot.room` is published from now on. `role` is
    /// "host" or "guest".
    pub fn set_room(&mut self, id: &str, url: &str, role: &str) {
        self.room = Some(RoomSnapshot { id: id.into(), role: role.into(), url: url.into(), ..Default::default() });
    }

    /// Leave the room (`EngineSnapshot.room` becomes `None`).
    pub fn clear_room(&mut self) {
        self.room = None;
    }

    pub fn room(&self) -> Option<&RoomSnapshot> {
        self.room.as_ref()
    }

    /// Listener count reported by the relay.
    pub fn set_room_listeners(&mut self, listeners: u32) {
        if let Some(r) = self.room.as_mut() {
            r.listeners = listeners;
        }
    }

    /// Estimated clock offset to the host (guests).
    pub fn set_room_offset(&mut self, offset: f64) {
        if let Some(r) = self.room.as_mut() {
            r.offset = offset;
        }
    }

    /// A chat line arrived (from the relay or this UI): it joins the room's chat ring and
    /// becomes a caption (`from: text`). `/moment` also asks the live runner to save the
    /// recent data log. Returns false for blank lines, which are ignored.
    pub fn chat_received(&mut self, mut msg: ChatMessage) -> bool {
        msg.text = msg.text.trim().to_string();
        if msg.text.is_empty() {
            return false;
        }
        if msg.from.trim().is_empty() {
            msg.from = "anon".into();
        }
        if !msg.at.is_finite() {
            msg.at = self.last_now.unwrap_or(0.0);
        }
        if msg.text == MOMENT_COMMAND {
            self.moment_requests += 1;
        }
        self.captioner.push(msg.at, None, format!("{}: {}", msg.from, msg.text), CHAT_CAPTION_IMPORTANCE);
        if let Some(r) = self.room.as_mut() {
            r.chat.push(msg);
            if r.chat.len() > ROOM_CHAT_RING {
                let excess = r.chat.len() - ROOM_CHAT_RING;
                r.chat.drain(..excess);
            }
        }
        true
    }

    /// Chat lines said through `UserCommand::Chat` since the last call (for the publisher).
    pub fn take_outbound_chat(&mut self) -> Vec<ChatMessage> {
        std::mem::take(&mut self.outbound_chat)
    }

    /// Was a `/moment` requested since the last call?
    pub fn take_moment_request(&mut self) -> bool {
        std::mem::take(&mut self.moment_requests) > 0
    }

    /// Feed one sample from a source. Sources with a mapping (recipes, `event = {..}`)
    /// have their events reshaped first.
    pub fn ingest(&mut self, sample: Sample) {
        for sink in &mut self.sample_sinks {
            sink(&sample);
        }
        if let Some(m) = self.mappings.get(&sample.source) {
            if matches!(sample.datum, Datum::Event { .. }) {
                let beat = self.composer.transport().beats;
                for s in m.apply(sample, beat) {
                    self.ingest_raw(s);
                }
                return;
            }
        }
        self.ingest_raw(sample);
    }

    fn ingest_raw(&mut self, sample: Sample) {
        let now = sample.at;
        if let Some(st) = self.sources.get_mut(&sample.source) {
            st.last_sample_at = Some(now);
            st.recent.push_back(now);
            while st.recent.front().is_some_and(|t| now - *t > 1.0) {
                st.recent.pop_front();
            }
        }
        let readers = match self.readers.get(&sample.source) {
            Some(r) => r.clone(),
            None => Vec::new(),
        };
        match sample.datum {
            Datum::Reading { values } => {
                if let Some(st) = self.sources.get_mut(&sample.source) {
                    for (k, v) in &values {
                        st.readings.insert(k.clone(), *v);
                    }
                    if st.state == SourceState::Connecting {
                        st.state = SourceState::Live;
                    }
                }
                for &i in &readers {
                    if let Some(p) = self.pipelines[i].as_mut() {
                        p.push_reading(&values);
                    }
                }
            }
            Datum::Event { name, intensity, attrs } => {
                if let Some(st) = self.sources.get_mut(&sample.source) {
                    if st.state == SourceState::Connecting {
                        st.state = SourceState::Live;
                    }
                }
                for &i in &readers {
                    if let Some(p) = self.pipelines[i].as_mut() {
                        p.push_event(&name, intensity, &attrs, now);
                    }
                    self.pending_events.push(LayerEvent {
                        layer: LayerId(i as u16),
                        name: name.clone(),
                        at: now,
                        intensity,
                        attrs: attrs.clone(),
                    });
                }
                if let Some(&first) = readers.first() {
                    let layer = LayerId(first as u16);
                    let label = self.captioner.label(layer, &name, intensity, &attrs);
                    if let Some(importance) = self.captioner.salience(layer, &name, intensity, now) {
                        let text = if label.is_empty() { format!("{name} on {}", self.scene.layers[first].name) } else { label.clone() };
                        self.captioner.push(now, Some(layer), text, importance);
                    }
                    push_bounded(
                        &mut self.events,
                        EventSnapshot { at: now, layer, name, intensity, attrs: trim_attrs(attrs), label },
                        MAX_EVENTS,
                    );
                }
            }
            Datum::Status { state, message } => {
                if let Some(st) = self.sources.get_mut(&sample.source) {
                    let was = st.state;
                    st.state = state;
                    // A replayed status is history, not news: keep the replay marker in front.
                    st.message = if self.replay.is_some() { format!("replay · {message}") } else { message.clone() };
                    if was != state {
                        let name = sample.source.as_str();
                        let layer = readers.first().map(|&i| LayerId(i as u16));
                        match state {
                            SourceState::Live => self.captioner.push(now, layer, format!("{name} is live"), 0.4),
                            SourceState::Failed => self.captioner.push(now, layer, format!("{name} failed: {message}"), 0.9),
                            SourceState::Reconnecting => self.captioner.push(now, layer, format!("{name} reconnecting"), 0.5),
                            _ => {}
                        }
                    }
                }
            }
            Datum::Viz { payload } => {
                self.viz.insert(sample.source.to_string(), payload);
            }
            Datum::Audio { rate, samples } => {
                if let Some(st) = self.sources.get_mut(&sample.source) {
                    if st.state == SourceState::Connecting {
                        st.state = SourceState::Live;
                    }
                }
                // One box per reading layer; the synth copies it into the layer's ring and
                // hands the box back (see `recycle_audio`).
                for &i in &readers {
                    let mut chunk = self.audio_pool.pop().unwrap_or_default();
                    chunk.rate = rate;
                    chunk.samples.clear();
                    chunk.samples.extend_from_slice(&samples);
                    self.synth.send(SynthCommand::AudioChunk { layer: LayerId(i as u16), chunk });
                }
            }
        }
    }

    /// Reclaim audio-chunk boxes the synth has finished with.
    fn recycle_audio(&mut self) {
        while let Some(chunk) = self.synth.take_recycled_audio() {
            if self.audio_pool.len() < 64 {
                self.audio_pool.push(chunk);
            }
        }
    }

    /// Apply a UI command.
    pub fn command(&mut self, cmd: UserCommand) {
        for sink in &mut self.sinks {
            sink.on_user_command(&cmd);
        }
        match &cmd {
            UserCommand::Quit => self.quit_requested = true,
            UserCommand::SaveScene { path } => self.save_requests.push(path.clone()),
            UserCommand::LoadScene { toml } => self.load_request = Some(toml.clone()),
            UserCommand::Seek { to } => {
                if self.replay.is_some() {
                    self.pending_seek = Some(*to);
                }
            }
            UserCommand::SetDj { enabled } => self.dj_requests.push(*enabled),
            UserCommand::Chat { from, text } => {
                let msg = ChatMessage { at: self.last_now.unwrap_or(0.0), from: from.clone(), text: text.clone() };
                if self.chat_received(msg.clone()) {
                    self.outbound_chat.push(msg);
                }
            }
            UserCommand::SetSpeed { speed } => {
                if let Some(r) = self.replay.as_mut() {
                    r.set_speed(*speed);
                }
            }
            _ => self.composer.handle(&cmd),
        }
    }

    /// One control tick at engine time `now`.
    pub fn tick(&mut self, now: Seconds) {
        let dt = match self.last_now {
            Some(prev) => (now - prev).clamp(0.0, 1.0),
            None => TICK_SECONDS,
        };
        self.last_now = Some(now);
        self.tick_count = self.tick_count.wrapping_add(1);

        // Sinks that steer the engine (Ableton Link following a peer's tempo).
        self.sink_requests.clear();
        for sink in &mut self.sinks {
            sink.drain_requests(&mut self.sink_requests);
        }
        for req in std::mem::take(&mut self.sink_requests) {
            match req {
                crate::sink::SinkRequest::Command(cmd) => self.command(cmd),
                crate::sink::SinkRequest::SetBarPhase(phase) => self.composer.set_bar_phase(phase),
            }
        }

        // Time travel: pending seeks first, then whatever the log has for this tick.
        if let Some(to) = self.pending_seek.take() {
            self.do_seek(to, now);
        }
        if let Some(replay) = self.replay.as_mut() {
            let due = replay.advance_to(now);
            for s in due {
                self.ingest(s);
            }
        }

        // Features.
        let transport = self.composer.transport();
        for (i, pipeline) in self.pipelines.iter_mut().enumerate() {
            let Some(p) = pipeline.as_mut() else { continue };
            let fired = p.tick(now, dt, transport.beats, transport.bar as f64, transport.bpm);
            for f in fired {
                self.pending_events.push(LayerEvent {
                    layer: LayerId(i as u16),
                    name: f.name.clone(),
                    at: now,
                    intensity: f.intensity,
                    attrs: BTreeMap::new(),
                });
                let label = self.captioner.label(LayerId(i as u16), &f.name, f.intensity, &BTreeMap::new());
                push_bounded(
                    &mut self.events,
                    EventSnapshot { at: now, layer: LayerId(i as u16), name: f.name, intensity: f.intensity, attrs: BTreeMap::new(), label },
                    MAX_EVENTS,
                );
            }
            let input = &mut self.layer_inputs[i];
            input.params.clear();
            input.params.extend(p.params().iter().map(|(k, v)| (k.clone(), *v)));
            input.signals.clear();
            input.signals.extend(p.signals().iter().map(|(k, v)| (k.clone(), *v)));
        }

        // Compose.
        let events = std::mem::take(&mut self.pending_events);
        let out = self.composer.tick(now, dt, &ComposerInput { layers: &self.layer_inputs, events: &events });
        for cmd in out.commands {
            for sink in &mut self.sinks {
                sink.on_command(&cmd);
            }
            self.synth.send(cmd);
        }
        for sink in &mut self.sinks {
            sink.flush();
        }
        for mut n in out.notes {
            if let Some(cause) = n.cause.as_mut() {
                if cause.label.is_empty() {
                    cause.label = self.cause_label(n.layer, cause);
                }
            }
            push_bounded(&mut self.notes, n, MAX_NOTES);
        }

        // Audio-side housekeeping.
        self.recycle_audio();
        while let Some(old) = self.synth.take_recycled() {
            drop(old);
        }
        while let Some(old) = self.synth.take_recycled_effects() {
            drop(old);
        }
        while let Some(fb) = self.synth.poll_feedback() {
            if let SynthFeedback::Underruns(n) = fb {
                self.underruns = self.underruns.saturating_add(n);
            }
        }
        if let Some(tap) = self.tap.as_mut() {
            self.tap_buf.clear();
            let n = tap.pop_chunk(&mut self.tap_buf);
            if n > 0 {
                self.analyzer.push(&self.tap_buf[..n]);
                if let Some(tx) = &self.web_audio {
                    let _ = tx.send(Arc::new(self.tap_buf[..n].to_vec()));
                }
            }
        }

        if self.tick_count.is_multiple_of(SNAPSHOT_EVERY_TICKS) {
            self.publish_snapshot(now);
        }
    }

    /// Label for a note's cause: the matching recent event's label (same layer, name and
    /// time), else freshly formatted from the cause's own attrs.
    fn cause_label(&self, layer: LayerId, cause: &NoteCause) -> String {
        if let Some(e) = self.events.iter().rev().find(|e| e.layer == layer && e.name == cause.event && (e.at - cause.at).abs() < 0.05) {
            if !e.label.is_empty() {
                return e.label.clone();
            }
        }
        self.captioner.label(layer, &cause.event, 0.0, &cause.attrs)
    }

    fn publish_snapshot(&mut self, now: Seconds) {
        let mut snap = EngineSnapshot {
            scene_name: self.scene.name.clone(),
            viz_kind: self.scene.viz.clone(),
            time: now,
            ..Default::default()
        };
        self.composer.fill_snapshot(&mut snap);
        self.captioner.note_section(now, &snap.form.section, &snap.form.note);
        self.captioner.note_key(now, &snap.harmony.key_name, &snap.harmony.mode);
        snap.captions = self.captioner.captions().cloned().collect();
        snap.room = self.room.clone();
        // The composer fills harmony/mood/transport and may pre-populate layers; make sure
        // every layer exists and carries our signal/meter data.
        if snap.layers.len() != self.scene.layers.len() {
            snap.layers = (0..self.scene.layers.len()).map(|i| LayerSnapshot { id: LayerId(i as u16), ..Default::default() }).collect();
        }
        for (i, layer) in self.scene.layers.iter().enumerate() {
            let ls = &mut snap.layers[i];
            ls.id = LayerId(i as u16);
            ls.name = layer.name.clone();
            ls.role = layer.role;
            ls.instrument = self.instrument_names[i].clone();
            ls.muted = self.composer.layer_muted(ls.id);
            ls.signals = self.layer_inputs[i].signals.clone();
            ls.params = self.layer_inputs[i].params.clone();
            ls.meter = self.synth.layer_meter(ls.id);
            ls.source = self.layer_source[i].as_ref().map(|s| s.to_string());
        }
        snap.sources = self
            .sources
            .iter()
            .map(|(id, st)| SourceSnapshot {
                name: id.to_string(),
                kind: st.kind.clone(),
                state: if st.state == SourceState::Live && st.last_sample_at.is_some_and(|t| now - t > 120.0) {
                    SourceState::Idle
                } else {
                    st.state
                },
                message: st.message.clone(),
                rate: st.recent.iter().filter(|t| now - **t <= 1.0).count() as f64,
                last_sample_at: st.last_sample_at,
                readings: st.readings.clone(),
            })
            .collect();
        snap.replay = self.replay.as_ref().map(|r| r.snapshot());
        snap.dj = self.dj.clone();
        snap.events = self.events.iter().cloned().collect();
        snap.notes = self
            .notes
            .iter()
            .map(|n| {
                let mut n = n.clone();
                n.active = n.duration.is_none_or(|d| now < n.at + d);
                n
            })
            .collect();
        snap.master = self.synth.master_meter();
        snap.master_lufs = self.synth.lufs();
        snap.spectrum = self.analyzer.bands().to_vec();
        snap.viz = self.viz.clone();
        // Backends report underruns through the shared atomic, not the feedback queue.
        snap.underruns = self.underruns.saturating_add(self.synth.underruns());
        snap.audio_backend = self.audio_backend.clone();
        for sink in &mut self.sinks {
            sink.on_snapshot(&snap);
        }
        let _ = self.snapshot_tx.send(Arc::new(snap));
    }
}

/// Sources fed by a replay say so in the UI.
fn mark_replayed(sources: &mut BTreeMap<SourceId, SourceStatus>) {
    for st in sources.values_mut() {
        st.message = "replay".into();
    }
}

fn mmss(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

fn push_bounded<T>(q: &mut VecDeque<T>, item: T, max: usize) {
    if q.len() >= max {
        q.pop_front();
    }
    q.push_back(item);
}

/// Keep a handful of short attributes for display.
fn trim_attrs(attrs: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    attrs
        .into_iter()
        .filter(|(_, v)| match v {
            Value::Text(t) => t.len() <= 48,
            _ => true,
        })
        .take(6)
        .collect()
}

/// Build a `SourceCtx` and spawn every source in `specs` on the current tokio runtime.
#[cfg(feature = "native")]
pub fn spawn_sources(
    registry: &sonder_sources::Registry,
    specs: &[(SourceId, SourceSpec)],
    tx: tokio::sync::mpsc::Sender<Sample>,
    clock: Arc<dyn Clock>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    seed: u64,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let http = reqwest::Client::builder()
        .user_agent("sonder/0.1 (+https://github.com/yobryon/sonder)")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building http client")?;
    let mut handles = Vec::new();
    for (id, spec) in specs {
        let source = registry.create(spec).with_context(|| format!("source `{id}` ({})", spec.kind))?;
        let ctx = sonder_sources::SourceCtx {
            id: id.clone(),
            tx: tx.clone(),
            clock: clock.clone(),
            http: http.clone(),
            shutdown: shutdown.clone(),
            seed: seed ^ fnv(id.as_str()),
        };
        handles.push(tokio::spawn(source.run(ctx)));
    }
    Ok(handles)
}

#[cfg(feature = "native")]
fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_with_host() -> (ControlLoop, sonder_audio::SynthHost, tokio::sync::watch::Receiver<Arc<EngineSnapshot>>) {
        let scene = crate::load_scene("night-freeway").unwrap();
        let (controller, host) = sonder_audio::synth_pair(48_000.0, scene.layers.len(), None);
        let (tx, rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
        let control = ControlLoop::new(scene, 1, controller, None, None, "test".into(), tx).unwrap();
        (control, host, rx)
    }

    /// Two ticks = one snapshot publish.
    fn tick_and_publish(control: &mut ControlLoop, host: &mut sonder_audio::SynthHost, now: Seconds) {
        let mut block = vec![0.0f32; 960 * 2];
        control.tick(now);
        host.render(&mut block);
        control.tick(now + TICK_SECONDS);
        host.render(&mut block);
    }

    #[test]
    fn backend_underruns_reach_the_snapshot() {
        let (mut control, mut host, rx) = control_with_host();
        tick_and_publish(&mut control, &mut host, 0.0);
        assert_eq!(rx.borrow().underruns, 0);
        host.report_underrun();
        host.report_underrun();
        tick_and_publish(&mut control, &mut host, 1.0);
        assert_eq!(rx.borrow().underruns, 2);
    }

    #[test]
    fn events_get_labels_and_salient_ones_become_captions() {
        let (mut control, mut host, rx) = control_with_host();
        let traffic = SourceId::new("traffic");
        // First `car` on the layer: labelled from the built-in template and captioned.
        control.ingest(Sample::event(&traffic, 0.5, "car", 0.3).with_attr("lane", 2.0).with_attr("speed", 61.25));
        // A second, quiet one a moment later: labelled, not captioned.
        control.ingest(Sample::event(&traffic, 0.6, "car", 0.3).with_attr("lane", 3.0));
        // A loud one: captioned on intensity alone.
        control.ingest(Sample::event(&traffic, 0.7, "car", 0.95).with_attr("lane", 1.0));
        control.ingest(Sample::status(&traffic, 0.8, SourceState::Failed, "gone"));
        tick_and_publish(&mut control, &mut host, 1.0);
        let snap = rx.borrow().clone();
        let labels: Vec<&str> = snap.events.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["car in lane 2", "car in lane 3", "car in lane 1"]);
        let texts: Vec<&str> = snap.captions.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["car in lane 2", "car in lane 1", "traffic failed: gone"]);
        assert!(snap.captions[1].importance >= 0.9);
        assert_eq!(snap.captions[0].layer, Some(snap.events[0].layer));
        // Note causes are labelled from the event ring (or the template) when the composer
        // leaves the label empty.
        let cause = NoteCause { event: "car".into(), at: 0.6, label: String::new(), attrs: BTreeMap::new() };
        assert_eq!(control.cause_label(snap.events[0].layer, &cause), "car in lane 3");
        let fresh = NoteCause { event: "car".into(), at: 42.0, label: String::new(), attrs: [("lane".to_string(), Value::Num(4.0))].into() };
        assert_eq!(control.cause_label(snap.events[0].layer, &fresh), "car in lane 4");
    }

    #[test]
    fn live_audio_is_routed_to_every_reader_as_chunks() {
        let (mut control, mut host, _rx) = control_with_host();
        let traffic = SourceId::new("traffic");
        let readers = control.readers[&traffic].len();
        assert!(readers >= 2, "night-freeway shares `traffic` across layers");
        // Drain setup commands first so the ring is empty.
        let mut block = vec![0.0f32; 960 * 2];
        host.render(&mut block);
        let before = control.synth.capacity_left();
        control.ingest(Sample { source: traffic.clone(), at: 0.0, datum: Datum::Audio { rate: 48_000, samples: vec![0.1; 2400] } });
        assert_eq!(before - control.synth.capacity_left(), readers, "one AudioChunk command per reading layer");
        // Unknown sources route nowhere.
        let before = control.synth.capacity_left();
        control.ingest(Sample { source: SourceId::new("nobody"), at: 0.0, datum: Datum::Audio { rate: 48_000, samples: vec![0.0; 10] } });
        assert_eq!(before, control.synth.capacity_left());
        host.render(&mut block);
        control.tick(0.02);
    }

    #[test]
    fn source_rate_decays_when_samples_stop() {
        let (mut control, mut host, rx) = control_with_host();
        let traffic = SourceId::new("traffic");
        for i in 0..5 {
            control.ingest(Sample::reading(&traffic, i as f64 * 0.1, [("mean_speed", 60.0)]));
        }
        tick_and_publish(&mut control, &mut host, 0.5);
        let src = rx.borrow().sources.iter().find(|s| s.name == "traffic").cloned().unwrap();
        assert_eq!(src.rate, 5.0);
        assert_eq!(src.state, SourceState::Live);
        // Nothing arrives for a while: the per-second rate must fall to zero, not stick at 5.
        tick_and_publish(&mut control, &mut host, 5.0);
        let src = rx.borrow().sources.iter().find(|s| s.name == "traffic").cloned().unwrap();
        assert_eq!(src.rate, 0.0, "rate is stale: {src:?}");
    }
}
