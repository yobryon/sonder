//! Live mode: real clock, sources on tokio, control loop on its own thread, audio on a
//! backend thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use sonder_audio::{AudioStream, Backend};
use sonder_core::*;

use crate::control::{spawn_sources, ControlLoop};
use crate::TICK_SECONDS;

/// Which audio output to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioBackendChoice {
    /// Native if it works, else the silent realtime clock.
    #[default]
    Auto,
    Native,
    /// No device: a silent realtime pacing thread (meters, tap and web audio still work).
    Null,
    /// s16le interleaved stereo on stdout.
    PcmStdout,
}

#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    pub backend: AudioBackendChoice,
    /// 0 = backend default (48000 preferred).
    pub sample_rate: u32,
    pub seed: Option<u64>,
    /// Keep an audio tap for the web UI / analyzer.
    pub web_tap: bool,
    /// Replace the source of the named layer (and everything sharing it).
    pub source_overrides: Vec<(String, SourceSpec)>,
    /// MIDI output port substring, or "virtual". None = no MIDI.
    pub midi_port: Option<String>,
    /// Play a data log instead of the scene's sources (time travel).
    pub replay: Option<ReplayOptions>,
    /// Write every ingested sample to this `.sonder` / `.sonder.gz` data log.
    pub record_data: Option<std::path::PathBuf>,
    /// Host a shared room on this relay (docs/ROOMS.md).
    pub room: Option<crate::room::RoomConfig>,
    /// Keep the last 10 minutes of data in memory; `/moment` saves them here.
    pub moments: Option<std::path::PathBuf>,
    /// `host:port` to send OSC to (see `docs/OSC.md`). None = no OSC.
    pub osc_target: Option<String>,
    /// Join an Ableton Link session (feature `link`).
    pub link: bool,
    /// Run the DJ (rule-based scene switching; see `docs/DJ.md`).
    pub dj: Option<DjOptions>,
    /// Run the narrator (one sentence from Claude every `interval` seconds).
    pub narrator: Option<crate::narrator::NarratorOptions>,
}

/// A `dj.toml` to run, for [`EngineOptions::dj`].
#[derive(Debug, Clone)]
pub struct DjOptions {
    /// The file's text (`crate::dj::DEFAULT_CONFIG` for the built-in rules).
    pub toml: String,
    /// Directory rule scenes are resolved against (besides built-ins and the cwd).
    pub base_dir: Option<std::path::PathBuf>,
}

impl DjOptions {
    /// The embedded `dj/default.toml`.
    pub fn builtin() -> Self {
        Self { toml: crate::dj::DEFAULT_CONFIG.to_string(), base_dir: None }
    }
}

/// A log to play, for [`EngineOptions::replay`].
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub log: Arc<crate::datalog::DataLog>,
    /// Shown in the UI (usually the file name).
    pub title: String,
    pub config: crate::replay::ReplayConfig,
}

/// Rendered audio for the web server: a broadcast of interleaved stereo f32 chunks.
pub type WebAudio = tokio::sync::broadcast::Receiver<Arc<Vec<f32>>>;

/// Handles to a running engine.
pub struct EngineHandle {
    pub snapshot: crate::SnapshotRx,
    pub commands: tokio::sync::mpsc::Sender<UserCommand>,
    pub audio: Option<tokio::sync::broadcast::Sender<Arc<Vec<f32>>>>,
    pub sample_rate: u32,
    pub audio_backend: String,
    /// Name of the connected MIDI port, if any.
    pub midi_port: Option<String>,
    /// Resolved OSC target, if any.
    pub osc_target: Option<std::net::SocketAddr>,
    /// Resolves when the engine stops (quit command or fatal error).
    pub done: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
    source_tasks_alive: Arc<AtomicUsize>,
    /// Scene reloads applied so far (`LoadScene`).
    reloads: Arc<AtomicUsize>,
}

impl EngineHandle {
    /// Ask everything to stop. `done` resolves once the control thread, the audio
    /// stream and any command sinks (MIDI) have been shut down.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
    pub fn subscribe_audio(&self) -> Option<WebAudio> {
        self.audio.as_ref().map(|a| a.subscribe())
    }
    /// Source tasks still running (updated every control tick).
    pub fn source_tasks_alive(&self) -> usize {
        self.source_tasks_alive.load(Ordering::Relaxed)
    }
    /// Number of `LoadScene` reloads that have been applied.
    pub fn reloads(&self) -> usize {
        self.reloads.load(Ordering::Relaxed)
    }
}

/// Spare layer slots the synth is built with, so a hot-reloaded scene may grow.
const MIN_LAYER_SLOTS: usize = 12;

/// One scene's worth of running sources.
struct Generation {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    sample_rx: tokio::sync::mpsc::Receiver<Sample>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Generation {
    fn spawn(
        registry: &sonder_sources::Registry,
        scene: &Scene,
        clock: Arc<dyn Clock>,
        seed: u64,
        rt: &tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        Self::spawn_specs(registry, &ControlLoop::source_specs(scene), clock, seed, rt)
    }
    /// Spawn an arbitrary set of sources (the DJ's watchers).
    fn spawn_specs(
        registry: &sonder_sources::Registry,
        specs: &[(SourceId, SourceSpec)],
        clock: Arc<dyn Clock>,
        seed: u64,
        rt: &tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let (sample_tx, sample_rx) = tokio::sync::mpsc::channel::<Sample>(4096);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let _guard = rt.enter();
        let tasks = spawn_sources(registry, specs, sample_tx, clock, shutdown_rx, seed)?;
        Ok(Self { tasks, sample_rx, shutdown })
    }
    fn alive(&self) -> usize {
        self.tasks.iter().filter(|t| !t.is_finished()).count()
    }
    fn stop(self) {
        let _ = self.shutdown.send(true);
        for t in self.tasks {
            t.abort();
        }
    }
}

impl Drop for EngineHandle {
    /// Dropping the last handle stops the engine; otherwise the control thread and the
    /// audio stream would keep running (and playing) with nobody able to reach them.
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// The source registry: builtin adapters plus the camera.
pub fn registry() -> sonder_sources::Registry {
    let mut r = sonder_sources::Registry::new();
    sonder_vision::register(&mut r);
    r
}

/// Apply `--source` overrides to a scene.
pub fn apply_overrides(scene: &mut Scene, overrides: &[(String, SourceSpec)]) -> anyhow::Result<()> {
    for (layer_name, spec) in overrides {
        let target = if layer_name.is_empty() {
            scene.layers.iter().position(|l| matches!(l.source, LayerSource::Spec(_)))
        } else {
            scene.layers.iter().position(|l| &l.name == layer_name)
        };
        let Some(i) = target else {
            if layer_name.is_empty() {
                anyhow::bail!("no layer in scene `{}` has a source to override; pass --layer <name>", scene.name);
            }
            anyhow::bail!("no layer `{layer_name}` to override the source of");
        };
        scene.layers[i].source = LayerSource::Spec(spec.clone());
    }
    Ok(())
}

/// Start the engine on the current tokio runtime.
///
/// `UserCommand::LoadScene { toml }` on the command channel hot-reloads: the TOML is
/// parsed and validated, the old sources are stopped, the control loop is rebuilt around
/// the same synth (all notes off, instruments/effects/tempo re-sent) and the new scene's
/// sources are spawned; the snapshot channel is kept. An invalid scene is logged, surfaces
/// as a caption, and leaves the old one running. `--source` overrides apply to the
/// initial scene only.
pub async fn start(mut scene: Scene, opts: EngineOptions) -> anyhow::Result<EngineHandle> {
    apply_overrides(&mut scene, &opts.source_overrides)?;
    if let Err(errs) = crate::validate(&scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let seed = opts.seed.or(scene.conductor.seed).unwrap_or_else(rand::random::<u64>);
    let requested_sr = if opts.sample_rate == 0 { 48_000 } else { opts.sample_rate };
    let max_layers = scene.layers.len().max(MIN_LAYER_SLOTS);

    // Audio first, so we know the real sample rate.
    let tap_cfg = if opts.web_tap { Some(sonder_audio::TapConfig::default()) } else { None };
    let (mut controller, stream, backend_name) = open_audio(opts.backend, requested_sr, max_layers, tap_cfg)?;
    let tap = controller.take_tap();
    let sample_rate = controller.sample_rate() as u32;

    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<UserCommand>(64);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let web_audio_tx = if opts.web_tap { Some(tokio::sync::broadcast::channel::<Arc<Vec<f32>>>(64).0) } else { None };
    let alive = Arc::new(AtomicUsize::new(0));
    let reloads = Arc::new(AtomicUsize::new(0));

    let clock: Arc<dyn Clock> = Arc::new(WallClock::new());
    let registry = registry();
    let rt = tokio::runtime::Handle::current();

    let perc_layers: Vec<bool> = scene.layers.iter().map(|l| l.role == Role::Perc).collect();
    let mut control = ControlLoop::new(scene.clone(), seed, controller, tap, web_audio_tx.clone(), backend_name.clone(), snapshot_tx)
        .context("building control loop")?;
    if let Some(r) = &opts.replay {
        control.set_replay(Some(crate::replay::Replay::new(r.log.clone(), r.title.clone(), r.config.clone())));
    }
    // A replay feeds the scene's sources from the log; nothing live runs.
    let mut generation = if control.replay().is_some() {
        if control.replay_fed_sources().is_empty() {
            tracing::warn!("the log feeds none of scene `{}`'s sources; its layers play on the conductor alone", scene.name);
        }
        Generation::empty()
    } else {
        Generation::spawn(&registry, &scene, clock.clone(), seed, &rt)?
    };
    if let Some(path) = &opts.record_data {
        let header = crate::datalog::LogHeader::new(&scene.name, seed, crate::datalog::unix_now(), &ControlLoop::source_specs(&scene));
        let mut writer = crate::datalog::DataLogWriter::create(path, &header).with_context(|| format!("creating data log {}", path.display()))?;
        tracing::info!(path = %path.display(), "recording data log");
        control.add_sample_sink(Box::new(move |s: &Sample| writer.write(s)));
    }
    let mut room = opts.room.clone().map(|cfg| {
        let link = crate::room::RoomLink::spawn(cfg, snapshot_rx.clone(), shutdown_rx.clone(), &rt);
        control.set_room(&link.id(), link.config().url.trim_end_matches('/'), "host");
        control.add_sample_sink(link.sink());
        link.announce(control.scene(), seed, 0.0);
        tracing::info!(url = %link.config().url, "hosting a room");
        link
    });
    let moments = opts.moments.clone().map(crate::moments::MomentBuffer::new);
    if let Some(m) = &moments {
        control.add_sample_sink(m.sink());
        tracing::info!(dir = %m.dir().display(), "keeping the last 10 minutes for /moment");
    }
    #[cfg_attr(not(feature = "midi"), allow(unused_mut))]
    let mut midi_name = None;
    if let Some(port) = &opts.midi_port {
        #[cfg(feature = "midi")]
        {
            let midi = crate::midi::MidiOut::open(port, perc_layers).context("opening MIDI output")?;
            midi_name = Some(midi.port_name().to_string());
            control.add_sink(Box::new(midi));
        }
        #[cfg(not(feature = "midi"))]
        {
            let _ = perc_layers;
            anyhow::bail!("this build has no MIDI support (feature `midi`); requested port `{port}`");
        }
    }
    #[cfg_attr(not(feature = "osc"), allow(unused_mut))]
    let mut osc_target = None;
    if let Some(target) = &opts.osc_target {
        #[cfg(feature = "osc")]
        {
            let osc = crate::osc::OscOut::connect(target).context("opening OSC output")?;
            osc_target = Some(osc.target());
            tracing::info!(target = %osc.target(), "osc out");
            control.add_sink(Box::new(osc));
        }
        #[cfg(not(feature = "osc"))]
        anyhow::bail!("this build has no OSC support (feature `osc`); requested target `{target}`");
    }
    if opts.link {
        #[cfg(feature = "link")]
        {
            let link = crate::link::LinkSync::start(scene.conductor.tempo);
            tracing::info!("ableton link enabled");
            control.add_sink(Box::new(link));
        }
        #[cfg(not(feature = "link"))]
        anyhow::bail!("this build has no Ableton Link support (build with `--features link`)");
    }

    // The DJ and its watcher sources (independent of the scene's generation).
    let mut dj = match &opts.dj {
        Some(d) => {
            let dj = crate::dj::Dj::from_toml(&d.toml, d.base_dir.as_deref()).context("dj.toml")?;
            let watchers = Generation::spawn_specs(&registry, dj.watch_specs(), clock.clone(), seed ^ 0xd1, &rt).context("starting the DJ's watcher sources")?;
            tracing::info!(watchers = dj.watch_specs().len(), "dj on");
            Some((dj, watchers))
        }
        None => None,
    };
    // Captions from tasks outside the control thread (the narrator).
    let (caption_tx, mut caption_rx) = tokio::sync::mpsc::channel::<(String, f64)>(16);
    if let Some(n) = opts.narrator.clone() {
        rt.spawn(crate::narrator::run(n, snapshot_rx.clone(), caption_tx, shutdown_rx.clone()));
    }

    // Control thread.
    let mut shutdown_watch = shutdown_rx.clone();
    let shutdown_for_thread = shutdown_tx.clone();
    let alive_for_thread = alive.clone();
    let reloads_for_thread = reloads.clone();
    let fixed_seed = opts.seed;
    std::thread::Builder::new()
        .name("sonder-control".into())
        .spawn(move || {
            let _stream = stream; // keep audio alive for the life of the control thread
            let period = Duration::from_secs_f64(TICK_SECONDS);
            let mut next = Instant::now();
            let result: anyhow::Result<()> = loop {
                if *shutdown_watch.borrow_and_update() {
                    break Ok(());
                }
                while let Ok(cmd) = cmd_rx.try_recv() {
                    control.command(cmd);
                }
                let now = clock.now();
                if let Some(toml_src) = control.take_load_request() {
                    // A scene the user picked (picker, editor, --watch): the DJ steps back.
                    match crate::parse_scene(&toml_src) {
                        Ok(scene) => {
                            if swap_scene(&mut control, &mut generation, &registry, &clock, &rt, &reloads_for_thread, scene, fixed_seed) {
                                if let Some((dj, _)) = dj.as_mut() {
                                    if dj.enabled() {
                                        dj.pause_for_user();
                                        control.push_caption("DJ paused — you picked the scene", 0.5);
                                    }
                                }
                            }
                        }
                        Err(e) => tracing::error!("scene reload rejected: {e:#}"),
                    }
                }
                if let Some((dj, watchers)) = dj.as_mut() {
                    for enabled in control.take_dj_requests() {
                        dj.set_enabled(enabled, now);
                        control.push_caption(if enabled { "DJ on" } else { "DJ off" }, 0.5);
                    }
                    let mut drained = 0;
                    while let Ok(sample) = watchers.sample_rx.try_recv() {
                        dj.observe(sample);
                        drained += 1;
                        if drained > 5_000 {
                            break;
                        }
                    }
                    let current = control.scene().name.clone();
                    for action in dj.tick(now, &current) {
                        match action {
                            crate::dj::Action::Load { scene, caption } => {
                                if swap_scene(&mut control, &mut generation, &registry, &clock, &rt, &reloads_for_thread, *scene, fixed_seed) {
                                    control.push_caption(caption, 0.9);
                                }
                            }
                            crate::dj::Action::Caption(text, importance) => control.push_caption(text, importance),
                        }
                    }
                    control.set_dj(Some(dj.snapshot(now)));
                }
                while let Ok((text, importance)) = caption_rx.try_recv() {
                    control.push_caption(text, importance);
                }
                // Bound the per-tick drain so a firehose can't starve the tick.
                let mut drained = 0;
                while let Ok(sample) = generation.sample_rx.try_recv() {
                    control.ingest(sample);
                    drained += 1;
                    if drained > 20_000 {
                        break;
                    }
                }
                control.tick(now);
                if let Some(link) = room.as_mut() {
                    link.pump(&mut control, reloads_for_thread.load(Ordering::Relaxed), now);
                }
                if control.take_moment_request() {
                    match &moments {
                        Some(m) => match m.save(control.scene(), control.seed(), now) {
                            Ok((path, n)) => {
                                tracing::info!(path = %path.display(), samples = n, "moment saved");
                                control.push_caption(format!("moment saved: {} ({n} samples)", path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()), 0.8);
                            }
                            Err(e) => {
                                tracing::error!("saving moment: {e}");
                                control.push_caption(format!("moment not saved: {e}"), 0.6);
                            }
                        },
                        None => control.push_caption("moments are off — start the host with --moments <dir>", 0.5),
                    }
                }
                alive_for_thread.store(generation.alive(), Ordering::Relaxed);
                for path in control.take_save_requests() {
                    let scene = control.current_scene();
                    let path = if path.is_empty() { format!("{}.toml", slug(&scene.name)) } else { path };
                    match toml::to_string_pretty(&scene) {
                        Ok(text) => {
                            if let Err(e) = std::fs::write(&path, text) {
                                tracing::error!(%path, "saving scene: {e}");
                            } else {
                                tracing::info!(%path, "scene saved");
                            }
                        }
                        Err(e) => tracing::error!("serializing scene: {e}"),
                    }
                }
                if control.quit_requested() {
                    let _ = shutdown_for_thread.send(true);
                    break Ok(());
                }
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_millis(200) {
                    // We fell far behind (suspend?). Don't try to catch up.
                    next = now;
                }
            };
            // Shutdown order: sources, then the control loop with its sinks (MIDI sends its
            // note-offs on drop), then the audio stream (joins the backend thread). Only
            // then is the engine `done`.
            generation.stop();
            if let Some((_, watchers)) = dj.take() {
                watchers.stop();
            }
            alive_for_thread.store(0, Ordering::Relaxed);
            drop(control);
            drop(_stream);
            let _ = done_tx.send(result);
        })
        .context("spawning control thread")?;

    Ok(EngineHandle {
        snapshot: snapshot_rx,
        commands: cmd_tx,
        audio: web_audio_tx,
        sample_rate,
        audio_backend: backend_name,
        midi_port: midi_name,
        osc_target,
        done: done_rx,
        shutdown: shutdown_tx,
        source_tasks_alive: alive,
        reloads,
    })
}

impl Generation {
    fn empty() -> Self {
        let (_, sample_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self { tasks: Vec::new(), sample_rx, shutdown }
    }
}

/// Validate and apply a scene to the control loop. Returns the seed the new scene runs
/// with. On error the old scene keeps running.
fn reload(control: &mut ControlLoop, scene: Scene, fixed_seed: Option<u64>) -> anyhow::Result<u64> {
    if let Err(errs) = crate::validate(&scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let seed = fixed_seed.or(scene.conductor.seed).unwrap_or_else(rand::random::<u64>);
    control.rebuild(scene, seed)?;
    Ok(seed)
}

/// The hot-reload path shared by `LoadScene` and the DJ: rebuild the control loop around
/// `scene`, stop the old sources and start the new ones. Errors are logged; `true` when
/// the scene changed.
#[allow(clippy::too_many_arguments)]
fn swap_scene(
    control: &mut ControlLoop,
    generation: &mut Generation,
    registry: &sonder_sources::Registry,
    clock: &Arc<dyn Clock>,
    rt: &tokio::runtime::Handle,
    reloads: &AtomicUsize,
    scene: Scene,
    fixed_seed: Option<u64>,
) -> bool {
    match reload(control, scene, fixed_seed) {
        Ok(new_seed) => {
            let scene = control.scene();
            let spawned = if control.replay().is_some() { Ok(Generation::empty()) } else { Generation::spawn(registry, scene, clock.clone(), new_seed, rt) };
            match spawned {
                Ok(g) => {
                    std::mem::replace(generation, g).stop();
                    tracing::info!(scene = %scene.name, "scene reloaded");
                }
                Err(e) => {
                    tracing::error!("reloaded scene `{}` but its sources failed to start: {e:#}", scene.name);
                    std::mem::replace(generation, Generation::empty()).stop();
                }
            }
            reloads.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(e) => {
            tracing::error!("scene reload rejected: {e:#}");
            false
        }
    }
}

type Opened = (sonder_audio::SynthController, AudioStream, String);

/// Open the requested backend, falling back to Null for `Auto`.
fn open_audio(
    choice: AudioBackendChoice,
    sample_rate: u32,
    max_layers: usize,
    tap: Option<sonder_audio::TapConfig>,
) -> anyhow::Result<Opened> {
    let try_backend = |backend: Backend, tap: Option<sonder_audio::TapConfig>| -> anyhow::Result<Opened> {
        let name = backend.name().to_string();
        // Build the synth at the device's real rate so pitch is right.
        let sr = match backend {
            #[cfg(feature = "native-audio")]
            Backend::Native => sonder_audio::native_default_sample_rate().unwrap_or(sample_rate),
            _ => sample_rate,
        };
        let (controller, host) = sonder_audio::synth_pair(sr as f32, max_layers, tap);
        let stream = AudioStream::start(backend, host).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        Ok((controller, stream, name))
    };
    match choice {
        AudioBackendChoice::Native => try_backend(Backend::Native, tap),
        AudioBackendChoice::Null => try_backend(Backend::Null, tap),
        AudioBackendChoice::PcmStdout => try_backend(Backend::PcmStdout, tap),
        AudioBackendChoice::Auto => match try_backend(Backend::Native, tap) {
            Ok(o) => Ok(o),
            Err(e) => {
                tracing::warn!("native audio unavailable ({e}); running silent — listen via the web UI");
                try_backend(Backend::Null, tap)
            }
        },
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn null_opts() -> EngineOptions {
        EngineOptions { backend: AudioBackendChoice::Null, seed: Some(7), ..EngineOptions::default() }
    }

    /// Resolves once the control thread has exited (it owns the snapshot sender).
    async fn control_thread_exited(mut rx: crate::SnapshotRx) {
        while rx.changed().await.is_ok() {}
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quit_resolves_done_and_stops_the_control_thread() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let mut handle = start(scene, null_opts()).await.unwrap();
        let rx = handle.snapshot.clone();
        handle.commands.send(UserCommand::Quit).await.unwrap();
        let res = tokio::time::timeout(Duration::from_secs(5), &mut handle.done).await.expect("done never resolved");
        res.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(5), control_thread_exited(rx)).await.expect("control thread still running");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_scene_swaps_scene_and_sources_in_place() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let mut handle = start(scene, null_opts()).await.unwrap();
        let mut rx = handle.snapshot.clone();
        // Night-freeway: one synthetic source running.
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.source_tasks_alive() != 1 {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("initial sources never came up");
        assert_eq!(rx.borrow().scene_name, "Night Freeway");

        // An invalid scene is rejected and leaves everything running.
        handle.commands.send(UserCommand::LoadScene { toml: "name = 'x'\n[[layer]]\nname = 'a'\nrole = 'pad'\ninstrument = 'nope'".into() }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(handle.reloads(), 0);
        assert_eq!(rx.borrow().scene_name, "Night Freeway");
        assert_eq!(handle.source_tasks_alive(), 1);

        // A scene with no sources replaces it: the name, the layers and the sources change.
        let bare = "name = \"Bare\"\n[[layer]]\nname = \"drone\"\nrole = \"pad\"\ninstrument = \"pad.glass\"\n";
        handle.commands.send(UserCommand::LoadScene { toml: bare.into() }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.borrow().scene_name != "Bare" {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("scene name never changed");
        assert_eq!(handle.reloads(), 1);
        let snap = rx.borrow().clone();
        assert_eq!(snap.layers.len(), 1);
        assert!(snap.sources.is_empty());
        assert!(snap.captions.iter().any(|c| c.text == "scene: Bare"), "{:?}", snap.captions);
        // The old scene's source task was stopped.
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.source_tasks_alive() != 0 {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("old sources still running");
        // And a scene with more layers than the first one still fits (spare slots).
        let (_, big) = crate::scenes::BUILTIN_SCENE_SOURCES.iter().find(|(n, _)| *n == "midnight-walk").unwrap();
        handle.commands.send(UserCommand::LoadScene { toml: big.to_string() }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while rx.borrow().scene_name == "Bare" {
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("second reload never applied");
        assert_eq!(handle.reloads(), 2);
        // Still ticking and still quits cleanly.
        handle.commands.send(UserCommand::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut handle.done).await.expect("done").unwrap().unwrap();
    }

    /// Wait (≤ 10 s) for a snapshot that satisfies `pred`.
    async fn wait_for(rx: &mut crate::SnapshotRx, what: &str, pred: impl Fn(&EngineSnapshot) -> bool) -> Arc<EngineSnapshot> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if pred(&rx.borrow()) {
                    return rx.borrow().clone();
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dj_switches_scenes_on_a_rule_and_fills_the_snapshot() {
        // A synthetic rule: the walk's periodic `tick` sends us to Midnight Walk for 2 s,
        // then (cooldown keeps it from re-firing) back to the default.
        let toml = r#"
[dj]
min_hold = 0
cooldown = 600
default = "night-freeway"

[[rule]]
name = "walk ticks"
when = "synthetic.walk: event tick with n >= 1"
scene = "midnight-walk"
hold = 2
params = { event_every = 0.3 }
"#;
        let scene = crate::load_scene("night-freeway").unwrap();
        let opts = EngineOptions { dj: Some(DjOptions { toml: toml.into(), base_dir: None }), ..null_opts() };
        let mut handle = start(scene, opts).await.unwrap();
        let mut rx = handle.snapshot.clone();
        let snap = wait_for(&mut rx, "the DJ to switch to Midnight Walk", |s| s.scene_name == "Midnight Walk").await;
        let dj = snap.dj.clone().expect("snapshot.dj is filled while the DJ runs");
        assert!(dj.enabled);
        assert_eq!(dj.rule, "walk ticks");
        assert!(dj.reason.starts_with("tick (n "), "{}", dj.reason);
        assert!(dj.hold_remaining <= 2.0);
        assert!(
            snap.captions.iter().any(|c| c.text.starts_with("DJ → Midnight Walk: tick") && c.importance >= 0.9),
            "{:?}",
            snap.captions.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert_eq!(handle.reloads(), 1);
        // The hold ends, nothing else fires: back to the default scene.
        let snap = wait_for(&mut rx, "the DJ to return to the default", |s| s.scene_name == "Night Freeway").await;
        let dj = snap.dj.clone().unwrap();
        assert!(dj.rule.is_empty() && dj.reason == "quiet again", "{dj:?}");
        assert_eq!(handle.reloads(), 2);
        // SetDj false shows up in the snapshot; a user-picked scene pauses it.
        handle.commands.send(UserCommand::SetDj { enabled: false }).await.unwrap();
        let snap = wait_for(&mut rx, "the DJ to turn off", |s| s.dj.as_ref().is_some_and(|d| !d.enabled)).await;
        assert_eq!(snap.dj.as_ref().unwrap().reason, "off");
        handle.commands.send(UserCommand::SetDj { enabled: true }).await.unwrap();
        wait_for(&mut rx, "the DJ to turn on", |s| s.dj.as_ref().is_some_and(|d| d.enabled)).await;
        let bare = "name = \"Bare\"\n[[layer]]\nname = \"drone\"\nrole = \"pad\"\ninstrument = \"pad.glass\"\n";
        handle.commands.send(UserCommand::LoadScene { toml: bare.into() }).await.unwrap();
        let snap = wait_for(&mut rx, "the user's scene", |s| s.scene_name == "Bare" && s.dj.as_ref().is_some_and(|d| !d.enabled)).await;
        assert!(snap.dj.as_ref().unwrap().reason.starts_with("paused"));
        handle.commands.send(UserCommand::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut handle.done).await.expect("done").unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bad_dj_config_fails_start() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let opts = EngineOptions { dj: Some(DjOptions { toml: "[[rule]]\nname='x'\nwhen='k: event e'\nscene='no-such-scene'\n".into(), base_dir: None }), ..null_opts() };
        let err = format!("{:#}", start(scene, opts).await.err().expect("bad dj.toml must fail"));
        assert!(err.contains("dj.toml") && err.contains("rule `x`") && err.contains("no-such-scene"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_handle_stops_the_engine() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let handle = start(scene, null_opts()).await.unwrap();
        let rx = handle.snapshot.clone();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(5), control_thread_exited(rx))
            .await
            .expect("engine kept running after its handle was dropped");
    }
}
