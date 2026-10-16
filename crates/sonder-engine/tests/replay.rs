//! Time travel, end to end: record `night-freeway` to a data log while rendering, replay
//! the log through the same scene in lockstep, and get the same events at the same engine
//! times and the same notes. Then seek.

use std::sync::{Arc, Mutex};

use sonder_core::*;
use sonder_engine::datalog::{DataLog, DataLogWriter, LogHeader};
use sonder_engine::render::{render_with, RenderOptions};
use sonder_engine::replay::{Replay, ReplayConfig};

const SECONDS: f64 = 40.0;
const SEED: u64 = 20_261_015;

fn temp_log(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sonder-replay-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Render the scene while recording every ingested sample to `path`.
fn record(scene: &Scene, path: &std::path::Path) -> (Vec<f32>, sonder_engine::RenderStats, usize) {
    let header = LogHeader::new(&scene.name, SEED, 1_760_500_000.0, &sonder_engine::control::ControlLoop::source_specs(scene));
    let writer = Arc::new(Mutex::new(DataLogWriter::create(path, &header).unwrap()));
    let w = writer.clone();
    let opts = RenderOptions { record: Some(Box::new(move |s: &Sample| w.lock().unwrap().write(s))), ..Default::default() };
    let (audio, stats) = render_with(scene, SECONDS, SEED, opts).unwrap();
    let mut w = writer.lock().unwrap();
    w.finish().unwrap();
    let lines = w.lines() as usize;
    (audio, stats, lines)
}

fn rms_db(samples: &[f32]) -> f64 {
    let sum: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    20.0 * (sum / samples.len().max(1) as f64).sqrt().max(1e-9).log10()
}

#[test]
fn replaying_a_recorded_log_reproduces_the_events_and_notes() {
    let scene = sonder_engine::load_scene("night-freeway").unwrap();
    let path = temp_log("night-freeway.sonder.gz");
    let (direct_audio, direct, lines) = record(&scene, &path);
    assert!(direct.events > 5, "the freeway must have produced cars: {direct:?}");
    assert!(lines > 100, "{lines} lines recorded");

    let log = Arc::new(DataLog::open(&path).unwrap());
    assert_eq!(log.header().scene, "Night Freeway");
    assert_eq!(log.header().seed, SEED);
    assert_eq!(log.header().sources.len(), 1);
    assert_eq!(log.header().sources[0].kind, "synthetic.freeway");
    assert!(log.length_seconds() <= SECONDS && log.length_seconds() > SECONDS - 1.0, "{}", log.length_seconds());
    // Readings, events and viz all made it; the recorder was fed in time order.
    assert!(log.entries().iter().any(|e| matches!(e.datum, Datum::Event { .. })));
    assert!(log.entries().iter().any(|e| matches!(e.datum, Datum::Reading { .. })));
    assert!(log.entries().iter().any(|e| matches!(e.datum, Datum::Viz { .. })));
    assert!(log.entries().windows(2).all(|w| w[0].t <= w[1].t));

    // Replay through the same scene with the same seed, in lockstep.
    let replay = Replay::new(log.clone(), "test", ReplayConfig::default());
    let opts = RenderOptions { replay: Some(replay), ..Default::default() };
    let (replay_audio, replayed) = render_with(&scene, SECONDS, SEED, opts).unwrap();

    // Same events (name, layer, intensity) at the same engine time, ±1 tick.
    let pick = |e: &EventSnapshot| (e.layer, e.name.clone(), (e.intensity * 1e6).round() as i64);
    assert_eq!(direct.event_log.len(), replayed.event_log.len(), "event count differs:\n{:?}\n{:?}", direct.event_log, replayed.event_log);
    for (a, b) in direct.event_log.iter().zip(&replayed.event_log) {
        assert_eq!(pick(a), pick(b));
        assert!((a.at - b.at).abs() <= sonder_engine::TICK_SECONDS + 1e-9, "event `{}` at {} vs {}", a.name, a.at, b.at);
    }
    // Same notes.
    assert_eq!(direct.notes, replayed.notes, "note count differs");
    assert_eq!(direct.note_log.len(), replayed.note_log.len());
    for (a, b) in direct.note_log.iter().zip(&replayed.note_log) {
        assert_eq!((a.layer, a.midi.to_bits(), a.velocity.to_bits()), (b.layer, b.midi.to_bits(), b.velocity.to_bits()));
        assert!((a.at - b.at).abs() <= sonder_engine::TICK_SECONDS + 1e-9);
    }
    // And the same loudness, give or take a dB.
    assert_eq!(direct_audio.len(), replay_audio.len());
    let (da, ra) = (rms_db(&direct_audio), rms_db(&replay_audio));
    assert!((da - ra).abs() < 1.0, "direct {da:.2} dB vs replay {ra:.2} dB");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn seeking_moves_the_position_and_notes_resume() {
    let scene = sonder_engine::load_scene("night-freeway").unwrap();
    let path = temp_log("seek.sonder");
    let (_, _, _) = record(&scene, &path);
    let log = Arc::new(DataLog::open(&path).unwrap());

    // Play for 20 s, then seek to 25 s into the log; render 40 s in all.
    const SEEK_AT: f64 = 20.0;
    const SEEK_TO: f64 = 25.0;
    const TOTAL: f64 = 40.0;
    let replay = Replay::new(log.clone(), "seek", ReplayConfig::default());
    let opts = RenderOptions { replay: Some(replay), seek_at: Some((SEEK_AT, SEEK_TO)), ..Default::default() };
    let (_, stats) = render_with(&scene, TOTAL, SEED, opts).unwrap();

    // What the log holds decides what we should have heard: its events before the seek
    // point, then the ones from `SEEK_TO` on for the remaining engine time.
    let log_events = |lo: f64, hi: f64| log.entries().iter().filter(|e| matches!(e.datum, Datum::Event { .. }) && e.t >= lo && e.t < hi).count();
    let want_before = log_events(0.0, SEEK_AT);
    let want_after = log_events(SEEK_TO, SEEK_TO + (TOTAL - SEEK_AT));
    assert!(want_after > 0, "pick a seek window with cars in it");
    let before = stats.event_log.iter().filter(|e| e.at < SEEK_AT).count();
    let after = stats.event_log.iter().filter(|e| e.at >= SEEK_AT).count();
    assert_eq!(before, want_before, "events before the seek");
    assert_eq!(after, want_after, "events after the seek (log {SEEK_TO}..)");
    // Notes keep coming after the seek (the composer was rebuilt and is playing again).
    let notes_after = stats.note_log.iter().filter(|n| n.at >= SEEK_AT + 0.5).count();
    assert!(notes_after > 0, "no notes after the seek: {:?}", stats.note_log);

    // Position is observable: replay the same way through a control loop and read the snapshot.
    let (controller, mut host) = sonder_audio::synth_pair(48_000.0, scene.layers.len(), None);
    let (tx, rx) = tokio::sync::watch::channel(Arc::new(EngineSnapshot::default()));
    let mut control = sonder_engine::control::ControlLoop::new(scene.clone(), SEED, controller, None, None, "test".into(), tx).unwrap();
    control.set_replay(Some(Replay::new(log.clone(), "seek", ReplayConfig::default())));
    let mut block = vec![0.0f32; 960 * 2];
    let mut t = 0.0;
    for _ in 0..100 {
        control.tick(t);
        host.render(&mut block);
        t += sonder_engine::TICK_SECONDS;
    }
    let snap = rx.borrow().clone();
    let r = snap.replay.clone().expect("replay state in the snapshot");
    assert_eq!(r.title, "seek");
    assert_eq!(r.scene, "Night Freeway");
    assert_eq!(r.speed, 1.0);
    assert!((r.position - 2.0).abs() < 0.05, "{}", r.position);
    assert!((r.length - log.length_seconds()).abs() < 1e-9);
    assert!(snap.sources.iter().all(|s| s.message.starts_with("replay")), "{:?}", snap.sources);

    control.command(UserCommand::Seek { to: 10.0 });
    control.command(UserCommand::SetSpeed { speed: 2.0 });
    for _ in 0..50 {
        control.tick(t);
        host.render(&mut block);
        t += sonder_engine::TICK_SECONDS;
    }
    let r = rx.borrow().replay.clone().unwrap();
    assert_eq!(r.speed, 2.0);
    assert!((r.position - 12.0).abs() < 0.1, "1 s of engine time at 2× after seeking to 10 s: {}", r.position);
    assert!(rx.borrow().captions.iter().any(|c| c.text.starts_with('⟲')), "seek is captioned");
    // Past the end it stops and holds.
    for _ in 0..1600 {
        control.tick(t);
        host.render(&mut block);
        t += sonder_engine::TICK_SECONDS;
    }
    let r = rx.borrow().replay.clone().unwrap();
    assert!((r.position - r.length).abs() < 1e-9, "{} vs {}", r.position, r.length);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_scene_can_declare_a_replay_log_source() {
    let scene = sonder_engine::load_scene("night-freeway").unwrap();
    let path = temp_log("declared.sonder");
    record(&scene, &path);
    let toml = format!(
        r#"name = "From a log"
[[layer]]
name = "traffic"
role = "pad"
instrument = "pad.glass"
source = {{ kind = "replay.log", path = {path:?}, speed = 4.0, from = 5.0 }}
signals.flow = "norm(smooth(rate(event('*'), 20), 4))"
map.energy = "flow"

[[layer]]
name = "headlights"
role = "pluck"
instrument = "pluck.glass"
source = "traffic"
on_event."*".pitch = "attr('x')"
"#
    );
    let scene = sonder_engine::parse_scene(&toml).unwrap();
    sonder_engine::validate(&scene).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(sonder_engine::control::ControlLoop::source_specs(&scene).is_empty(), "replay.log is not an adapter kind");
    let (_, stats) = render_with(&scene, 4.0, 1, RenderOptions::default()).unwrap();
    assert!(stats.events > 0, "the declared log fed the scene: {stats:?}");
    // Bad parameters are validation errors.
    let bad = toml.replace("speed = 4.0", "speed = -1");
    let errs = sonder_engine::validate(&sonder_engine::parse_scene(&bad).unwrap()).unwrap_err();
    assert!(errs.iter().any(|e| e.contains("speed")), "{errs:?}");
    let _ = std::fs::remove_file(&path);
}
