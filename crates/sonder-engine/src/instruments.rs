//! Resolving the instrument a layer plays: scene-inline → built-in preset → role default.

use sonder_core::{Instrument, LayerConfig, Scene};

/// Returns the resolved instrument and the name it resolved to.
pub fn resolve(scene: &Scene, layer: &LayerConfig) -> (Instrument, String) {
    if !layer.instrument.is_empty() {
        if let Some(inst) = scene.instruments.get(&layer.instrument) {
            return (inst.clone(), layer.instrument.clone());
        }
        if let Some(inst) = sonder_dsp::presets::get(&layer.instrument) {
            return (inst, layer.instrument.clone());
        }
        tracing::warn!(layer = %layer.name, instrument = %layer.instrument, "unknown instrument; using role default");
    }
    let name = sonder_dsp::presets::default_for_role(layer.role);
    let inst = sonder_dsp::presets::get(name).unwrap_or_default();
    (inst, name.to_string())
}

/// Is this instrument name resolvable (inline or preset)?
pub fn exists(scene: &Scene, name: &str) -> bool {
    name.is_empty() || scene.instruments.contains_key(name) || sonder_dsp::presets::get(name).is_some()
}
