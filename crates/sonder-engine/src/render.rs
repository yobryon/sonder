//! Offline, deterministic rendering to WAV.
//!
//! Runs sources, control loop and synth in lockstep on one thread inside a paused-time
//! tokio runtime. Each control tick: advance the virtual clock 20 ms, let source tasks
//! run their timers, drain samples, compose, render 20 ms of audio.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use sonder_core::*;

use crate::control::{spawn_sources, ControlLoop};
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
}

/// Render `seconds` of a scene to a 16-bit stereo WAV at 48 kHz.
pub fn render_to_wav(scene: &Scene, seconds: f64, seed: u64, path: &Path) -> anyhow::Result<RenderStats> {
    let (samples, mut stats) = render_to_samples(scene, seconds, seed)?;
    let spec = hound::WavSpec { channels: 2, sample_rate: stats.sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
    for s in &samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)?;
    }
    w.finalize()?;
    stats.frames = samples.len() / 2;
    Ok(stats)
}

/// Render to interleaved stereo f32 in memory.
pub fn render_to_samples(scene: &Scene, seconds: f64, seed: u64) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    if let Err(errs) = crate::validate(scene) {
        anyhow::bail!("scene `{}` is invalid:\n  {}", scene.name, errs.join("\n  "));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build()?;
    rt.block_on(render_inner(scene, seconds, seed))
}

async fn render_inner(scene: &Scene, seconds: f64, seed: u64) -> anyhow::Result<(Vec<f32>, RenderStats)> {
    const SR: u32 = 48_000;
    let max_layers = scene.layers.len().max(1);
    let (controller, mut host) = sonder_audio::synth_pair(SR as f32, max_layers, None);
    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
    let (sample_tx, mut sample_rx) = tokio::sync::mpsc::channel::<Sample>(65_536);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let clock = Arc::new(VirtualClock::new());
    let clock_dyn: Arc<dyn Clock> = clock.clone();
    let registry = crate::live::registry();
    let specs = ControlLoop::source_specs(scene);
    let tasks = spawn_sources(&registry, &specs, sample_tx, clock_dyn, shutdown_rx, seed)?;

    let mut control = ControlLoop::new(scene.clone(), seed, controller, None, None, "offline".into(), snapshot_tx)?;

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

    for i in 0..ticks {
        let t = i as f64 * TICK_SECONDS;
        clock.set(t);
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
        for n in snap.notes.iter().rev() {
            if n.at <= last_note_at {
                break;
            }
            notes += 1;
        }
        if let Some(n) = snap.notes.last() {
            last_note_at = n.at;
        }
        for e in snap.events.iter().rev() {
            if e.at <= last_event_at {
                break;
            }
            events += 1;
        }
        if let Some(e) = snap.events.last() {
            last_event_at = e.at;
        }
    }
    let _ = shutdown_tx.send(true);
    for t in tasks {
        t.abort();
    }
    let frames = out.len() / 2;
    let rms = if out.is_empty() { 0.0 } else { (sumsq / out.len() as f64).sqrt() as f32 };
    Ok((out, RenderStats { seconds, sample_rate: SR, frames, peak, rms, notes, events, seed }))
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
    }

    #[test]
    fn different_seeds_differ() {
        let scene = crate::load_scene("midnight-walk").unwrap();
        let (a, _) = render_to_samples(&scene, 6.0, 1).unwrap();
        let (b, _) = render_to_samples(&scene, 6.0, 2).unwrap();
        assert!(a != b);
    }
}
