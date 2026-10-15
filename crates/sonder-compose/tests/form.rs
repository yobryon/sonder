//! Run a scene for fifteen simulated minutes and check the form engine, motif
//! memory and call-and-response do what they promise.

use std::collections::BTreeMap;

use sonder_compose::{
    Composer, ComposerInput, LayerInput, ANSWER_REASON, MOTIF_REASON, TRIGGER_REASON,
};
use sonder_core::{
    map_keys, ConductorConfig, EngineSnapshot, FormConfig, FormStyle, LayerConfig, LayerEvent,
    LayerId, Role, Scene, Value,
};

fn scene(style: FormStyle) -> Scene {
    let mut s = Scene {
        name: "form".into(),
        conductor: ConductorConfig {
            key: "D".into(),
            mode: "lydian".into(),
            tempo: 80.0,
            tempo_range: [80.0, 80.0],
            bars_per_chord: 2.0,
            mood_smoothing: 5.0,
            humanize: 0.2,
            form: FormConfig {
                style,
                arc_minutes: 3.0,
                section_bars: 8,
                strength: 0.6,
                call_and_response: true,
                motif_recall: 0.5,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    for (name, role) in [
        ("pad", Role::Pad),
        ("bass", Role::Bass),
        ("lead", Role::Lead),
        ("perc", Role::Perc),
    ] {
        s.layers.push(LayerConfig {
            name: name.into(),
            role,
            ..Default::default()
        });
    }
    let mut car = BTreeMap::new();
    car.insert("pitch".to_string(), "attr('lane') / 3".to_string());
    car.insert("velocity".to_string(), "0.4 + 0.6 * intensity".to_string());
    let mut pluck = LayerConfig {
        name: "headlights".into(),
        role: Role::Pluck,
        ..Default::default()
    };
    pluck.on_event.insert("car".into(), car);
    s.layers.push(pluck);
    s
}

struct Run {
    /// (section name, bars it lasted) in order.
    sections: Vec<(String, u32)>,
    /// Mean density target per section name.
    density_by_section: BTreeMap<String, (f64, usize)>,
    notes: Vec<sonder_core::NoteSnapshot>,
    /// Event time of every salient call we injected.
    calls: Vec<f64>,
    bpm: f64,
    last_snap: EngineSnapshot,
}

fn run(scene: &Scene, seed: u64, minutes: f64) -> Run {
    let mut composer = Composer::new(scene, seed).unwrap();
    let dt = 0.02;
    let ticks = (minutes * 60.0 / dt) as usize;
    let mut inputs: Vec<LayerInput> = scene.layers.iter().map(|_| LayerInput::default()).collect();
    let mut snap = EngineSnapshot::default();
    let mut r = Run {
        sections: Vec::new(),
        density_by_section: BTreeMap::new(),
        notes: Vec::new(),
        calls: Vec::new(),
        bpm: 80.0,
        last_snap: EngineSnapshot::default(),
    };
    let mut cur: Option<(String, u32)> = None;
    let mut rng_state: u64 = 7;
    let mut next_rand = move || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        (rng_state % 10_000) as f64 / 10_000.0
    };
    for i in 0..ticks {
        let now = i as f64 * dt;
        let wave = 0.5 + 0.4 * (now * 0.05).sin();
        inputs[0].params.insert(map_keys::ENERGY.into(), wave);
        inputs[2].params.insert(map_keys::DENSITY.into(), 0.6);
        let mut events = Vec::new();
        // Steady quiet traffic, plus a loud car every ~9 s (a call).
        if next_rand() < 0.06 {
            let loud = i % 450 == 0;
            let mut attrs = BTreeMap::new();
            attrs.insert("lane".into(), Value::Num((next_rand() * 4.0).floor()));
            let intensity = if loud { 0.9 } else { 0.2 + 0.3 * next_rand() };
            if loud {
                r.calls.push(now);
            }
            events.push(LayerEvent {
                layer: LayerId(4),
                name: "car".into(),
                at: now,
                intensity,
                attrs,
            });
        } else if i % 450 == 0 {
            let mut attrs = BTreeMap::new();
            attrs.insert("lane".into(), Value::Num(1.0));
            r.calls.push(now);
            events.push(LayerEvent {
                layer: LayerId(4),
                name: "car".into(),
                at: now,
                intensity: 0.9,
                attrs,
            });
        }
        let out = composer.tick(
            now,
            dt,
            &ComposerInput {
                layers: &inputs,
                events: &events,
            },
        );
        r.notes.extend(out.notes);
        composer.fill_snapshot(&mut snap);
        r.bpm = snap.transport.bpm;
        let name = snap.form.section.clone();
        match &mut cur {
            Some((n, bars)) if *n == name => *bars = snap.form.bar_in_section.max(*bars),
            _ => {
                if let Some(done) = cur.take() {
                    r.sections.push(done);
                }
                cur = Some((name.clone(), 0));
            }
        }
        if let Some(t) = composer.form_targets() {
            let e = r.density_by_section.entry(name).or_insert((0.0, 0));
            e.0 += t.density;
            e.1 += 1;
        }
    }
    if let Some(done) = cur {
        r.sections.push(done);
    }
    r.last_snap = snap;
    r
}

#[test]
fn fifteen_minutes_of_arc_form() {
    let s = scene(FormStyle::Arc);
    let r = run(&s, 11, 15.0);

    // 1. Sections cycle intro → build → peak → release, with plausible lengths.
    let order = ["intro", "build", "peak", "release"];
    assert!(
        r.sections.len() >= 12,
        "only {} sections in 15 minutes: {:?}",
        r.sections.len(),
        r.sections
    );
    for (i, (name, _)) in r.sections.iter().enumerate() {
        assert_eq!(name, order[i % 4], "{:?}", r.sections);
    }
    // 3 minutes at 80 bpm in 4/4 = 60 bars per arc (±30 %); energy ~0.5 ± 0.4
    // hurries intros and builds a little. Every completed section is 3–40 bars.
    for (name, bars) in &r.sections[..r.sections.len() - 1] {
        assert!(
            (3..=40).contains(bars),
            "{name} lasted {bars} bars: {:?}",
            r.sections
        );
    }
    let cycle_bars: Vec<u32> = r
        .sections
        .chunks(4)
        .filter(|c| c.len() == 4)
        .map(|c| c.iter().map(|(_, b)| b + 1).sum())
        .collect();
    for total in &cycle_bars {
        assert!(
            (30..=85).contains(total),
            "arc of {total} bars: {cycle_bars:?}"
        );
    }

    // 2. Density targets: build is denser than release.
    let mean = |k: &str| {
        let (sum, n) = r.density_by_section[k];
        sum / n as f64
    };
    assert!(
        mean("build") > mean("release") + 0.2,
        "build {:.2} release {:.2}",
        mean("build"),
        mean("release")
    );
    assert!(mean("peak") > mean("intro") + 0.4);

    // 3. The lead restated a motif at least once.
    let motif_notes = r.notes.iter().filter(|n| n.reason == MOTIF_REASON).count();
    assert!(motif_notes > 0, "no motif restatement in 15 minutes");

    // 4. Answers follow salient events within 1–2 beats, on the lead, with the call's cause.
    let spb = 60.0 / r.bpm;
    let answers: Vec<_> = r
        .notes
        .iter()
        .filter(|n| n.reason == ANSWER_REASON)
        .collect();
    assert!(
        answers.len() >= 10,
        "{} answers for {} calls",
        answers.len(),
        r.calls.len()
    );
    for n in &answers {
        assert_eq!(n.layer, LayerId(2), "answers come from the lead");
        let cause = n.cause.as_ref().expect("answer without its call");
        assert_eq!(cause.event, "car");
        assert!(cause.attrs.contains_key("lane"));
        let beats = (n.at - cause.at) / spb;
        assert!(
            (0.9..=2.2).contains(&beats),
            "answer {beats:.2} beats after the call"
        );
    }
    // Every injected call was heard (the lead rate-limits to one answer per bar,
    // and calls are 9 s apart, so most get answered).
    let answered_calls = r
        .calls
        .iter()
        .filter(|c| {
            answers
                .iter()
                .any(|n| (n.cause.as_ref().unwrap().at - **c).abs() < 1e-9)
        })
        .count();
    assert!(
        answered_calls * 2 >= r.calls.len(),
        "{answered_calls} of {} calls answered",
        r.calls.len()
    );

    // 5. Event notes carry causes; everything else carries a reason.
    assert!(r.notes.iter().all(|n| !n.reason.is_empty()));
    assert!(r
        .notes
        .iter()
        .filter(|n| n.layer == LayerId(4))
        .all(|n| n.reason == TRIGGER_REASON && n.cause.is_some()));
    let snap = &r.last_snap;
    assert!(!snap.form.section.is_empty());
    assert!(snap.form.bars_in_section > 0);
    assert!(snap.form.arc > 0.0 && snap.form.arc <= 1.0);
    assert!(
        snap.form.note.contains("answering car"),
        "{}",
        snap.form.note
    );
    assert!(
        snap.conductor_note.contains(&snap.form.section),
        "{}",
        snap.conductor_note
    );
}

#[test]
fn sections_style_alternates_and_strength_zero_leaves_data_alone() {
    let s = scene(FormStyle::Sections);
    let r = run(&s, 3, 6.0);
    let names: Vec<&str> = r.sections.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.len() >= 8, "{names:?}");
    for (i, n) in names.iter().enumerate() {
        assert_eq!(*n, ["A", "B", "A'", "B'"][i % 4], "{names:?}");
    }
    for (_, bars) in &r.sections[..r.sections.len() - 1] {
        assert_eq!(*bars, 7, "8-bar sections: {:?}", r.sections);
    }

    // With strength 0 the form only narrates; the lead's density stays as mapped.
    let mut s0 = scene(FormStyle::Arc);
    s0.conductor.form.strength = 0.0;
    let r0 = run(&s0, 3, 1.0);
    let lead = &r0.last_snap.layers[2];
    assert!(
        (lead.params[map_keys::DENSITY] - 0.6).abs() < 1e-9,
        "{:?}",
        lead.params
    );
    let r1 = run(&scene(FormStyle::Arc), 3, 1.0);
    let lead = &r1.last_snap.layers[2];
    assert!(
        (lead.params[map_keys::DENSITY] - 0.6).abs() > 0.05,
        "form should move density: {:?}",
        lead.params
    );
}

#[test]
fn static_form_never_changes_section() {
    let r = run(&scene(FormStyle::Static), 5, 3.0);
    assert_eq!(r.sections.len(), 1);
    assert_eq!(r.sections[0].0, "static");
    assert!(
        r.sections[0].1 >= 55,
        "open-ended sections still count bars: {:?}",
        r.sections
    );
    assert_eq!(r.last_snap.form.bars_in_section, 0);
    assert!(
        r.density_by_section.is_empty(),
        "static publishes no targets"
    );
    // Call-and-response and motifs still work without sections.
    assert!(r.notes.iter().any(|n| n.reason == ANSWER_REASON));
}
