//! End-to-end tests of the `sonder` binary (offline subcommands only).

use std::path::PathBuf;
use std::process::Command;

fn sonder() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sonder"))
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = sonder().args(args).output().expect("spawn sonder");
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sonder-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn list_shows_every_builtin_scene() {
    let (ok, out, _) = run(&["list"]);
    assert!(ok);
    for name in ["night-freeway", "planet-tonight", "this-machine", "midnight-walk", "orbit"] {
        assert!(out.contains(name), "missing {name} in:\n{out}");
    }
}

#[test]
fn every_builtin_scene_validates() {
    let (_, out, _) = run(&["list"]);
    let names: Vec<&str> = out.lines().skip(1).filter_map(|l| l.split_whitespace().next()).collect();
    assert!(names.len() >= 9);
    for name in names {
        let (ok, out, err) = run(&["validate", name]);
        assert!(ok, "{name} failed validation:\n{out}{err}");
        assert!(out.starts_with("ok:"), "{out}");
    }
}

#[test]
fn validate_rejects_bad_scene_with_exit_code() {
    let p = scratch("bad.toml");
    std::fs::write(
        &p,
        r#"
name = "bad"
[[layer]]
name = "a"
role = "pad"
signals.x = "norm(smooth(nope(1), 2))"
map.energy = "x"
map.bogus = "1"
"#,
    )
    .unwrap();
    let (ok, _, err) = run(&["validate", p.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("unknown map key `bogus`"), "{err}");
    assert!(err.contains("nope"), "{err}");
}

#[test]
fn render_writes_a_wav_and_is_deterministic() {
    let a = scratch("a.wav");
    let b = scratch("b.wav");
    let (ok, _, err) = run(&["render", "night-freeway", "--seconds", "4", "--seed", "9", "--out", a.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(err.contains("done:"), "{err}");
    let (ok, _, _) = run(&["render", "night-freeway", "--seconds", "4", "--seed", "9", "--out", b.to_str().unwrap()]);
    assert!(ok);
    let da = std::fs::read(&a).unwrap();
    let db = std::fs::read(&b).unwrap();
    // 44-byte header + 4 s × 48000 × 2 ch × 2 bytes
    assert_eq!(da.len(), 44 + 4 * 48_000 * 2 * 2);
    assert_eq!(da, db, "same seed → identical WAV");
    assert!(da.iter().skip(44).any(|&x| x != 0), "render should not be silent");
}

#[test]
fn show_prints_toml_and_sources_lists_kinds() {
    let (ok, out, _) = run(&["show", "night-freeway"]);
    assert!(ok);
    assert!(out.contains("[conductor]") && out.contains("synthetic.freeway"));
    let (ok, out, _) = run(&["sources"]);
    assert!(ok);
    for kind in ["camera.mjpeg", "wikipedia.recent-changes", "usgs.earthquakes", "http-json", "synthetic.freeway"] {
        assert!(out.contains(kind), "missing {kind}");
    }
    let (ok, out, _) = run(&["sources", "synthetic.freeway"]);
    assert!(ok);
    assert!(out.contains("lanes") && out.contains("car"), "{out}");
    let (ok, out, _) = run(&["instruments"]);
    assert!(ok);
    assert!(out.contains("pad.glass") && out.contains("bells.fm"));
}

#[test]
fn source_override_applies_to_render() {
    let p = scratch("override.wav");
    let (ok, _, err) = run(&[
        "render", "night-freeway", "--seconds", "2", "--out", p.to_str().unwrap(),
        "--source", "synthetic.walk", "--param", "names=[\"a\"]", "--param", "event_every=0.5",
    ]);
    assert!(ok, "{err}");
    let (ok, _, err) = run(&["render", "night-freeway", "--seconds", "1", "--out", p.to_str().unwrap(), "--source", "no.such.kind"]);
    assert!(!ok);
    assert!(err.contains("unknown source kind"), "{err}");
}
