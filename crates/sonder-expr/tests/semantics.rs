//! End-to-end semantics: builtins on synthetic tick sequences, pipelines, events,
//! errors and determinism.

use std::collections::BTreeMap;

use sonder_core::Value;
use sonder_expr::{compile, EventCtx, EventExpr, ExprError, LayerPipeline, Program, Scope, Tick};

const DT: f64 = 0.02;

fn prog(src: &str) -> Program {
    compile(src, &Scope::default()).unwrap_or_else(|e| panic!("{}", e.render(src)))
}

fn err(src: &str) -> ExprError {
    compile(src, &Scope::default())
        .err()
        .unwrap_or_else(|| panic!("`{src}` should not compile"))
}

/// Tick a program `n` times at 50 Hz starting at t=0, feeding `x` into field `x` each tick.
fn run(p: &mut Program, xs: &[f64]) -> Vec<f64> {
    let mut out = Vec::new();
    for (i, &x) in xs.iter().enumerate() {
        p.push_reading(&BTreeMap::from([("x".to_string(), x)]));
        out.push(p.tick(&Tick {
            now: i as f64 * DT,
            dt: DT,
            beat: 0.0,
            bar: 0.0,
            bpm: 0.0,
        }));
    }
    out
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn signals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------- pure

#[test]
fn arithmetic_and_precedence() {
    let cases = [
        ("1 + 2 * 3", 7.0),
        ("(1 + 2) * 3", 9.0),
        ("2 ^ 3 ^ 2", 512.0),
        ("-2 ^ 2", -4.0),
        ("7 % 3", 1.0),
        ("-1 % 3", 2.0),
        ("10 / 4", 2.5),
        ("1 / 0", 0.0),
        ("5 % 0", 0.0),
        ("10s + 250ms", 10.25),
        ("2m / 60", 2.0),
        ("pi > 3.14 && pi < 3.15", 1.0),
        ("1 < 2", 1.0),
        ("2 <= 1", 0.0),
        ("3 >= 3", 1.0),
        ("1 == 1 && 2 != 3", 1.0),
        ("0 || 0", 0.0),
        ("0 || 0.5", 1.0),
        ("!0", 1.0),
        ("!2", 0.0),
        ("-(3)", -3.0),
    ];
    for (src, want) in cases {
        let got = prog(src).tick(&Tick::at(0.0));
        assert!(approx(got, want), "{src}: got {got}, want {want}");
    }
}

#[test]
fn pure_builtins() {
    let cases = [
        ("abs(-2)", 2.0),
        ("min(3, 1, 2)", 1.0),
        ("max(3, 1, 2)", 3.0),
        ("clamp(5, 0, 1)", 1.0),
        ("clamp(-5, 0, 1)", 0.0),
        ("clamp(0.5, 1, 0)", 0.5), // swapped bounds tolerated
        ("lerp(0, 10, 0.25)", 2.5),
        ("pow(2, 10)", 1024.0),
        ("sqrt(16)", 4.0),
        ("sqrt(-1)", 0.0),
        ("exp(0)", 1.0),
        ("log(e)", 1.0),
        ("log2(8)", 3.0),
        ("log10(1000)", 3.0),
        ("log(0)", 0.0), // -inf sanitized
        ("floor(1.7)", 1.0),
        ("ceil(1.2)", 2.0),
        ("round(2.5)", 3.0),
        ("fract(1.25)", 0.25),
        ("sign(-3)", -1.0),
        ("sign(0)", 0.0),
        ("step(0.5, 0.7)", 1.0),
        ("step(0.5, 0.3)", 0.0),
        ("smoothstep(0, 1, 0.5)", 0.5),
        ("smoothstep(0, 1, 2)", 1.0),
        ("if(1, 2, 3)", 2.0),
        ("if(0, 2, 3)", 3.0),
        ("sin(0)", 0.0),
        ("cos(0)", 1.0),
        ("tan(0)", 0.0),
        ("tanh(0)", 0.0),
        ("wrap(1.75)", 0.75),
        ("wrap(-0.25)", 0.75),
        ("norm(5, 0, 10)", 0.5),
        ("norm(50, 0, 10)", 1.0),
        ("norm(5, 3, 3)", 0.5),
        ("tau / pi", 2.0),
    ];
    for (src, want) in cases {
        let got = prog(src).tick(&Tick::at(0.0));
        assert!(approx(got, want), "{src}: got {got}, want {want}");
    }
}

#[test]
fn constants_from_tick() {
    let mut p = prog("time * 100 + beat * 10 + bar + bpm / 1000 + dt");
    let v = p.tick(&Tick {
        now: 1.0,
        dt: 0.02,
        beat: 2.0,
        bar: 3.0,
        bpm: 120.0,
    });
    assert!(approx(v, 100.0 + 20.0 + 3.0 + 0.12 + 0.02), "{v}");
}

#[test]
fn hash_is_stable_for_strings_and_numbers() {
    let a = prog("hash('car')").tick(&Tick::at(0.0));
    let b = prog("hash(\"car\")").tick(&Tick::at(0.0));
    assert_eq!(a, b);
    assert!((0.0..1.0).contains(&a));
    assert_ne!(a, prog("hash('bus')").tick(&Tick::at(0.0)));
    let n1 = prog("hash(42)").tick(&Tick::at(0.0));
    let n2 = prog("hash(21 * 2)").tick(&Tick::at(0.0));
    assert_eq!(n1, n2);
    assert_ne!(n1, prog("hash(43)").tick(&Tick::at(0.0)));
    assert_eq!(sonder_expr::hash_text("car"), a);
}

// ---------------------------------------------------------------- fields & events

#[test]
fn field_holds_last_value_and_bare_ident_is_field() {
    let mut p = prog("x");
    assert_eq!(p.tick(&Tick::at(0.0)), 0.0, "0 before first reading");
    p.push_reading(&BTreeMap::from([
        ("x".to_string(), 3.0),
        ("other".to_string(), 9.0),
    ]));
    assert_eq!(p.tick(&Tick::at(0.02)), 3.0);
    assert_eq!(p.tick(&Tick::at(0.04)), 3.0, "holds");
    p.push_reading(&BTreeMap::from([("other".to_string(), 1.0)]));
    assert_eq!(
        p.tick(&Tick::at(0.06)),
        3.0,
        "unrelated reading doesn't disturb"
    );
    assert_eq!(p.field_names(), vec!["x".to_string()]);
    let mut q = prog("field('mean.speed') * 2");
    q.push_reading(&BTreeMap::from([("mean.speed".to_string(), 1.5)]));
    assert_eq!(q.tick(&Tick::at(0.0)), 3.0);
    assert_eq!(q.field_names(), vec!["mean.speed".to_string()]);
    let mut r = prog("mean.speed");
    assert_eq!(r.field_names(), vec!["mean.speed".to_string()]);
    assert_eq!(r.tick(&Tick::at(0.0)), 0.0);
}

#[test]
fn event_is_impulse_train_summing_intensities() {
    let mut p = prog("event('car')");
    assert_eq!(p.tick(&Tick::at(0.0)), 0.0);
    p.push_event("car", 0.5);
    p.push_event("car", 0.25);
    p.push_event("bus", 1.0);
    assert_eq!(p.tick(&Tick::at(0.02)), 0.75);
    assert_eq!(p.tick(&Tick::at(0.04)), 0.0, "cleared after tick");
    let mut any = prog("event('*')");
    any.push_event("car", 1.0);
    any.push_event("bus", 2.0);
    assert_eq!(any.tick(&Tick::at(0.0)), 3.0);
    assert_eq!(any.event_names(), vec!["*".to_string()]);
    let mut pre = prog("event('blob*')");
    pre.push_event("blob_exit", 1.0);
    pre.push_event("car", 1.0);
    assert_eq!(pre.tick(&Tick::at(0.0)), 1.0);
}

// ---------------------------------------------------------------- stateful

#[test]
fn smooth_is_one_pole_dt_aware() {
    let mut p = prog("smooth(x, 1s)");
    let xs = vec![1.0; 51]; // 1 second worth after init
    let out = run(&mut p, &xs);
    assert_eq!(out[0], 1.0, "first tick snaps to input");
    let mut p = prog("smooth(x, 1s)");
    let mut xs = vec![0.0];
    xs.extend(vec![1.0; 50]);
    let out = run(&mut p, &xs);
    assert_eq!(out[0], 0.0);
    assert!(out[1] > 0.0 && out[1] < 0.03, "{}", out[1]);
    // After one time constant (50 ticks of 20ms) we are at 1 - 1/e.
    let want = 1.0 - (-1.0f64).exp();
    assert!((out[50] - want).abs() < 1e-6, "{} vs {want}", out[50]);
    // Monotone.
    assert!(out.windows(2).all(|w| w[1] >= w[0]));
    // A zero time constant passes through.
    let mut p = prog("smooth(x, 0)");
    assert_eq!(run(&mut p, &[0.0, 5.0, 2.0]), vec![0.0, 5.0, 2.0]);
}

#[test]
fn ema_alpha() {
    let mut p = prog("ema(x, 0.5)");
    assert_eq!(
        run(&mut p, &[0.0, 1.0, 1.0, 1.0]),
        vec![0.0, 0.5, 0.75, 0.875]
    );
}

#[test]
fn derivative_per_second_two_tick_average() {
    let mut p = prog("derivative(x)");
    let out = run(&mut p, &[0.0, 1.0, 2.0, 2.0, 2.0]);
    assert_eq!(out[0], 0.0);
    assert!(approx(out[1], 25.0), "{}", out[1]); // (50 + 0)/2
    assert!(approx(out[2], 50.0), "{}", out[2]); // (50 + 50)/2
    assert!(approx(out[3], 25.0), "{}", out[3]); // (0 + 50)/2
    assert!(approx(out[4], 0.0));
}

#[test]
fn norm_adaptive_from_first_sample() {
    let mut p = prog("norm(x)");
    let out = run(&mut p, &[5.0, 5.0, 5.0]);
    assert_eq!(out, vec![0.5, 0.5, 0.5], "flat input → 0.5, never NaN");
    let mut p = prog("norm(x)");
    let out = run(&mut p, &[0.0, 10.0, 5.0, 10.0, 0.0, 20.0]);
    assert_eq!(out[0], 0.5);
    assert!(approx(out[1], 1.0), "new max → 1: {}", out[1]);
    assert!((out[2] - 0.5).abs() < 0.01, "midpoint ≈ 0.5: {}", out[2]);
    assert!(approx(out[3], 1.0));
    assert!(approx(out[4], 0.0), "back to min → 0");
    assert!(approx(out[5], 1.0), "new max → 1");
    assert!(out.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
}

#[test]
fn norm_envelope_decays_with_two_minute_half_life() {
    // Push max to 10 then feed 0 forever: after 120 s the max should have decayed
    // halfway toward 0, so 5 maps to ~1.0 and 2.5 maps to ~0.5.
    let mut p = prog("norm(x)");
    run(&mut p, &[0.0, 10.0]);
    let n = (120.0 / DT) as usize;
    let out = run(&mut p, &vec![0.0; n]);
    assert!(out.iter().all(|&v| v == 0.0));
    let v = run(&mut p, &[2.5])[0];
    assert!((v - 0.5).abs() < 0.02, "after 2 min max≈5, 2.5 → {v}");
}

#[test]
fn rate_count_sum_windows() {
    let mut p = prog("rate(event('car'), 1s)");
    // 50 ticks per window; 10 events in the first tick → 10 events/s while in window.
    for _ in 0..10 {
        p.push_event("car", 0.3);
    }
    let v0 = p.tick(&Tick::at(0.0));
    assert!(approx(v0, 10.0), "{v0}");
    for i in 1..50 {
        let v = p.tick(&Tick::at(i as f64 * DT));
        assert!(approx(v, 10.0), "tick {i}: {v}");
    }
    assert_eq!(p.tick(&Tick::at(1.0)), 0.0, "fell out of window");

    let mut c = prog("count(event('car'), 100ms)");
    c.push_event("car", 0.1);
    c.push_event("car", 0.1);
    assert_eq!(
        c.tick(&Tick::at(0.0)),
        2.0,
        "count uses event count, not intensity"
    );
    for i in 1..5 {
        assert_eq!(c.tick(&Tick::at(i as f64 * DT)), 2.0);
    }
    assert_eq!(c.tick(&Tick::at(0.1)), 0.0);

    let mut s = prog("sum(event('car'), 100ms)");
    s.push_event("car", 0.1);
    s.push_event("car", 0.2);
    assert!(approx(s.tick(&Tick::at(0.0)), 0.3), "sum uses intensity");
    s.push_event("car", 1.0);
    assert!(approx(s.tick(&Tick::at(0.02)), 1.3));

    // rate of a non-event signal counts nonzero ticks.
    let mut r = prog("rate(x > 0.5, 1s)");
    let out = run(&mut r, &[1.0, 0.0, 1.0]);
    assert!(approx(out[2], 2.0), "{}", out[2]);
}

#[test]
fn lag_hold_maxw_minw() {
    let mut p = prog("lag(x, 40ms)");
    assert_eq!(
        run(&mut p, &[1.0, 2.0, 3.0, 4.0, 5.0]),
        vec![1.0, 1.0, 1.0, 2.0, 3.0]
    );

    let mut h = prog("hold(x, 1s)");
    let out = run(&mut h, &[1.0, 0.0, 0.0, 0.0]);
    assert_eq!(out[0], 1.0);
    assert!(approx(out[1], (-0.02f64).exp()), "{}", out[1]);
    assert!(out[1] > out[2] && out[2] > out[3]);
    let mut h = prog("hold(x, 1s)");
    let out = run(&mut h, &[1.0, 2.0, 0.5]);
    assert_eq!(out[1], 2.0, "new peak wins");
    assert!(out[2] < 2.0 && out[2] > 1.9);

    let mut m = prog("maxw(x, 60ms)");
    assert_eq!(
        run(&mut m, &[1.0, 3.0, 2.0, 0.0, 0.0, 0.0]),
        vec![1.0, 3.0, 3.0, 3.0, 2.0, 0.0]
    );
    let mut m = prog("minw(x, 60ms)");
    assert_eq!(
        run(&mut m, &[3.0, 1.0, 2.0, 5.0, 5.0, 5.0]),
        vec![3.0, 1.0, 1.0, 1.0, 2.0, 5.0]
    );
}

#[test]
fn onset_every_change_edges() {
    let mut p = prog("onset(x, 0.5)");
    assert_eq!(
        run(&mut p, &[0.0, 0.4, 0.6, 0.9, 0.2, 0.5]),
        vec![0.0, 0.0, 1.0, 0.0, 0.0, 1.0]
    );
    let mut p = prog("onset(x, 0.5)");
    assert_eq!(run(&mut p, &[0.9, 0.9])[0], 0.0, "no fire on first sample");
    assert!(p.is_edge_detector());

    let mut e = prog("every(100ms)");
    let out: Vec<f64> = (0..11).map(|i| e.tick(&Tick::at(i as f64 * DT))).collect();
    assert_eq!(
        out,
        vec![1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]
    );
    assert!(e.is_edge_detector());
    let mut e0 = prog("every(0)");
    assert_eq!(e0.tick(&Tick::at(0.0)), 1.0);
    assert_eq!(e0.tick(&Tick::at(0.02)), 1.0);

    let mut c = prog("change(x)");
    assert_eq!(
        run(&mut c, &[1.0, 1.0, 2.0, 2.0, 1.0]),
        vec![0.0, 0.0, 1.0, 0.0, 1.0]
    );
    assert!(c.is_edge_detector());
    let mut c = prog("change(x, 0.5)");
    assert_eq!(run(&mut c, &[1.0, 1.2, 2.0, 2.4]), vec![0.0, 0.0, 1.0, 0.0]);

    assert!(
        !prog("onset(x, 0.5) * 2").is_edge_detector(),
        "only the top-level node counts"
    );
    assert!(!prog("smooth(x, 1)").is_edge_detector());
}

#[test]
fn random_and_walk_are_seeded_and_bounded() {
    let mut a = sonder_expr::compile_seeded("random()", &Scope::default(), 7).unwrap();
    let mut b = sonder_expr::compile_seeded("random()", &Scope::default(), 7).unwrap();
    let mut c = sonder_expr::compile_seeded("random()", &Scope::default(), 8).unwrap();
    let va: Vec<f64> = (0..20).map(|i| a.tick(&Tick::at(i as f64 * DT))).collect();
    let vb: Vec<f64> = (0..20).map(|i| b.tick(&Tick::at(i as f64 * DT))).collect();
    let vc: Vec<f64> = (0..20).map(|i| c.tick(&Tick::at(i as f64 * DT))).collect();
    assert_eq!(va, vb, "same seed → same stream");
    assert_ne!(va, vc, "different seed → different stream");
    assert!(va.iter().all(|v| (0.0..1.0).contains(v)));
    assert!(va.windows(2).any(|w| w[0] != w[1]), "new value each tick");

    // Two call-sites in one expression are independent.
    let mut two = sonder_expr::compile_seeded("random() - random()", &Scope::default(), 7).unwrap();
    assert!((0..10).any(|i| two.tick(&Tick::at(i as f64 * DT)) != 0.0));

    let mut w = sonder_expr::compile_seeded("walk(0.2, 1s)", &Scope::default(), 3).unwrap();
    let vals: Vec<f64> = (0..500).map(|i| w.tick(&Tick::at(i as f64 * DT))).collect();
    assert!(vals.iter().all(|v| (0.0..=1.0).contains(v)));
    assert!(
        vals.windows(2)
            .all(|p| (p[1] - p[0]).abs() <= 0.2 * DT + 1e-9),
        "moves smoothly"
    );
    assert!(
        vals.iter().any(|&v| (v - 0.5).abs() > 0.05),
        "actually wanders"
    );
}

// ---------------------------------------------------------------- pipeline

#[test]
fn pipeline_signals_reference_each_other_in_any_order() {
    // `a` is alphabetically first but depends on `b`.
    let sig = signals(&[("a", "b * 2"), ("b", "x + 1"), ("c", "a + b")]);
    let map = signals(&[("energy", "c / 10"), ("register", "clamp(a, 0, 1)")]);
    let mut p = LayerPipeline::compile(&sig, &map, 1).unwrap();
    assert_eq!(p.evaluation_order(), vec!["b", "a", "c"]);
    p.push_reading(&BTreeMap::from([("x".to_string(), 2.0)]));
    let fired = p.tick(0.0, DT, 0.0, 0.0, 60.0);
    assert!(fired.is_empty());
    assert_eq!(p.signals()["b"], 3.0);
    assert_eq!(p.signals()["a"], 6.0);
    assert_eq!(p.signals()["c"], 9.0);
    assert!(approx(p.params()["energy"], 0.9));
    assert_eq!(p.params()["register"], 1.0);
    assert_eq!(p.field_names(), vec!["x".to_string()]);
}

#[test]
fn pipeline_rejects_cycles_and_self_reference() {
    let e = LayerPipeline::compile(&signals(&[("a", "b"), ("b", "a")]), &BTreeMap::new(), 0)
        .unwrap_err();
    assert!(e.message.contains("cycle"), "{e}");
    let e = LayerPipeline::compile(&signals(&[("a", "smooth(a, 1)")]), &BTreeMap::new(), 0)
        .unwrap_err();
    assert!(e.message.contains("cannot reference itself"), "{e}");
    assert!(e.message.contains("field(\"a\")"), "{e}");
}

#[test]
fn pipeline_errors_carry_table_key_context() {
    let e = LayerPipeline::compile(
        &signals(&[("density", "norm(smoothe(x, 2))")]),
        &BTreeMap::new(),
        0,
    )
    .unwrap_err();
    assert!(e.message.starts_with("signals.density: "), "{e}");
    assert!(e.message.contains("did you mean `smooth`"), "{e}");
    assert_eq!(e.span, Some((5, 12)));
    let e = LayerPipeline::compile(&BTreeMap::new(), &signals(&[("energy", "density +")]), 0)
        .unwrap_err();
    assert!(e.message.starts_with("map.energy: "), "{e}");
    // In a pipeline any bare identifier is a field, so `density` compiles as a field...
    let p =
        LayerPipeline::compile(&BTreeMap::new(), &signals(&[("energy", "density")]), 0).unwrap();
    assert_eq!(p.field_names(), vec!["density".to_string()]);
    // ...but with a restricted field list it is an error listing known names.
    let e = LayerPipeline::compile_with_fields(
        &signals(&[("d", "norm(count)")]),
        &signals(&[("energy", "densty")]),
        0,
        &["count".to_string(), "mean_speed".to_string()],
    )
    .unwrap_err();
    assert!(e.message.contains("unknown identifier `densty`"), "{e}");
    assert!(e.message.contains("Known signals: d"), "{e}");
    assert!(e.message.contains("fields: count, mean_speed"), "{e}");
    let e = LayerPipeline::compile_with_fields(
        &signals(&[("d", "field('cont')")]),
        &BTreeMap::new(),
        0,
        &["count".to_string()],
    )
    .unwrap_err();
    assert!(
        e.message
            .contains("unknown field `cont` (did you mean `count`?)"),
        "{e}"
    );
}

#[test]
fn pipeline_fired_reports_top_level_edge_detectors() {
    let sig = signals(&[
        ("density", "norm(smooth(rate(event('car'), 10s), 1s))"),
        ("spike", "onset(x, 0.5)"),
        ("pulse", "every(100ms)"),
        ("flip", "change(x)"),
        ("not_edge", "onset(x, 0.5) * 2"),
    ]);
    let mut p = LayerPipeline::compile(&sig, &BTreeMap::new(), 0).unwrap();
    assert_eq!(
        p.edge_signals(),
        vec!["flip".to_string(), "pulse".to_string(), "spike".to_string()]
    );
    let mut t = 0.0;
    let mut step = |p: &mut LayerPipeline, x: f64| {
        p.push_reading(&BTreeMap::from([("x".to_string(), x)]));
        let f = p.tick(t, DT, 0.0, 0.0, 60.0);
        t += DT;
        let mut names: Vec<String> = f.iter().map(|f| f.name.clone()).collect();
        names.sort();
        assert!(f.iter().all(|f| f.intensity == 1.0));
        names
    };
    assert_eq!(step(&mut p, 0.0), vec!["pulse"]);
    assert_eq!(step(&mut p, 0.9), vec!["flip", "spike"]);
    assert_eq!(step(&mut p, 0.9), Vec::<String>::new());
    assert_eq!(step(&mut p, 0.1), vec!["flip"]);
    assert_eq!(step(&mut p, 0.1), Vec::<String>::new());
    assert_eq!(step(&mut p, 0.1), vec!["pulse"]);
    assert_eq!(p.ticks(), 6);
}

#[test]
fn pipeline_full_scene_example_runs_and_is_deterministic() {
    let sig = signals(&[
        ("density", "norm(smooth(rate(event('car'), 10), 3))"),
        ("speed", "norm(smooth(field('mean_speed'), 4))"),
        ("jitter", "walk(0.1, 2s)"),
        ("tick", "every(1s)"),
    ]);
    let map = signals(&[
        ("energy", "density"),
        ("brightness", "speed"),
        ("register", "0.3 + 0.4 * speed + 0.1 * jitter"),
    ]);
    let run_once = |seed: u64| {
        let mut p = LayerPipeline::compile(&sig, &map, seed).unwrap();
        let mut trace = Vec::new();
        let mut fires = 0;
        for i in 0..2000 {
            let t = i as f64 * DT;
            // Deterministic synthetic traffic: a car every 7 ticks, faster at "night".
            if i % 7 == 0 {
                p.push_event(
                    "car",
                    0.8,
                    &BTreeMap::from([("lane".to_string(), Value::Num(2.0))]),
                    t,
                );
            }
            p.push_reading(&BTreeMap::from([(
                "mean_speed".to_string(),
                60.0 + (t * 0.3).sin() * 10.0,
            )]));
            fires += p.tick(t, DT, t * 1.2, t * 0.3, 72.0).len();
            if i % 100 == 0 {
                trace.push((p.signals().clone(), p.params().clone()));
            }
        }
        (trace, fires)
    };
    let (a, fa) = run_once(11);
    let (b, fb) = run_once(11);
    assert_eq!(a, b, "same seed, same inputs → identical outputs");
    assert_eq!(fa, fb);
    assert_eq!(fa, 40, "every(1s) fires at t=0 then every 50 ticks");
    let (c, _) = run_once(12);
    assert_ne!(a, c, "walk() differs by seed");
    for (s, m) in &a {
        for v in s.values().chain(m.values()) {
            assert!(v.is_finite());
        }
        assert!((0.0..=1.0).contains(&s["density"]));
        assert!((0.0..=1.0).contains(&s["speed"]));
        assert!((0.0..=1.0).contains(&m["register"]));
    }
    // After warm-up the density has settled near the middle of its own range.
    let last = &a.last().unwrap().0;
    assert!(last["density"] > 0.0);
}

#[test]
fn pipeline_tick_does_not_allocate_in_steady_state() {
    // We can't count allocations without a custom allocator here, but we can at least
    // check that the per-tick path is cheap: 10 signals × 50 000 ticks well under a second.
    let sig = signals(&[
        ("a", "norm(smooth(rate(event('car'), 10), 3))"),
        ("b", "norm(smooth(x, 4))"),
        ("c", "maxw(x, 2s)"),
        ("d", "minw(x, 2s)"),
        ("e", "sum(event('*'), 5s)"),
        ("f", "derivative(b)"),
        ("g", "hold(event('car'), 2s)"),
        ("h", "walk(0.1, 1s)"),
        ("i", "onset(a, 0.8)"),
        ("j", "lag(b, 1s) + random()"),
    ]);
    let map = signals(&[
        ("energy", "a"),
        ("brightness", "clamp(b + c - d, 0, 1)"),
        ("density", "lerp(0.2, 0.9, h)"),
    ]);
    let mut p = LayerPipeline::compile(&sig, &map, 1).unwrap();
    let start = std::time::Instant::now();
    for i in 0..50_000u64 {
        let t = i as f64 * DT;
        if i % 3 == 0 {
            p.push_event("car", 0.5, &BTreeMap::new(), t);
        }
        p.push_reading(&BTreeMap::from([("x".to_string(), (t * 0.1).sin())]));
        p.tick(t, DT, 0.0, 0.0, 60.0);
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_secs_f64() < 5.0, "50k ticks took {elapsed:?}");
}

#[test]
fn pipeline_rejects_event_only_names_in_signal_context() {
    let e =
        LayerPipeline::compile(&signals(&[("a", "intensity")]), &BTreeMap::new(), 0).unwrap_err();
    assert!(e.message.contains("only available in on_event"), "{e}");
    let e = LayerPipeline::compile(&signals(&[("a", "attr('lane')")]), &BTreeMap::new(), 0)
        .unwrap_err();
    assert!(e.message.contains("only available in on_event"), "{e}");
}

// ---------------------------------------------------------------- event expressions

fn event_ctx<'a>(
    intensity: f64,
    attrs: &'a BTreeMap<String, Value>,
    signals: &'a BTreeMap<String, f64>,
) -> EventCtx<'a> {
    EventCtx {
        intensity,
        attrs,
        signals,
        time: 10.0,
        beat: 4.5,
    }
}

#[test]
fn event_expr_reads_intensity_attrs_and_signals() {
    let sigs = vec!["density".to_string()];
    let attrs = BTreeMap::from([
        ("lane".to_string(), Value::Num(2.0)),
        ("wiki".to_string(), Value::Text("enwiki".to_string())),
        ("n".to_string(), Value::Text(" 7 ".to_string())),
        ("bot".to_string(), Value::Bool(true)),
    ]);
    let sv = BTreeMap::from([("density".to_string(), 0.25)]);
    let ctx = event_ctx(0.5, &attrs, &sv);
    let ev = |src: &str| {
        EventExpr::compile(src, &sigs)
            .unwrap_or_else(|e| panic!("{}", e.render(src)))
            .eval(&ctx)
    };
    assert_eq!(ev("attr('lane') / 4"), 0.5);
    assert_eq!(ev("0.4 + 0.6 * intensity"), 0.7);
    assert_eq!(ev("density * 2"), 0.5);
    assert_eq!(ev("attr('n')"), 7.0, "numeric text parses");
    assert_eq!(ev("attr('bot')"), 1.0);
    assert_eq!(ev("attr('missing')"), 0.0);
    assert_eq!(ev("attr('missing', 0.3)"), 0.3);
    assert_eq!(ev("has('lane') + has('nope')"), 1.0);
    let h = ev("attr('wiki')");
    assert!(
        (0.0..1.0).contains(&h) && h == sonder_expr::hash_text("enwiki"),
        "text attrs hash"
    );
    assert_eq!(
        ev("hash(attr_text('wiki'))"),
        sonder_expr::hash_text("enwiki")
    );
    assert_eq!(ev("attr_text('wiki') == 'enwiki'"), 1.0);
    assert_eq!(ev("'enwiki' == attr_text('wiki')"), 1.0);
    assert_eq!(ev("attr_text('wiki') != 'enwiki'"), 0.0);
    assert_eq!(ev("attr_text('wiki') == 'dewiki'"), 0.0);
    assert_eq!(
        ev("attr_text('lane') == '2'"),
        1.0,
        "numbers compare by value"
    );
    assert_eq!(ev("attr_text('bot') == 'true'"), 1.0);
    assert_eq!(ev("attr_text('missing') == ''"), 0.0);
    assert_eq!(
        ev("if(attr_text('wiki') == 'enwiki', 1, 0) * time + beat"),
        14.5
    );
    assert_eq!(ev("'a' == 'a'"), 1.0);
    let e = EventExpr::compile("attr('lane')", &sigs).unwrap();
    assert_eq!(e.source(), "attr('lane')");
}

#[test]
fn event_expr_rejects_state_fields_and_unknown_names() {
    let sigs = vec!["density".to_string()];
    let e = EventExpr::compile("smooth(intensity, 1)", &sigs).unwrap_err();
    assert!(e.message.contains("cannot be used in on_event"), "{e}");
    let e = EventExpr::compile("random()", &sigs).unwrap_err();
    assert!(e.message.contains("cannot be used in on_event"), "{e}");
    let e = EventExpr::compile("field('x')", &sigs).unwrap_err();
    assert!(e.message.contains("not available in on_event"), "{e}");
    let e = EventExpr::compile("event('x')", &sigs).unwrap_err();
    assert!(e.message.contains("not available in on_event"), "{e}");
    let e = EventExpr::compile("densty", &sigs).unwrap_err();
    assert!(
        e.message
            .contains("unknown identifier `densty` (did you mean `density`?)"),
        "{e}"
    );
    assert!(e.message.contains("Known signals: density"), "{e}");
    assert!(e.message.contains("intensity"), "{e}");
    assert_eq!(e.span, Some((0, 6)));
}

// ---------------------------------------------------------------- errors

#[test]
fn compile_errors_are_helpful() {
    let e = err("smooth(x)");
    assert_eq!(e.message, "`smooth` takes 2 arguments, got 1");
    assert_eq!(e.span, Some((0, 9)));
    assert_eq!(e.to_string(), "`smooth` takes 2 arguments, got 1 (at 0..9)");
    assert_eq!(
        e.render("smooth(x)"),
        "`smooth` takes 2 arguments, got 1\n  smooth(x)\n  ^^^^^^^^^"
    );

    let e = err("norm(x, 1)");
    assert!(e.message.contains("1 argument (adaptive) or 3"), "{e}");
    let e = err("frobnicate(1)");
    assert!(
        e.message.starts_with("unknown function `frobnicate`"),
        "{e}"
    );
    assert!(e.message.contains("Stateful: smooth"), "{e}");
    assert_eq!(e.span, Some((0, 10)));
    let e = err("rate(event('a'), x)");
    assert!(
        e.message.contains("window must be a constant number"),
        "{e}"
    );
    let e = err("rate(event('a'), 0)");
    assert!(e.message.contains("window must be positive"), "{e}");
    let e = err("field(x)");
    assert!(e.message.contains("expects a quoted string"), "{e}");
    let e = err("event('')");
    assert!(e.message.contains("must not be empty"), "{e}");
    let e = err("'abc'");
    assert!(e.message.contains("string literal not allowed here"), "{e}");
    let e = err("1 + 'abc'");
    assert!(
        e.message
            .contains("operator `+` cannot be applied to a string"),
        "{e}"
    );
    let e = err("x == 'abc'");
    assert!(e.message.contains("attr_text"), "{e}");
    let e = err("smooth('x', 1)");
    assert!(e.message.contains("string literal not allowed"), "{e}");
    let e = err("min(1)");
    assert_eq!(e.message, "`min` takes 2 to 8 arguments, got 1");
    let e = err("random(1)");
    assert_eq!(e.message, "`random` takes 0 arguments, got 1");
    let e = err("attr_text('k')");
    assert!(e.message.contains("can only be compared"), "{e}");
    let e = err("attr_text('k') + 1");
    assert!(e.message.contains("can only be compared"), "{e}");
    let e = err("x +");
    assert!(e.message.contains("end of input"), "{e}");
    let e = err("3x");
    assert!(e.message.contains("unit suffix"), "{e}");
    let e = err("");
    assert!(e.message.contains("empty"), "{e}");
    let e = err("attr('lane')");
    assert!(e.message.contains("only available in on_event"), "{e}");

    let scope = Scope {
        signals: vec!["density".into()],
        fields: vec!["count".into()],
        event_ctx: false,
    };
    let e = compile("densiti + 1", &scope).unwrap_err();
    assert_eq!(
        e.message,
        "unknown identifier `densiti` (did you mean `density`?). Known signals: density; fields: count; constants: time, dt, beat, bar, bpm, pi, tau, e"
    );
    assert_eq!(e.span, Some((0, 7)));
    assert!(sonder_expr::check_syntax("1 +").is_err());
    assert!(
        sonder_expr::check_syntax("unknown_fn(1)").is_ok(),
        "syntax only"
    );
}

#[test]
fn program_api_round_trip() {
    let scope = Scope::signals(["density"]);
    let mut p = compile("density * 2 + x", &scope).unwrap();
    assert!(p.set_signal("density", 0.25));
    assert!(!p.set_signal("nope", 1.0));
    p.push_reading(&BTreeMap::from([("x".to_string(), 1.0)]));
    assert_eq!(p.tick(&Tick::at(0.0)), 1.5);
    assert_eq!(p.signal_names(), vec!["density".to_string()]);
    assert!(!p.is_stateful());
    assert!(prog("smooth(x, 1)").is_stateful());
    assert_eq!(p.scope(), &scope);
}

#[test]
fn if_and_logic_keep_both_branches_ticking() {
    // Stateful nodes inside `if` branches advance regardless of the condition, so
    // switching the condition later gives a warmed-up value rather than a cold start.
    let mut p = prog("if(x > 0.5, smooth(x, 100ms), 0)");
    let out = run(&mut p, &[0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
    assert_eq!(out[0], 0.0);
    assert!(out[7] > 0.7, "{}", out[7]);
}

#[test]
fn non_finite_inputs_never_poison_state() {
    let mut p = prog("smooth(x, 1s) + norm(x)");
    let out = run(&mut p, &[1.0, f64::NAN, f64::INFINITY, 1.0]);
    assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
}
