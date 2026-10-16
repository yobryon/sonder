//! Offline, deterministic rendering to WAV.
//!
//! Runs sources, control loop and synth in lockstep on one thread inside a paused-time
//! tokio runtime. Each control tick: advance the virtual clock 20 ms, let source tasks
//! run their timers, drain samples, compose, render 20 ms of audio.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use sonder_core::*;

use crate::control::{spawn_sources, ControlLoop, SampleSink};
use crate::replay::Replay;
use crate::TICK_SECONDS;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderStats {
    pub seconds: f64,
    pub sample_rate: u32,
    pub frames: usize,
    pub peak: f32,
    pub rms: f32,
    pub notes: usize,
    pub events: usize,
    pub seed: u64,
    /// Every caption the engine produced, in order.
    pub captions: Vec<Caption>,
    /// Every event that reached the snapshot stream, in order.
    pub event_log: Vec<EventSnapshot>,
    /// Every note that reached the snapshot stream, in order.
    pub note_log: Vec<NoteSnapshot>,
}

/// Extras for [`render_with`]: time travel and recording during an offline render.
#[derive(Default)]
pub struct RenderOptions {
    /// Play this log instead of running the scene's sources.
    pub replay: Option<Replay>,
    /// Observe every ingested sample (e.g. a `DataLogWriter`).
    pub record: Option<SampleSink>,
    /// `(engine seconds, log seconds)`: issue `Seek { to }` when the render reaches that time.
    pub seek_at: Option<(f64, f64)>,
    /// `(engine seconds, speed)`: issue `SetSpeed` when the render reaches that time.
    pub speed_at: Option<(f64, f64)>,
    /// `(engine seconds, command)`: issue any user command when the render reaches that
    /// time (in order). Stems mute every other layer at t = 0 this way.
    pub commands_at: Vec<(f64, UserCommand)>,
}

/// One rendered stem.
#[derive(Debug, Clone, PartialEq)]
pub struct StemFile {
    pub layer: LayerId,
    pub layer_name: String,
    pub path: PathBuf,
    pub stats: RenderStats,
}

/// Render one layer alone: the same deterministic render as the mix, with every other
/// (unmuted) layer muted at t = 0, so the stem starts sample-aligned with the mix and
/// its notes come from the same seed. Layers the scene starts muted are left muted.
///
/// Stems are solo renders, not a decomposition of the mix: the master chain (compressor,
/// limiter, saturation) is nonlinear, and a muted layer stops feeding the mood, so the
/// stems do not sum sample-exactly to the mix.
pub fn render_stem(scene: &Scene, seconds: f64, seed: u64, layer: LayerId) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    let commands_at = scene
        .layers
        .iter()
        .enumerate()
        .filter(|(i, l)| *i != layer.0 as usize && !l.muted)
        .map(|(i, _)| (0.0, UserCommand::ToggleMute { layer: LayerId(i as u16) }))
        .collect();
    render_with(scene, seconds, seed, RenderOptions { commands_at, ..Default::default() })
}

/// Render every layer's stem to `<dir>/<prefix>-<layer>.wav` (layer names slugged; a
/// duplicate name gets its index appended). N renders for N layers, run on up to
/// `min(N, available_parallelism, 4)` threads at once — each render is an independent
/// single-thread deterministic job, and the cap bounds peak memory (a 10-minute stereo
/// render is ~230 MB of f32 before it is written).
pub fn render_stems_to_wav(scene: &Scene, seconds: f64, seed: u64, dir: &Path, prefix: &str) -> anyhow::Result<Vec<StemFile>> {
    if let Err(errs) = crate::validate(scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let n = scene.layers.len();
    let mut names: Vec<String> = scene.layers.iter().map(|l| slug(&l.name)).collect();
    for i in 0..n {
        if names.iter().filter(|x| **x == names[i]).count() > 1 {
            names[i] = format!("{}-{i}", names[i]);
        }
    }
    let jobs: Vec<(LayerId, PathBuf)> = (0..n).map(|i| (LayerId(i as u16), dir.join(format!("{prefix}-{}.wav", names[i])))).collect();
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1).clamp(1, 4).min(n.max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<anyhow::Result<RenderStats>>>> = (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((layer, path)) = jobs.get(i) else { break };
                let res = render_stem(scene, seconds, seed, *layer).and_then(|(samples, mut stats)| {
                    write_wav(path, &samples, stats.sample_rate)?;
                    stats.frames = samples.len() / 2;
                    Ok(stats)
                });
                *results[i].lock().unwrap() = Some(res);
            });
        }
    });
    let mut out = Vec::with_capacity(n);
    for (i, (layer, path)) in jobs.into_iter().enumerate() {
        let stats = results[i].lock().unwrap().take().expect("every job ran").with_context(|| format!("stem `{}`", scene.layers[i].name))?;
        out.push(StemFile { layer, layer_name: scene.layers[i].name.clone(), path, stats });
    }
    Ok(out)
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
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "layer".into() } else { out }
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec { channels: 2, sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    Ok(())
}

/// Render `seconds` of a scene to a 16-bit stereo WAV at 48 kHz.
pub fn render_to_wav(scene: &Scene, seconds: f64, seed: u64, path: &Path) -> anyhow::Result<RenderStats> {
    render_to_wav_with(scene, seconds, seed, path, RenderOptions::default())
}

/// [`render_to_wav`] with [`RenderOptions`] (replay, recording).
pub fn render_to_wav_with(scene: &Scene, seconds: f64, seed: u64, path: &Path, opts: RenderOptions) -> anyhow::Result<RenderStats> {
    let (samples, mut stats) = render_with(scene, seconds, seed, opts)?;
    write_wav(path, &samples, stats.sample_rate)?;
    stats.frames = samples.len() / 2;
    Ok(stats)
}

/// Render to interleaved stereo f32 in memory.
pub fn render_to_samples(scene: &Scene, seconds: f64, seed: u64) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    render_with(scene, seconds, seed, RenderOptions::default())
}

/// [`render_to_samples`] with a replay source, a sample recorder and scripted seeks. With
/// a replay the scene's own sources are not run: the log feeds them in lockstep, exactly
/// as the live engine would, tick for tick.
pub fn render_with(scene: &Scene, seconds: f64, seed: u64, opts: RenderOptions) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    if let Err(errs) = crate::validate(scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build()?;
    rt.block_on(render_inner(scene, seconds, seed, opts))
}

async fn render_inner(scene: &Scene, seconds: f64, seed: u64, opts: RenderOptions) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    const SR: u32 = 48_000;
    let max_layers = scene.layers.len().max(1);
    let (controller, mut host) = sonder_audio::synth_pair(SR as f32, max_layers, None);
    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let clock = Arc::new(VirtualClock::new());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let mut control = ControlLoop::new(scene.clone(), seed, controller, None, None, "offline".into(), snapshot_tx)?;
    if let Some(r) = opts.replay {
        control.set_replay(Some(r));
    }
    if let Some(sink) = opts.record {
        control.add_sample_sink(sink);
    }
    let tasks = if control.replay().is_some() {
        Vec::new()
    } else {
        let registry = crate::live::registry();
        let specs = ControlLoop::source_specs(scene);
        spawn_sources(&registry, &specs, sample_tx, clock_dyn, shutdown_rx, seed)?
    };
    let mut seek_at = opts.seek_at;
    let mut speed_at = opts.speed_at;
    let mut commands_at = opts.commands_at.into_iter().peekable();

    let frames_per_tick = (SR as f64 * TICK_SECONDS).round() as usize;
    let ticks = (seconds / TICK_SECONDS).ceil() as usize;
    let mut out = Vec::with_capacity(ticks * frames_per_tick * 2);
    let mut block = vec![0.0f32; frames_per_tick * 2];
    let mut peak = 0.0f32;
    let mut sumsq = 0.0f64;
    let mut notes = 0usize;
    let mut events = 0usize;
    let mut last_event_at = -1.0;
    let mut last_note_at = -1.0;
    let mut captions: Vec<Caption> = Vec::new();
    let mut event_log: Vec<EventSnapshot> = Vec::new();
    let mut note_log: Vec<NoteSnapshot> = Vec::new();

    for i in 0..ticks {
        let t = i as f64 * TICK_SECONDS;
        clock.set(t);
        if seek_at.is_some_and(|(at, _)| t + 1e-9 >= at) {
            control.command(UserCommand::Seek { to: seek_at.take().map(|(_, to)| to).unwrap_or(0.0) });
        }
        if speed_at.is_some_and(|(at, _)| t + 1e-9 >= at) {
            control.command(UserCommand::SetSpeed { speed: speed_at.take().map(|(_, s)| s).unwrap_or(1.0) });
        }
        while commands_at.peek().is_some_and(|(at, _)| t + 1e-9 >= *at) {
            let (_, cmd) = commands_at.next().expect("peeked");
            control.command(cmd);
        }
        // Let source timers fire and tasks run until they're all waiting again.
        tokio::time::advance(std::time::Duration::from_secs_f64(TICK_SECONDS)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        while let Ok(s) = sample_rx.try_recv() {
            control.ingest(s);
        }
        control.tick(t);
        // Pump the synth: commands are consumed inside render.
        host.render(&mut block);
        for &s in &block {
            peak = peak.max(s.abs());
            sumsq += (s as f64) * (s as f64);
        }
        out.extend_from_slice(&block);
        // Count notes/events via the snapshot stream (cheap: watch holds the latest).
        let snap = snapshot_rx.borrow();
        // Rings are stamped in engine time, which is monotonic even across a replay seek
        // (a seek clears them; new entries are newer than the cursor), so a cursor suffices.
        let fresh_notes = snap.notes.iter().rev().take_while(|n| n.at > last_note_at).count();
        notes += fresh_notes;
        note_log.extend(snap.notes[snap.notes.len() - fresh_notes..].iter().cloned());
        if let Some(n) = snap.notes.last() {
            last_note_at = n.at;
        }
        let fresh_events = snap.events.iter().rev().take_while(|e| e.at > last_event_at).count();
        events += fresh_events;
        event_log.extend(snap.events[snap.events.len() - fresh_events..].iter().cloned());
        if let Some(e) = snap.events.last() {
            last_event_at = e.at;
        }
        // Captions are a bounded ring; collect the ones newer than the last we kept.
        let fresh = snap.captions.iter().rev().take_while(|c| captions.last().is_none_or(|l| l.at != c.at || l.text != c.text)).count();
        captions.extend(snap.captions[snap.captions.len() - fresh..].iter().cloned());
    }
    let _ = shutdown_tx.send(true);
    for t in tasks {
        t.abort();
    }
    let frames = out.len() / 2;
    let rms = if out.is_empty() { 0.0 } else { (sumsq / out.len() as f64).sqrt() as f32 };
    Ok((out, RenderStats { seconds, sample_rate: SR, frames, peak, rms, notes, events, seed, captions, event_log, note_log }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_deterministic_and_audible() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let (a, sa) = render_to_samples(&scene, 12.0, 42).unwrap();
        let (b, _) = render_to_samples(&scene, 12.0, 42).unwrap();
        assert_eq!(a.len(), b.len());
        assert!(a == b, "same seed must render identical audio");
        assert!(sa.peak > 0.01, "render should not be silent: {sa:?}");
        assert!(sa.peak <= 1.0);
        assert!(a.iter().all(|s| s.is_finite()));
        assert!(!sa.captions.is_empty(), "the first car should have been captioned");
        assert!(sa.captions.windows(2).all(|w| w[0].at <= w[1].at));
        assert!(sa.captions.iter().any(|c| c.text.starts_with("car in lane")), "{:?}", sa.captions);
    }

    /// Three layers, 3 s: every stem is audible, stems differ from each other, each one is
    /// a solo (the stem's note log names only its own layer), and the mix is untouched.
    #[test]
    fn stems_are_solo_renders_and_leave_the_mix_alone() {
        let scene = crate::parse_scene(
            r#"
name = "Three"
[conductor]
tempo = 120
[[layer]]
name = "pad"
role = "pad"
instrument = "pad.glass"
[[layer]]
name = "bass"
role = "bass"
instrument = "bass.sub"
map.density = "1"
[[layer]]
name = "arp"
role = "arp"
instrument = "arp.glass"
map.density = "1"
"#,
        )
        .unwrap();
        let (mix, _) = render_to_samples(&scene, 3.0, 5).unwrap();
        let (mix_again, _) = render_to_samples(&scene, 3.0, 5).unwrap();
        assert_eq!(mix, mix_again);
        let mut stems = Vec::new();
        for i in 0..3u16 {
            let (samples, stats) = render_stem(&scene, 3.0, 5, LayerId(i)).unwrap();
            assert_eq!(samples.len(), mix.len());
            assert!(stats.peak > 0.005, "stem {i} is silent: {stats:?}");
            assert!(stats.note_log.iter().all(|n| n.layer == LayerId(i)), "stem {i} has other layers' notes: {:?}", stats.note_log);
            assert!(!stats.note_log.is_empty(), "stem {i} played nothing");
            stems.push(samples);
        }
        assert!(stems[0] != stems[1] && stems[1] != stems[2]);
        // On disk, named after the layers.
        let dir = std::env::temp_dir().join(format!("sonder-stems-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = render_stems_to_wav(&scene, 1.0, 5, &dir, "2026-01-01").unwrap();
        let names: Vec<String> = files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["2026-01-01-pad.wav", "2026-01-01-bass.wav", "2026-01-01-arp.wav"]);
        assert!(files.iter().all(|f| f.path.exists() && f.stats.frames == 48_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_seeds_differ() {
        let scene = crate::load_scene("midnight-walk").unwrap();
        let (a, _) = render_to_samples(&scene, 6.0, 1).unwrap();
        let (b, _) = render_to_samples(&scene, 6.0, 2).unwrap();
        assert!(a != b);
    }
}
