//! Live mode: real clock, sources on tokio, control loop on its own thread, audio on a
//! backend thread.

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
    /// Resolves when the engine stops (quit command or fatal error).
    pub done: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
    shutdown: tokio::sync::watch::Sender<bool>,
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
pub async fn start(mut scene: Scene, opts: EngineOptions) -> anyhow::Result<EngineHandle> {
    apply_overrides(&mut scene, &opts.source_overrides)?;
    if let Err(errs) = crate::validate(&scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let seed = opts.seed.or(scene.conductor.seed).unwrap_or_else(rand::random::<u64>);
    let requested_sr = if opts.sample_rate == 0 { 48_000 } else { opts.sample_rate };
    let max_layers = scene.layers.len().max(1);

    // Audio first, so we know the real sample rate.
    let tap_cfg = if opts.web_tap { Some(sonder_audio::TapConfig::default()) } else { None };
    let (mut controller, stream, backend_name) = open_audio(opts.backend, requested_sr, max_layers, tap_cfg)?;
    let tap = controller.take_tap();
    let sample_rate = controller.sample_rate() as u32;

    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<UserCommand>(64);
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(4096);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let web_audio_tx = if opts.web_tap { Some(tokio::sync::broadcast::channel::<Arc<Vec<f32>>>(64).0) } else { None };

    let clock: Arc<dyn Clock> = Arc::new(WallClock::new());
    let specs = ControlLoop::source_specs(&scene);
    let registry = registry();
    let source_tasks = spawn_sources(&registry, &specs, sample_tx, clock.clone(), shutdown_rx.clone(), seed)?;

    let perc_layers: Vec<bool> = scene.layers.iter().map(|l| l.role == Role::Perc).collect();
    let mut control = ControlLoop::new(scene, seed, controller, tap, web_audio_tx.clone(), backend_name.clone(), snapshot_tx)
        .context("building control loop")?;
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

    // Control thread.
    let mut shutdown_watch = shutdown_rx.clone();
    let shutdown_for_thread = shutdown_tx.clone();
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
                // Bound the per-tick drain so a firehose can't starve the tick.
                let mut drained = 0;
                while let Ok(sample) = sample_rx.try_recv() {
                    control.ingest(sample);
                    drained += 1;
                    if drained > 20_000 {
                        break;
                    }
                }
                control.tick(clock.now());
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
            // Shutdown order: sources (they also saw the watch flip), then the control
            // loop with its sinks (MIDI sends its note-offs on drop), then the audio
            // stream (joins the backend thread). Only then is the engine `done`.
            for t in source_tasks {
                t.abort();
            }
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
        done: done_rx,
        shutdown: shutdown_tx,
    })
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
