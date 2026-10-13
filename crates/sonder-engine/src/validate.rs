//! Full scene validation: structure, expressions, source kinds, instrument names.

use sonder_core::{LayerSource, Scene};

/// Validate a scene as thoroughly as possible without running it.
pub fn validate(scene: &Scene) -> Result<(), Vec<String>> {
    let mut errs = match scene.validate_structure() {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    let registry = crate::live::registry();
    for layer in &scene.layers {
        let ctx = format!("layer `{}`", layer.name);
        if let LayerSource::Spec(spec) = &layer.source {
            if registry.info(&spec.kind).is_none() {
                let known: Vec<&str> = registry.catalog().iter().map(|i| i.kind).collect();
                errs.push(format!("{ctx}: unknown source kind `{}` (known: {})", spec.kind, known.join(", ")));
            }
        }
        if !crate::instruments::exists(scene, &layer.instrument) {
            errs.push(format!(
                "{ctx}: unknown instrument `{}` (presets: {})",
                layer.instrument,
                sonder_dsp::presets::names().join(", ")
            ));
        }
        if let Err(e) = sonder_expr::LayerPipeline::compile(&layer.signals, &layer.map, 0) {
            errs.push(format!("{ctx}: {e}"));
        }
        let signal_names: Vec<String> = layer.signals.keys().cloned().collect();
        for (ev, mapping) in &layer.on_event {
            for (key, src) in mapping {
                if matches!(key.as_str(), "quantize" | "drum" | "snap") {
                    if key == "quantize" {
                        if let Err(e) = sonder_expr::parse_grid(src) {
                            errs.push(format!("{ctx}: on_event.{ev}.quantize: {e}"));
                        }
                    }
                    continue;
                }
                if let Err(e) = sonder_expr::EventExpr::compile(src, &signal_names) {
                    errs.push(format!("{ctx}: on_event.{ev}.{key}: {e}"));
                }
            }
        }
    }
    if sonder_theory::mode_by_name(&scene.conductor.mode).is_none() {
        errs.push(format!("conductor.mode `{}` is not a known mode", scene.conductor.mode));
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtin_scenes_validate_fully() {
        for (name, scene) in crate::builtin_scenes() {
            if let Err(errs) = super::validate(&scene) {
                panic!("{name}:\n  {}", errs.join("\n  "));
            }
        }
    }
}
