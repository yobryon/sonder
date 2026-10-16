//! Built-in scene library (embedded) and scene loading.

use anyhow::{anyhow, Context};
use sonder_core::Scene;
use std::path::Path;

/// (name, toml source) for every curated scene in `scenes/`.
pub const BUILTIN_SCENE_SOURCES: &[(&str, &str)] = &[
    ("night-freeway", include_str!("../../../scenes/night-freeway.toml")),
    ("this-machine", include_str!("../../../scenes/this-machine.toml")),
    ("midnight-walk", include_str!("../../../scenes/midnight-walk.toml")),
    ("planet-tonight", include_str!("../../../scenes/planet-tonight.toml")),
    ("wikipedia-rain", include_str!("../../../scenes/wikipedia-rain.toml")),
    ("markets-open", include_str!("../../../scenes/markets-open.toml")),
    ("markets-live", include_str!("../../../scenes/markets-live.toml")),
    ("weather-station", include_str!("../../../scenes/weather-station.toml")),
    ("hacker-news", include_str!("../../../scenes/hacker-news.toml")),
    ("orbit", include_str!("../../../scenes/orbit.toml")),
    ("social-firehose", include_str!("../../../scenes/social-firehose.toml")),
    ("storm", include_str!("../../../scenes/storm.toml")),
    ("machines", include_str!("../../../scenes/machines.toml")),
    ("deep-ocean", include_str!("../../../scenes/deep-ocean.toml")),
    ("city-transit", include_str!("../../../scenes/city-transit.toml")),
    ("seismic", include_str!("../../../scenes/seismic.toml")),
    ("jam", include_str!("../../../scenes/jam.toml")),
];

/// Parse a scene from TOML text.
pub fn parse_scene(toml_src: &str) -> anyhow::Result<Scene> {
    let scene: Scene = toml::from_str(toml_src).context("scene is not valid TOML for the Scene schema")?;
    Ok(scene)
}

/// All built-in scenes, parsed.
pub fn builtin_scenes() -> Vec<(&'static str, Scene)> {
    BUILTIN_SCENE_SOURCES
        .iter()
        .filter_map(|(name, src)| parse_scene(src).ok().map(|s| (*name, s)))
        .collect()
}

/// Load a scene by built-in name (`night-freeway`), or by path to a `.toml` file.
/// A bare name is tried as a built-in first, then as `<name>.toml` in the cwd.
pub fn load_scene(name_or_path: &str) -> anyhow::Result<Scene> {
    if let Some((_, src)) = BUILTIN_SCENE_SOURCES.iter().find(|(n, _)| *n == name_or_path) {
        return parse_scene(src).with_context(|| format!("built-in scene `{name_or_path}`"));
    }
    let path = Path::new(name_or_path);
    let candidates = [path.to_path_buf(), path.with_extension("toml")];
    for p in &candidates {
        if p.is_file() {
            let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            return parse_scene(&text).with_context(|| format!("scene file {}", p.display()));
        }
    }
    let names: Vec<&str> = BUILTIN_SCENE_SOURCES.iter().map(|(n, _)| *n).collect();
    Err(anyhow!("no scene `{name_or_path}` (not a built-in: {}; not a file)", names.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtin_scenes_parse_and_validate_structure() {
        for (name, src) in BUILTIN_SCENE_SOURCES {
            let scene = parse_scene(src).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            scene.validate_structure().unwrap_or_else(|errs| panic!("{name}: {}", errs.join("; ")));
            assert!(!scene.layers.is_empty(), "{name} has no layers");
        }
        assert_eq!(builtin_scenes().len(), BUILTIN_SCENE_SOURCES.len());
    }
}
