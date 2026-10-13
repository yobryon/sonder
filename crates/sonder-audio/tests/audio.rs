use std::time::{Duration, Instant};

use sonder_core::{LayerId, NoteId, SynthCommand, SynthFeedback};
use sonder_audio::{render_offline, synth_pair, AudioStream, Backend, TapConfig};
use sonder_dsp::presets;

fn note(layer: u16, id: u64, hz: f32) -> SynthCommand {
    SynthCommand::NoteOn { layer: LayerId(layer), note: NoteId(id), pitch_hz: hz, velocity: 0.8, pan: 0.5 }
}

#[test]
fn controller_to_host_round_trip_renders_sound() {
    let (mut ctl, mut host) = synth_pair(48_000.0, 4, Some(TapConfig::default()));
    assert_eq!(ctl.sample_rate(), 48_000.0);
    assert!(ctl.send(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("lead.sine").unwrap()) }));
    assert!(ctl.send(note(0, 1, 440.0)));
    let mut tap = ctl.take_tap().expect("tap configured");
    assert!(ctl.take_tap().is_none());

    let mut out = vec![0.0f32; 512 * 2];
    for _ in 0..40 {
        host.render(&mut out);
    }
    let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
    assert!(rms > 0.01, "rms {rms}");
    assert!(ctl.master_meter().peak > 0.0);
    assert!(ctl.layer_meter(LayerId(0)).rms > 0.0);
    assert_eq!(ctl.layer_meter(LayerId(1)).rms, 0.0);
    assert_eq!(ctl.active_voices(), 1);
    assert_eq!(ctl.drops(), 0);
    assert_eq!(ctl.underruns(), 0);

    // Tap received everything rendered so far.
    let mut tapped = Vec::new();
    let n = tap.pop_chunk(&mut tapped);
    assert_eq!(n, 40 * 512 * 2);
    assert!(tapped.iter().any(|s| s.abs() > 0.01));

    // Release → NoteEnded feedback arrives.
    ctl.send(SynthCommand::NoteOff { layer: LayerId(0), note: NoteId(1) });
    for _ in 0..200 {
        host.render(&mut out);
    }
    let mut ended = false;
    ctl.drain_feedback(|f| ended |= matches!(f, SynthFeedback::NoteEnded { layer: LayerId(0), note: NoteId(1) }));
    assert!(ended);

    // Instrument boxes come back for the control side to free.
    ctl.send(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("pad.warm").unwrap()) });
    host.render(&mut out);
    assert!(ctl.take_recycled().is_some());
}

#[test]
fn full_ring_counts_drops() {
    let (mut ctl, _host) = synth_pair(48_000.0, 1, None);
    let mut ok = 0;
    for i in 0..10_000u64 {
        if ctl.send(note(0, i, 440.0)) {
            ok += 1;
        }
    }
    assert!((8000..10_000).contains(&ok));
    assert_eq!(ctl.drops(), 10_000 - ok);
}

#[test]
fn null_backend_runs_in_real_time_and_stops_on_drop() {
    let (mut ctl, host) = synth_pair(48_000.0, 2, Some(TapConfig { capacity_seconds: 1.0 }));
    ctl.send(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("drone.sub").unwrap()) });
    ctl.send(note(0, 1, 110.0));
    let mut tap = ctl.take_tap().unwrap();
    let stream = AudioStream::start(Backend::Null, host).expect("null backend starts");
    assert_eq!(stream.sample_rate(), 48_000);
    assert_eq!(stream.backend_name(), "null");
    let start = Instant::now();
    let mut tapped = Vec::new();
    while start.elapsed() < Duration::from_millis(400) {
        tap.pop_chunk(&mut tapped);
        std::thread::sleep(Duration::from_millis(20));
    }
    tap.pop_chunk(&mut tapped);
    let frames = tapped.len() / 2;
    // ~0.4 s at 48 kHz ≈ 19200 frames; allow generous slack for a loaded machine.
    assert!(frames > 8_000 && frames < 48_000, "tapped {frames} frames in 400 ms");
    assert!(ctl.blocks_rendered() > 10);
    assert!(ctl.master_meter().rms > 0.0);
    drop(stream);
    let after = ctl.blocks_rendered();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(ctl.blocks_rendered(), after, "render thread kept running after drop");
    assert!(!ctl.is_connected());
}

#[test]
fn wav_backend_writes_a_readable_file() {
    let dir = std::env::temp_dir().join(format!("sonder-audio-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.wav");
    let (mut ctl, host) = synth_pair(44_100.0, 1, None);
    ctl.send(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("pluck.glass").unwrap()) });
    ctl.send(note(0, 1, 523.25));
    let stream = AudioStream::start(Backend::Wav { path: path.clone(), seconds: 1.5 }, host).unwrap();
    assert_eq!(stream.sample_rate(), 44_100);
    stream.wait();
    let reader = hound::WavReader::open(&path).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 44_100);
    assert_eq!(reader.duration(), 66_150);
    let samples: Vec<i16> = reader.into_samples::<i16>().map(|s| s.unwrap()).collect();
    assert_eq!(samples.len(), 66_150 * 2);
    assert!(samples.iter().any(|s| s.abs() > 500), "wav is silent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offline_render_calls_on_block_in_order() {
    let (mut ctl, mut host) = synth_pair(48_000.0, 1, None);
    ctl.send(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("bells.fm").unwrap()) });
    let mut times = Vec::new();
    let mut sent = false;
    let out = render_offline(&mut host, 0.5, |t| {
        times.push(t);
        if t >= 0.1 && !sent {
            sent = true;
            ctl.send(note(0, 1, 880.0));
        }
    });
    assert_eq!(out.len(), 24_000 * 2);
    assert_eq!(times.len(), 24_000 / 256 + 1);
    assert!(times.windows(2).all(|w| w[1] > w[0]));
    // Silent before the note, sound after.
    assert!(out[..(0.09 * 48_000.0) as usize * 2].iter().all(|s| *s == 0.0));
    assert!(out[(0.2 * 48_000.0) as usize * 2..].iter().any(|s| s.abs() > 0.01));
}

#[cfg(feature = "native-audio")]
#[test]
fn native_backend_fails_gracefully_without_a_device() {
    // On CI / WSL there is no sound card: starting must return an error, not panic.
    let (_ctl, host) = synth_pair(48_000.0, 1, None);
    match AudioStream::start(Backend::Native, host) {
        Ok(stream) => {
            // A real device exists here; make sure it stops cleanly.
            assert!(stream.sample_rate() > 0);
            drop(stream);
        }
        Err(e) => {
            let msg = e.to_string();
            assert!(!msg.is_empty());
        }
    }
}
