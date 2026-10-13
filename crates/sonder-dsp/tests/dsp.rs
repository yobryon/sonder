//! Signal-level sanity tests for the synth, effects and presets.

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use sonder_core::{Adsr, Chorus, DelaySettings, Effects, Filter, FilterKind, Instrument, LayerId, MasterSettings, NoteId, Osc, ReverbSettings, SynthCommand, Table, Wave};
use sonder_dsp::master::MasterChain;
use sonder_dsp::wavetable::{self, MIPS};
use sonder_dsp::reverb::Plate;
use sonder_dsp::{presets, Synth};

const SR: f32 = 48_000.0;

fn render_seconds(synth: &mut Synth, seconds: f32) -> Vec<f32> {
    let n = (seconds * SR) as usize;
    let mut out = vec![0.0f32; n * 2];
    for chunk in out.chunks_mut(256 * 2) {
        synth.render(chunk);
    }
    out
}

fn rms(buf: &[f32]) -> f32 {
    (buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32).sqrt()
}

fn peak(buf: &[f32]) -> f32 {
    buf.iter().fold(0f32, |m, s| m.max(s.abs()))
}

fn set_instrument(synth: &mut Synth, layer: u16, inst: Instrument) {
    synth.handle(SynthCommand::SetInstrument { layer: LayerId(layer), instrument: Box::new(inst) });
}

fn note_on(synth: &mut Synth, layer: u16, id: u64, hz: f32, vel: f32) {
    synth.handle(SynthCommand::NoteOn { layer: LayerId(layer), note: NoteId(id), pitch_hz: hz, velocity: vel, pan: 0.5 });
}

fn note_off(synth: &mut Synth, layer: u16, id: u64) {
    synth.handle(SynthCommand::NoteOff { layer: LayerId(layer), note: NoteId(id) });
}

/// A dry sine instrument (no filter, no sends, instant envelope).
fn dry_sine() -> Instrument {
    Instrument {
        osc: vec![Osc { wave: Wave::Sine, ..Osc::default() }],
        filter: Filter { kind: FilterKind::Off, ..Filter::default() },
        amp_env: Adsr { attack: 0.002, decay: 0.1, sustain: 1.0, release: 0.05 },
        reverb_send: 0.0,
        delay_send: 0.0,
        gain: 0.5,
        velocity_to_amp: 0.0,
        ..Instrument::default()
    }
}

fn no_effects() -> Effects {
    Effects {
        reverb: ReverbSettings { level: 0.0, ..ReverbSettings::default() },
        delay: DelaySettings { level: 0.0, ..DelaySettings::default() },
        master: MasterSettings { gain: 1.0, saturation: 0.0, ceiling_db: -0.1 },
    }
}

#[test]
fn level_sanity_and_decay() {
    let mut synth = Synth::new(SR, 4);
    set_instrument(&mut synth, 0, presets::get("pad.glass").unwrap());
    set_instrument(&mut synth, 1, presets::get("pluck.nylon").unwrap());
    for (i, hz) in [220.0, 277.18, 329.63, 440.0].iter().enumerate() {
        note_on(&mut synth, 0, i as u64, *hz, 0.8);
        note_on(&mut synth, 1, 100 + i as u64, hz * 2.0, 0.9);
    }
    let sounding = render_seconds(&mut synth, 2.0);
    assert!(sounding.iter().all(|s| s.is_finite()), "NaN/inf in output");
    assert!(peak(&sounding) <= 1.0);
    let tail_rms = rms(&sounding[sounding.len() / 2..]);
    assert!(tail_rms > 0.01, "notes should sound: rms {tail_rms}");
    assert!(synth.master_meter().rms > 0.0);
    assert!(synth.layer_meter(LayerId(0)).peak > 0.0);
    for i in 0..4 {
        note_off(&mut synth, 0, i);
        note_off(&mut synth, 1, 100 + i);
    }
    // Release (2.5 s) + reverb tail: after 10 s the output should be essentially silent.
    let tail = render_seconds(&mut synth, 10.0);
    assert!(tail.iter().all(|s| s.is_finite()));
    let last = &tail[tail.len() - (SR as usize) * 2..];
    assert!(rms(last) < 1e-3, "tail did not decay: rms {}", rms(last));
    assert_eq!(synth.active_voices(), 0);
    let mut ended = 0;
    synth.drain_feedback(&mut |f| {
        if matches!(f, sonder_core::SynthFeedback::NoteEnded { .. }) {
            ended += 1
        }
    });
    assert_eq!(ended, 8);
}

#[test]
fn sine_pitch_is_accurate() {
    let mut synth = Synth::new(SR, 1);
    synth.handle(SynthCommand::SetEffects(Box::new(no_effects())));
    set_instrument(&mut synth, 0, dry_sine());
    note_on(&mut synth, 0, 1, 440.0, 1.0);
    let out = render_seconds(&mut synth, 2.2);
    // Skip the first 100 ms (attack), count rising zero crossings on the left channel over 2 s.
    let start = (0.1 * SR) as usize;
    let end = start + 2 * SR as usize;
    let mut crossings = 0;
    for i in start + 1..end {
        if out[(i - 1) * 2] <= 0.0 && out[i * 2] > 0.0 {
            crossings += 1;
        }
    }
    let hz = crossings as f32 / 2.0;
    assert!((hz - 440.0).abs() < 1.5, "measured {hz} Hz");
}

#[test]
fn polyblep_saw_aliases_less_than_naive() {
    // Render a high saw (which aliases heavily when naive) and compare the
    // energy landing in frequencies that are not harmonics of the fundamental.
    let f0 = 2_793.83f32; // F7
    let n = 1 << 15;
    let mut synth = Synth::new(SR, 1);
    synth.handle(SynthCommand::SetEffects(Box::new(no_effects())));
    set_instrument(&mut synth, 0, Instrument { osc: vec![Osc { wave: Wave::Saw, ..Osc::default() }], ..dry_sine() });
    note_on(&mut synth, 0, 1, f0, 1.0);
    let out = render_seconds(&mut synth, (n as f32 + 4800.0) / SR);
    let blep: Vec<f32> = out.iter().skip(4800 * 2).step_by(2).take(n).copied().collect();
    let mut naive = Vec::with_capacity(n);
    let mut phase = 0.0f32;
    for _ in 0..n {
        naive.push(2.0 * phase - 1.0);
        phase += f0 / SR;
        if phase >= 1.0 {
            phase -= 1.0;
        }
    }
    let ratio = |sig: &[f32]| {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let mut buf: Vec<Complex<f32>> = sig
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos();
                Complex::new(s * w, 0.0)
            })
            .collect();
        fft.process(&mut buf);
        let bin_hz = SR / n as f32;
        let (mut harm, mut alias) = (0.0f32, 0.0f32);
        for (k, c) in buf.iter().enumerate().take(n / 2).skip(1) {
            let f = k as f32 * bin_hz;
            let nearest = (f / f0).round() * f0;
            let e = c.norm_sqr();
            if (f - nearest).abs() < 3.0 * bin_hz {
                harm += e;
            } else {
                alias += e;
            }
        }
        alias / harm.max(1e-12)
    };
    let r_blep = ratio(&blep);
    let r_naive = ratio(&naive);
    assert!(r_blep < r_naive * 0.2, "PolyBLEP alias ratio {r_blep} vs naive {r_naive}");
}

#[test]
fn reverb_tail_decays_and_is_finite() {
    let mut plate = Plate::new(SR);
    plate.configure(&ReverbSettings { size: 0.8, damping: 0.4, predelay_ms: 20.0, level: 1.0, lowcut_hz: 100.0 });
    let block = 64;
    let mut input = vec![0.0f32; block * 2];
    let mut out = vec![0.0f32; block * 2];
    // A 10 ms burst of noise.
    let mut seed = 12345u32;
    let mut energies = Vec::new();
    let seconds = 6.0;
    let blocks = (seconds * SR / block as f32) as usize;
    let mut window_sum = 0.0f32;
    let mut window_blocks = 0;
    for b in 0..blocks {
        for s in input.iter_mut() {
            if b * block < (0.01 * SR) as usize {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *s = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
            } else {
                *s = 0.0;
            }
        }
        plate.process(&input, &mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        window_sum += out.iter().map(|s| s * s).sum::<f32>();
        window_blocks += 1;
        // 250 ms windows.
        if window_blocks * block >= (0.25 * SR) as usize {
            energies.push(window_sum / (window_blocks * block * 2) as f32);
            window_sum = 0.0;
            window_blocks = 0;
        }
    }
    assert!(energies[0] > 0.0, "reverb produced no output");
    // Monotonic-ish: after the first window every later window is quieter than
    // the one two windows before it, and the last is far below the first.
    for i in 2..energies.len() {
        assert!(energies[i] < energies[i - 2] * 1.05, "non-decaying tail at window {i}: {:?}", energies);
    }
    let db = 10.0 * (energies[energies.len() - 1] / energies[0]).log10();
    assert!(db < -40.0, "tail only decayed {db} dB in {seconds} s");
    // Roughly exponential: the decay per window in the middle section should be
    // similar from window to window (no single dominant resonance ringing on).
    let ratios: Vec<f32> = (2..8).map(|i| energies[i] / energies[i - 1]).collect();
    let mean = ratios.iter().sum::<f32>() / ratios.len() as f32;
    for r in &ratios {
        assert!((r / mean).log2().abs() < 1.0, "irregular decay {ratios:?}");
    }
}

/// Energy away from the harmonics of `f0`, relative to the energy on them.
fn alias_ratio(sig: &[f32], f0: f32) -> f32 {
    let n = sig.len();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n);
    let mut buf: Vec<Complex<f32>> = sig
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos();
            Complex::new(s * w, 0.0)
        })
        .collect();
    fft.process(&mut buf);
    let bin_hz = SR / n as f32;
    let (mut harm, mut alias) = (0.0f32, 0.0f32);
    for (k, c) in buf.iter().enumerate().take(n / 2).skip(1) {
        let f = k as f32 * bin_hz;
        let nearest = (f / f0).round() * f0;
        let e = c.norm_sqr();
        if (f - nearest).abs() < 3.0 * bin_hz {
            harm += e;
        } else {
            alias += e;
        }
    }
    alias / harm.max(1e-12)
}

#[test]
fn wavetable_mip_selection_reduces_aliasing_at_high_pitch() {
    let f0 = 2_793.83f32; // F7
    let n = 1 << 15;
    let t = wavetable::tables();
    let mip = wavetable::mip_for(f0, SR);
    assert!(mip > 0 && mip < MIPS);
    for table in Table::ALL {
        let render = |level: usize| -> Vec<f32> {
            let mut phase = 0.0f64;
            (0..n)
                .map(|_| {
                    let s = t.sample(table, level, phase);
                    phase += (f0 / SR) as f64;
                    s
                })
                .collect()
        };
        let base = render(0);
        let picked = render(mip);
        assert!(picked.iter().all(|s| s.is_finite()));
        let r_base = alias_ratio(&base, f0);
        let r_mip = alias_ratio(&picked, f0);
        // Tables whose spectrum is already below the mip limit are identical at every level.
        let identical = base.iter().zip(&picked).all(|(a, b)| (a - b).abs() < 1e-6);
        if !identical {
            assert!(r_mip < r_base * 0.5, "{table:?}: mip {mip} alias ratio {r_mip} vs base {r_base}");
        }
        assert!(r_mip < 0.05, "{table:?}: residual alias ratio {r_mip}");
    }
}

#[test]
fn every_wavetable_renders_through_a_voice() {
    for table in Table::ALL {
        let mut synth = Synth::new(SR, 1);
        synth.handle(SynthCommand::SetEffects(Box::new(no_effects())));
        set_instrument(&mut synth, 0, Instrument { osc: vec![Osc { wave: Wave::Wavetable, table, ..Osc::default() }], ..dry_sine() });
        for (i, hz) in [55.0f32, 440.0, 3520.0, 12_000.0].iter().enumerate() {
            note_on(&mut synth, 0, i as u64, *hz, 0.9);
        }
        let out = render_seconds(&mut synth, 0.5);
        assert!(out.iter().all(|s| s.is_finite()), "{table:?} NaN");
        assert!(rms(&out) > 0.01, "{table:?} silent");
    }
}

#[test]
fn chorus_on_a_layer_widens_without_changing_level_much() {
    let run = |chorus: Chorus| -> Vec<f32> {
        let mut synth = Synth::new(SR, 1);
        synth.handle(SynthCommand::SetEffects(Box::new(no_effects())));
        set_instrument(&mut synth, 0, Instrument { osc: vec![Osc { wave: Wave::Saw, ..Osc::default() }], chorus, gain: 0.3, ..dry_sine() });
        note_on(&mut synth, 0, 1, 220.0, 1.0);
        render_seconds(&mut synth, 2.0)
    };
    let dry = run(Chorus::default());
    let wet = run(Chorus { depth: 0.6, rate_hz: 0.8, mix: 0.5, voices: 3 });
    assert!(wet.iter().all(|s| s.is_finite()));
    let half = dry.len() / 2;
    let (rd, rw) = (rms(&dry[half..]), rms(&wet[half..]));
    assert!(rw > rd * 0.5 && rw < rd * 1.6, "chorus level drifted: dry {rd} wet {rw}");
    // Dry render is mono (identical channels); the chorused one is not.
    let side = |b: &[f32]| rms(&b.chunks(2).map(|f| f[0] - f[1]).collect::<Vec<_>>());
    assert!(side(&dry[half..]) < 1e-6);
    assert!(side(&wet[half..]) > 0.01, "chorus should decorrelate channels");
    // Bypassed chorus in the synth equals no chorus at all, bit for bit.
    let bypass = run(Chorus { depth: 0.0, rate_hz: 3.0, mix: 1.0, voices: 3 });
    assert_eq!(bypass, dry);
}

#[test]
fn every_preset_renders_and_round_trips_through_serde() {
    for name in presets::names() {
        let inst = presets::get(name).unwrap();
        let json = serde_json::to_string(&inst).unwrap();
        let back: Instrument = serde_json::from_str(&json).unwrap();
        assert_eq!(back, inst, "{name} serde round trip");
        let mut synth = Synth::new(SR, 1);
        set_instrument(&mut synth, 0, inst);
        for (i, hz) in [110.0f32, 261.63, 523.25].iter().enumerate() {
            note_on(&mut synth, 0, i as u64, *hz, 0.9);
        }
        let out = render_seconds(&mut synth, 1.0);
        assert!(out.iter().all(|s| s.is_finite()), "{name} produced NaN");
        assert!(peak(&out) <= 1.0, "{name} clipped");
        let r = rms(&out[out.len() / 2..]);
        assert!(r > 1e-4, "{name} is silent (rms {r})");
        for i in 0..3 {
            note_off(&mut synth, 0, i);
        }
        let tail = render_seconds(&mut synth, 1.0);
        assert!(tail.iter().all(|s| s.is_finite()), "{name} tail NaN");
    }
}

#[test]
fn limiter_never_exceeds_ceiling() {
    let mut m = MasterChain::new(SR);
    let ceiling_db = -3.0;
    m.configure(&MasterSettings { gain: 4.0, saturation: 0.0, ceiling_db });
    let ceiling = 10f32.powf(ceiling_db / 20.0);
    let mut buf = vec![0.0f32; 48_000 * 2];
    for (i, s) in buf.iter_mut().enumerate() {
        let t = (i / 2) as f32 / SR;
        // Overdriven mix of a sine and bursts.
        *s = (t * 55.0 * std::f32::consts::TAU).sin() * 1.5 + if (t * 3.0).fract() < 0.1 { 2.0 } else { 0.0 };
    }
    m.process(&mut buf);
    let p = peak(&buf);
    assert!(p <= ceiling + 1e-4, "peak {p} exceeds ceiling {ceiling}");
    assert!(p > ceiling * 0.9, "limiter should sit near the ceiling, got {p}");
}

#[test]
fn drums_render_and_end() {
    use sonder_core::Drum;
    let mut synth = Synth::new(SR, 1);
    for (i, d) in [Drum::Kick, Drum::Snare, Drum::Hat, Drum::Click, Drum::Noise, Drum::Tom, Drum::Rim].iter().enumerate() {
        synth.handle(SynthCommand::Drum { layer: LayerId(0), drum: *d, velocity: 0.9, pan: i as f32 / 6.0, tone: 0.5 });
        let out = render_seconds(&mut synth, 0.05);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(peak(&out) > 0.02, "{d:?} is silent");
        assert!(peak(&out) <= 1.0);
        let _ = render_seconds(&mut synth, 1.5);
    }
    assert_eq!(synth.active_drums(), 0);
}

#[test]
fn polyphony_and_stealing() {
    let mut synth = Synth::new(SR, 1);
    set_instrument(&mut synth, 0, Instrument { polyphony: 4, ..presets::get("pad.warm").unwrap() });
    for i in 0..6 {
        note_on(&mut synth, 0, i, 220.0 + 20.0 * i as f32, 0.8);
    }
    let _ = render_seconds(&mut synth, 0.1);
    assert_eq!(synth.active_voices(), 4);
    let mut stolen = 0;
    synth.drain_feedback(&mut |f| {
        if matches!(f, sonder_core::SynthFeedback::NoteStolen { .. }) {
            stolen += 1
        }
    });
    assert_eq!(stolen, 2);
}

#[test]
fn panic_silences_everything() {
    let mut synth = Synth::new(SR, 2);
    set_instrument(&mut synth, 0, presets::get("drone.saw").unwrap());
    note_on(&mut synth, 0, 1, 110.0, 1.0);
    let _ = render_seconds(&mut synth, 1.0);
    synth.handle(SynthCommand::Panic);
    let fade = render_seconds(&mut synth, 0.1);
    assert!(fade.iter().all(|s| s.is_finite()));
    let after = render_seconds(&mut synth, 0.2);
    assert!(peak(&after) < 1e-5, "peak after panic {}", peak(&after));
    assert_eq!(synth.active_voices(), 0);
}

#[test]
fn odd_buffer_lengths_are_handled() {
    let mut synth = Synth::new(SR, 1);
    set_instrument(&mut synth, 0, dry_sine());
    note_on(&mut synth, 0, 1, 440.0, 1.0);
    let mut a = vec![0.0f32; 7];
    synth.render(&mut a);
    let mut b = vec![0.0f32; 1000];
    synth.render(&mut b);
    assert!(a.iter().chain(b.iter()).all(|s| s.is_finite()));
    // Garbage input must not panic.
    note_on(&mut synth, 0, 2, f32::NAN, f32::INFINITY);
    synth.handle(SynthCommand::NoteOn { layer: LayerId(99), note: NoteId(3), pitch_hz: 440.0, velocity: 1.0, pan: 0.5 });
    synth.handle(SynthCommand::SetTempo { bpm: -5.0 });
    synth.render(&mut b);
    assert!(b.iter().all(|s| s.is_finite()));
}
