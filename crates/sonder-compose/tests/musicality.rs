//! Run a small ensemble for two simulated minutes and check it stays musical.

use std::collections::{BTreeMap, HashMap, HashSet};

use sonder_compose::{Composer, ComposerInput, LayerInput};
use sonder_core::{
    hz_to_midi, map_keys, ConductorConfig, Drift, EngineSnapshot, LayerConfig, LayerEvent, LayerId, NoteId,
    Progression, Role, Scene, SynthCommand, Value,
};

fn scene() -> Scene {
    let mut s = Scene {
        name: "musicality".into(),
        conductor: ConductorConfig {
            key: "D".into(),
            mode: "lydian".into(),
            tempo: 72.0,
            bars_per_chord: 2.0,
            drift: Drift::Slow,
            progression: Progression::Modal,
            mood_smoothing: 5.0,
            humanize: 0.3,
            swing: 0.2,
            ..Default::default()
        },
        ..Default::default()
    };
    s.layers.push(LayerConfig { name: "traffic".into(), role: Role::Pad, ..Default::default() });
    s.layers.push(LayerConfig { name: "low".into(), role: Role::Bass, ..Default::default() });
    let mut car = BTreeMap::new();
    car.insert("pitch".to_string(), "attr('lane') / 3".to_string());
    car.insert("velocity".to_string(), "0.4 + 0.6 * intensity".to_string());
    car.insert("pan".to_string(), "attr('x')".to_string());
    car.insert("quantize".to_string(), "1/16".to_string());
    let mut pluck = LayerConfig { name: "headlights".into(), role: Role::Pluck, ..Default::default() };
    pluck.on_event.insert("car".into(), car);
    s.layers.push(pluck);
    s
}

#[test]
fn two_minutes_of_three_layers_stay_musical() {
    let scene = scene();
    let mut composer = Composer::new(&scene, 2024).unwrap();
    let dt = 0.02;
    let ticks = (120.0 / dt) as usize;

    let mut inputs: Vec<LayerInput> = scene.layers.iter().map(|_| LayerInput::default()).collect();
    let mut snap = EngineSnapshot::default();
    composer.fill_snapshot(&mut snap);
    let mut prev_scale: HashSet<u8> = snap.harmony.scale.iter().copied().collect();

    let mut live: HashMap<NoteId, LayerId> = HashMap::new();
    let mut per_layer_live: HashMap<LayerId, usize> = HashMap::new();
    let mut max_live: HashMap<LayerId, usize> = HashMap::new();
    let mut note_ons = 0usize;
    let mut off_without_on = 0usize;
    let mut out_of_scale = 0usize;
    let mut chord_names: Vec<String> = Vec::new();
    let mut tempos: Vec<f32> = Vec::new();

    // Fake, bursty traffic: ~2 cars/second with waves.
    let mut rng_state: u64 = 99;
    let mut next_rand = move || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        (rng_state % 10_000) as f64 / 10_000.0
    };

    for i in 0..ticks {
        let now = i as f64 * dt;
        let wave = 0.5 + 0.5 * (now * 0.08).sin();
        inputs[0].params.insert(map_keys::ENERGY.into(), wave);
        inputs[0].params.insert(map_keys::BRIGHTNESS.into(), 0.3 + 0.6 * wave);
        inputs[0].params.insert(map_keys::DENSITY.into(), wave);
        inputs[1].params.insert(map_keys::DENSITY.into(), 0.3 + 0.6 * wave);

        let mut events = Vec::new();
        let p_car = 0.02 + 0.08 * wave;
        if next_rand() < p_car {
            let mut attrs = BTreeMap::new();
            attrs.insert("lane".into(), Value::Num((next_rand() * 4.0).floor()));
            attrs.insert("x".into(), Value::Num(next_rand()));
            events.push(LayerEvent { layer: LayerId(2), name: "car".into(), at: now, intensity: next_rand(), attrs });
        }
        if i == 3000 {
            // A firehose tick: 500 events at once must not explode polyphony.
            for k in 0..500 {
                let mut attrs = BTreeMap::new();
                attrs.insert("lane".into(), Value::Num((k % 4) as f64));
                events.push(LayerEvent { layer: LayerId(2), name: "car".into(), at: now, intensity: 0.9, attrs });
            }
        }

        let out = composer.tick(now, dt, &ComposerInput { layers: &inputs, events: &events });
        composer.fill_snapshot(&mut snap);
        let scale: HashSet<u8> = snap.harmony.scale.iter().copied().collect();
        if chord_names.last() != Some(&snap.harmony.chord_name) {
            chord_names.push(snap.harmony.chord_name.clone());
        }

        for cmd in &out.commands {
            match cmd {
                SynthCommand::NoteOn { layer, note, pitch_hz, velocity, pan } => {
                    note_ons += 1;
                    let midi = hz_to_midi(*pitch_hz as f64).round() as i32;
                    let pc = midi.rem_euclid(12) as u8;
                    if !scale.contains(&pc) && !prev_scale.contains(&pc) {
                        out_of_scale += 1;
                    }
                    assert!((0.0..=1.0).contains(velocity) && (0.0..=1.0).contains(pan));
                    assert!((20..=110).contains(&midi), "midi {midi} out of any sane register");
                    live.insert(*note, *layer);
                    let n = per_layer_live.entry(*layer).or_default();
                    *n += 1;
                    let m = max_live.entry(*layer).or_default();
                    *m = (*m).max(*n);
                }
                SynthCommand::NoteOff { note, .. } => match live.remove(note) {
                    Some(layer) => {
                        *per_layer_live.entry(layer).or_default() -= 1;
                    }
                    None => off_without_on += 1,
                },
                SynthCommand::AllNotesOff { layer } => {
                    live.retain(|_, l| l != layer);
                    per_layer_live.insert(*layer, 0);
                }
                SynthCommand::SetTempo { bpm } => tempos.push(*bpm),
                _ => {}
            }
        }
        // Snapshot bookkeeping agrees with what we counted from commands.
        for l in &snap.layers {
            assert_eq!(l.active_notes as usize, *per_layer_live.get(&l.id).unwrap_or(&0), "layer {} at {now:.2}", l.name);
        }
        prev_scale = scale;
    }

    assert_eq!(off_without_on, 0, "every NoteOff must follow a NoteOn");
    assert_eq!(out_of_scale, 0, "every pitch must belong to the current scale");
    assert!(note_ons > 200 && note_ons < 6000, "note count {note_ons} is not reasonable for 2 minutes");
    assert!(chord_names.len() >= 4, "chords should change: {chord_names:?}");
    assert!(max_live[&LayerId(0)] <= 7, "pad polyphony {}", max_live[&LayerId(0)]);
    assert!(max_live[&LayerId(1)] <= 3, "bass polyphony {}", max_live[&LayerId(1)]);
    assert!(max_live[&LayerId(2)] <= 48, "pluck polyphony {}", max_live[&LayerId(2)]);
    assert!(!tempos.is_empty() && tempos.iter().all(|t| (55.0..=95.0).contains(t)));
    assert!(snap.conductor_note.contains("→"), "{}", snap.conductor_note);
    assert_eq!(snap.harmony.chord.len(), snap.harmony.chord.iter().collect::<HashSet<_>>().len());
}
