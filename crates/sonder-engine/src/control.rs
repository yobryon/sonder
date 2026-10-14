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

use crate::{SNAPSHOT_EVERY_TICKS, TICK_SECONDS};

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
        let composer = Composer::new(&scene, seed).map_err(|e| anyhow::anyhow!("composer: {e}"))?;
        let mut pipelines = Vec::with_capacity(scene.layers.len());
        let mut layer_source = Vec::with_capacity(scene.layers.len());
        let mut readers: HashMap<SourceId, Vec<usize>> = HashMap::new();
        let mut sources = BTreeMap::new();
        let mut instrument_names = Vec::with_capacity(scene.layers.len());

        for (i, layer) in scene.layers.iter().enumerate() {
            let pipeline = LayerPipeline::compile(&layer.signals, &layer.map, seed ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15))
                .map_err(|e| anyhow::anyhow!("layer `{}`: {e}", layer.name))?;
            pipelines.push(Some(pipeline));
            let owner = scene.source_owner(layer);
            let sid = owner.map(|o| SourceId::new(&o.name));
            if let (Some(sid), Some(owner)) = (&sid, owner) {
                readers.entry(sid.clone()).or_default().push(i);
                if let LayerSource::Spec(spec) = &owner.source {
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

            let (inst, name) = crate::instruments::resolve(&scene, layer);
            synth.send(SynthCommand::SetInstrument { layer: LayerId(i as u16), instrument: Box::new(inst) });
            instrument_names.push(name);
        }
        synth.send(SynthCommand::SetEffects(Box::new(scene.effects.clone())));
        synth.send(SynthCommand::SetTempo { bpm: scene.conductor.tempo as f32 });

        let analyzer = Analyzer::new(synth.sample_rate(), 48);
        let n = scene.layers.len();
        Ok(Self {
            scene,
            composer,
            pipelines,
            layer_source,
            readers,
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
        })
    }

    /// The names of the sources this scene needs started, with their specs (after the
    /// engine applied overrides).
    pub fn source_specs(scene: &Scene) -> Vec<(SourceId, SourceSpec)> {
        scene
            .layers
            .iter()
            .filter_map(|l| match &l.source {
                LayerSource::Spec(spec) => Some((SourceId::new(&l.name), spec.clone())),
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

    /// Feed one sample from a source.
    pub fn ingest(&mut self, sample: Sample) {
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
                    push_bounded(
                        &mut self.events,
                        EventSnapshot { at: now, layer: LayerId(first as u16), name, intensity, attrs: trim_attrs(attrs) },
                        MAX_EVENTS,
                    );
                }
            }
            Datum::Status { state, message } => {
                if let Some(st) = self.sources.get_mut(&sample.source) {
                    st.state = state;
                    st.message = message;
                }
            }
            Datum::Viz { payload } => {
                self.viz.insert(sample.source.to_string(), payload);
            }
        }
    }

    /// Apply a UI command.
    pub fn command(&mut self, cmd: UserCommand) {
        match &cmd {
            UserCommand::Quit => self.quit_requested = true,
            UserCommand::SaveScene { path } => self.save_requests.push(path.clone()),
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
                push_bounded(
                    &mut self.events,
                    EventSnapshot { at: now, layer: LayerId(i as u16), name: f.name, intensity: f.intensity, attrs: BTreeMap::new() },
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
        for n in out.notes {
            push_bounded(&mut self.notes, n, MAX_NOTES);
        }

        // Audio-side housekeeping.
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

    fn publish_snapshot(&mut self, now: Seconds) {
        let mut snap = EngineSnapshot {
            scene_name: self.scene.name.clone(),
            viz_kind: self.scene.viz.clone(),
            time: now,
            ..Default::default()
        };
        self.composer.fill_snapshot(&mut snap);
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
        snap.spectrum = self.analyzer.bands().to_vec();
        snap.viz = self.viz.clone();
        // Backends report underruns through the shared atomic, not the feedback queue.
        snap.underruns = self.underruns.saturating_add(self.synth.underruns());
        snap.audio_backend = self.audio_backend.clone();
        let _ = self.snapshot_tx.send(Arc::new(snap));
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
