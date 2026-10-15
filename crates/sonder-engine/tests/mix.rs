//! Mix sanity: every offline-renderable built-in scene lands in a sane loudness window
//! and never clips. Live scenes are skipped (they need the network).

use sonder_core::LayerSource;

fn is_offline(scene: &sonder_core::Scene) -> bool {
    let reg = sonder_engine::live::registry();
    scene.layers.iter().all(|l| match &l.source {
        LayerSource::Spec(s) => reg.info(&s.kind).is_some_and(|i| !i.network),
        _ => true,
    })
}

#[test]
fn offline_scenes_render_within_a_sane_loudness_window() {
    let mut checked = 0;
    for (name, scene) in sonder_engine::builtin_scenes() {
        if !is_offline(&scene) || name == "this-browser" {
            continue;
        }
        let (samples, stats) = sonder_engine::render::render_to_samples(&scene, 45.0, 7).unwrap();
        // Skip the first 10 s (intro) when measuring.
        let skip = 10 * 48_000 * 2;
        let body = &samples[skip.min(samples.len())..];
        let rms = (body.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / body.len().max(1) as f64).sqrt();
        let rms_db = 20.0 * rms.max(1e-9).log10();
        assert!(stats.peak <= 1.0, "{name}: clipped (peak {})", stats.peak);
        assert!(samples.iter().all(|s| s.is_finite()), "{name}: non-finite samples");
        assert!(
            (-30.0..=-8.0).contains(&rms_db),
            "{name}: body RMS {rms_db:.1} dBFS outside −30..−8 (peak {:.2})",
            stats.peak
        );
        checked += 1;
    }
    assert!(checked >= 2, "expected at least the simulated scenes to be checked");
}
