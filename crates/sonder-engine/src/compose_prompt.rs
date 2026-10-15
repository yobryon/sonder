//! Context for the LLM scene composer (`sonder compose`, browser prompt box).
//!
//! No network here: this module only assembles the *system prompt* — a compact, complete
//! reference the model needs to write a valid scene — from the scene schema, the expression
//! language, the source catalog, the recipes and the instrument presets. The native CLI
//! feeds it the live registry; the browser build gets the same thing serialized by
//! [`catalog_json`] (`site/catalog.json`), which also carries the finished prompt so both
//! surfaces send byte-identical context.

use serde::{Deserialize, Serialize};

/// A source kind as the prompt needs it. Mirrors `sonder_sources::SourceInfo` without the
/// dependency, so the browser build can be fed from a JSON catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceInfoLike {
    pub kind: String,
    pub summary: String,
    pub params: Vec<ParamLike>,
    /// Reading channel names (`field("x")`).
    pub channels: Vec<String>,
    /// Event names (`event("x")`, `on_event.x`).
    pub events: Vec<String>,
    #[serde(default)]
    pub network: bool,
}

/// One source/recipe parameter: `(name, summary, default)`; `default = None` means required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ParamLike {
    pub name: String,
    pub summary: String,
    pub default: Option<String>,
}

impl ParamLike {
    pub fn new(name: impl Into<String>, summary: impl Into<String>, default: Option<String>) -> Self {
        Self { name: name.into(), summary: summary.into(), default }
    }
}

/// A recipe as the prompt needs it (recipes expand to `websocket`/`sse`/`http-json` + a
/// mapping; the model only needs the kind, the summary and the parameters).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecipeLike {
    pub kind: String,
    pub summary: String,
    pub params: Vec<ParamLike>,
    /// Event names the recipe's mapping emits.
    pub events: Vec<String>,
    /// Reading channels the recipe's mapping pushes.
    pub channels: Vec<String>,
}

impl RecipeLike {
    /// Convert an embedded recipe.
    pub fn from_recipe(r: &crate::recipes::Recipe) -> Self {
        let params = r
            .params
            .iter()
            .map(|(name, p)| ParamLike::new(name, &p.summary, p.default.as_ref().map(|d| d.to_string())))
            .collect();
        let event = r.spec.get("event");
        let events = event
            .and_then(|e| e.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| vec![n.to_string()])
            .unwrap_or_default();
        let channels = event
            .and_then(|e| e.get("readings"))
            .and_then(|m| m.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        Self { kind: r.recipe.kind.clone(), summary: r.recipe.summary.clone(), params, events, channels }
    }
}

/// An instrument preset: `(name, description)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PresetLike {
    pub name: String,
    pub description: String,
}

/// Everything the composer needs, serializable for the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Catalog {
    pub sources: Vec<SourceInfoLike>,
    pub recipes: Vec<RecipeLike>,
    pub presets: Vec<PresetLike>,
    /// The finished system prompt built from the three lists above.
    pub system_prompt: String,
}

/// All embedded recipes, in prompt form.
pub fn recipes_catalog() -> Vec<RecipeLike> {
    crate::recipes::all().iter().map(RecipeLike::from_recipe).collect()
}

/// All built-in instrument presets, in prompt form.
pub fn presets_catalog() -> Vec<PresetLike> {
    sonder_dsp::presets::names()
        .into_iter()
        .map(|n| PresetLike { name: n.to_string(), description: sonder_dsp::presets::describe(n).unwrap_or("").to_string() })
        .collect()
}

/// Serialize the catalog (sources from the caller — the native registry or a browser list —
/// plus every recipe and preset, plus the finished system prompt) as pretty JSON.
pub fn catalog_json(sources: &[SourceInfoLike]) -> String {
    let recipes = recipes_catalog();
    let presets = presets_catalog();
    let system_prompt = system_prompt(sources, &recipes, &presets);
    let cat = Catalog { sources: sources.to_vec(), recipes, presets, system_prompt };
    serde_json::to_string_pretty(&cat).unwrap_or_else(|_| "{}".into())
}

/// Example scenes shown to the model. Each is validated by the test suite.
pub const EXAMPLE_SCENES: &[&str] = &[EXAMPLE_FREEWAY, EXAMPLE_MARKETS, EXAMPLE_MACHINE];

const EXAMPLE_FREEWAY: &str = r#"name = "Night Freeway"
description = "Headlights over an overpass at 3am; simulated traffic."
tags = ["ambient", "offline"]

[conductor]
key = "D"
mode = "lydian"
tempo = 62
tempo_range = [56, 76]
drift = "slow"
progression = "modal"
bars_per_chord = 2
mood_baseline = [0.35, 0.25, 0.55]

[effects.reverb]
size = 0.9
level = 0.9

[[layer]]
name = "traffic"
role = "pad"
instrument = "pad.glass"
source = { kind = "synthetic.freeway", lanes = 4, flow = 0.4 }
signals.flow   = "norm(smooth(rate(event('*'), 20), 4))"
signals.speed  = "norm(smooth(field('mean_speed'), 6))"
map.energy     = "flow"
map.brightness = "speed"
map.register   = "0.35 + 0.3 * speed"
map.cutoff     = "0.3 + 0.6 * speed"

[[layer]]
name = "headlights"
role = "pluck"
instrument = "pluck.glass"
source = "traffic"
on_event."*".pitch    = "1 - clamp(attr('size'), 0, 1) * 0.7"
on_event."*".velocity = "0.35 + 0.65 * clamp(attr('brightness'), 0, 1)"
on_event."*".pan      = "attr('x')"
on_event."*".quantize = "1/16"
on_event."*".snap     = "chord"

[[layer]]
name = "undertow"
role = "bass"
instrument = "bass.sub"
source = "traffic"
signals.flow = "norm(smooth(rate(event('*'), 30), 8))"
map.density  = "0.15 + 0.5 * flow"
"#;

const EXAMPLE_MARKETS: &str = r#"name = "Markets Live"
description = "Kraken trades: buys rise, sells fall, size is loudness."
tags = ["live", "rhythmic"]

[conductor]
key = "E"
mode = "minor_pentatonic"
tempo = 94
drift = "medium"
progression = "functional"
swing = 0.1

[conductor.form]
style = "sections"
section_bars = 16

[[layer]]
name = "kraken"
role = "pluck"
instrument = "pluck.marimba"
source = { kind = "kraken.trades", symbols = ["BTC/USD", "ETH/USD"] }
signals.rate = "norm(smooth(rate(event('trade'), 5), 1.5))"
signals.move = "norm(smooth(abs(derivative(smooth(field('price'), 2))), 10))"
map.energy   = "rate"
map.tension  = "move"
map.density  = "rate"
on_event.trade.chance   = "clamp(0.3 + 0.7 * intensity, 0, 1)"
on_event.trade.pitch    = "if(attr('buy') > 0.5, 0.55 + 0.45 * intensity, 0.45 - 0.45 * intensity)"
on_event.trade.velocity = "0.3 + 0.7 * intensity"
on_event.trade.pan      = "if(attr('buy') > 0.5, 0.65, 0.35)"
on_event.trade.quantize = "1/16"
on_event.trade.length   = "0.25"

[[layer]]
name = "ticks"
role = "perc"
instrument = "perc.kit"
source = "kraken"
signals.rate = "norm(smooth(rate(event('trade'), 5), 1.5))"
map.density  = "0.3 + 0.7 * rate"
on_event.trade.drum     = "hat"
on_event.trade.velocity = "0.3 + 0.6 * intensity"
on_event.trade.chance   = "0.5"
on_event.trade.quantize = "1/16"

[[layer]]
name = "bed"
role = "pad"
instrument = "pad.warm"
source = "kraken"
signals.trend = "norm(smooth(field('price'), 20))"
map.register  = "0.3 + 0.3 * trend"
map.volume    = "0.6"
"#;

const EXAMPLE_MACHINE: &str = r#"name = "This Machine"
description = "CPU and memory of the computer running Sonder."
tags = ["offline", "system"]

[conductor]
key = "A"
mode = "dorian"
tempo = 72
seed = 7

[conductor.form]
style = "arc"
arc_minutes = 4

[[layer]]
name = "cpu"
role = "drone"
instrument = "drone.sub"
source = { kind = "system.metrics", interval = 1 }
signals.load  = "smooth(norm(field('cpu'), 0, 100), 3)"
signals.mem   = "norm(field('mem'), 0, 100)"
signals.spike = "onset(load, 0.7)"
map.cutoff    = "0.2 + 0.7 * load"
map.texture   = "load"
map.energy    = "load"
map.brightness = "1 - mem"
on_event.spike.pitch    = "0.8"
on_event.spike.velocity = "0.7"

[[layer]]
name = "processes"
role = "bells"
instrument = "bells.fm"
source = "cpu"
on_event.proc_spawn.pitch    = "hash(attr_text('name'))"
on_event.proc_spawn.velocity = "0.5"
on_event.proc_spawn.length   = "4"

[[layer]]
name = "melody"
role = "lead"
instrument = "lead.soft"
map.density = "0.4"
map.step    = "0.3"
"#;

/// The system prompt: schema, expression reference, catalog, examples, hard rules.
pub fn system_prompt(sources: &[SourceInfoLike], recipes: &[RecipeLike], presets: &[PresetLike]) -> String {
    let mut s = String::with_capacity(32 * 1024);
    s.push_str(INTRO);
    s.push_str(SCHEMA);
    s.push_str(EXPR);

    s.push_str("\n## Source kinds\n\nFormat: `kind` — summary. Params (default; `required` if none). `field` channels. `event` names.\n\n");
    for src in sources {
        push_source(&mut s, &src.kind, &src.summary, &src.params, &src.channels, &src.events, src.network);
    }

    s.push_str("\n## Recipes (named kinds built on websocket/sse/http-json; use exactly like a source kind)\n\n");
    for r in recipes {
        push_source(&mut s, &r.kind, &r.summary, &r.params, &r.channels, &r.events, true);
    }

    s.push_str("\n## Instrument presets (`instrument = \"<name>\"`)\n\n");
    for p in presets {
        s.push_str(&format!("- `{}` — {}\n", p.name, p.description));
    }
    s.push_str(
        "\nRole defaults when `instrument` is omitted: pad→pad.glass, drone→drone.sub, bass→bass.sub, lead→lead.soft, arp→arp.pulse, pluck→pluck.nylon, bells→bells.fm, perc→perc.kit, texture→texture.wind.\n",
    );

    s.push_str("\n## Example scenes\n");
    for (i, ex) in EXAMPLE_SCENES.iter().enumerate() {
        s.push_str(&format!("\n### Example {}\n\n```toml\n{}```\n", i + 1, ex));
    }

    s.push_str(RULES);
    s
}

fn push_source(s: &mut String, kind: &str, summary: &str, params: &[ParamLike], channels: &[String], events: &[String], network: bool) {
    s.push_str(&format!("- `{kind}` — {summary}{}\n", if network { "" } else { " (offline)" }));
    if !params.is_empty() {
        let ps: Vec<String> = params
            .iter()
            .map(|p| match &p.default {
                Some(d) => format!("`{}`={} ({})", p.name, d, p.summary),
                None => format!("`{}` required ({})", p.name, p.summary),
            })
            .collect();
        s.push_str(&format!("  - params: {}\n", ps.join("; ")));
    }
    if !channels.is_empty() {
        s.push_str(&format!("  - channels: {}\n", channels.join(", ")));
    }
    if !events.is_empty() {
        s.push_str(&format!("  - events: {}\n", events.join(", ")));
    }
}

const INTRO: &str = r#"You are the scene composer for Sonder, a realtime generative-music engine that turns live data into music. The user describes a mood, a data source or an idea; you answer with ONE complete scene as TOML. A scene stacks *layers*: each layer has a data source, *signals* (expressions over the source's readings/events, normalized to 0..1), a musical *role*, an *instrument*, a *map* (signals → musical parameters) and *on_event* triggers (one event → one note). A *conductor* owns key, mode, tempo, harmony drift and musical form.

Compose like a musician: pick a key/mode/tempo that fits the mood, give the arrangement a foundation (pad/drone/bass), a voice (lead/arp/pluck/bells) and motion (perc/texture), and make the data audible — the busiest signal should drive `energy`, discrete happenings should become notes. Prefer sources the user named; if none, pick ones that fit the idea. Offline sources (synthetic.*, system.metrics, clock.*) always work; live ones need network.
"#;

const SCHEMA: &str = r#"
## Scene TOML schema

Top level: `name` (string), `description`, `tags = [..]`, `viz` ("generic" | "freeway" | "map"), `[conductor]`, `[conductor.form]`, `[effects.reverb]`, `[effects.delay]`, `[effects.master]`, `[instruments.<name>]` (inline synth patches; rarely needed — prefer presets), `[[layer]]` (one or more).

`[conductor]` (all optional, defaults shown):
- `key = "D"` — pitch class: C, C#/Db, D, Eb, E, F, F#, G, Ab, A, Bb, B
- `mode = "lydian"` — one of: ionian/major, dorian, phrygian, lydian, mixolydian, aeolian/minor, locrian, major_pentatonic, minor_pentatonic, whole_tone, harmonic_minor, melodic_minor, blues, hirajoshi, in_sen, chromatic
- `tempo = 70` (BPM), `tempo_range = [55, 95]` (energy nudges tempo within it; lo ≤ tempo ≤ hi), `beats_per_bar = 4`, `swing = 0.0` (0..1)
- `drift = "slow"` — static | slow | medium | fast (how eagerly harmony wanders)
- `progression = "modal"` — functional (I–vi–IV–V walks) | modal (one scale note at a time) | static
- `bars_per_chord = 2.0`, `mood_smoothing = 20` (seconds), `mood_baseline = [0.4, 0.3, 0.5]` (energy, tension, brightness when no layer publishes), `humanize = 0.15` (0..1), `seed = 1` (optional integer; omit for random)

`[conductor.form]`: `style = "arc"` (static | arc | sections), `arc_minutes = 6.0`, `section_bars = 16`, `strength = 0.5` (0 data only .. 1 form only), `call_and_response = true`, `motif_recall = 0.4`.

`[effects.reverb]`: `size = 0.8`, `damping = 0.4`, `predelay_ms = 20`, `level = 0.8`, `lowcut_hz = 150`.
`[effects.delay]`: `beats = 0.75` (tempo-synced), `feedback = 0.35`, `ping_pong = true`, `damp_hz = 4000`, `level = 0.7`.
`[effects.master]`: `gain = 0.9`, `saturation = 0.1`, `ceiling_db = -1`, `target_lufs = -16`, `glue = 0.25`.

`[[layer]]`:
- `name` — required, unique
- `role` — pad | drone | bass | lead | arp | pluck | bells | perc | texture
  - pad: sustains the chord, voice-led; `density` adds extensions, `brightness`/`cutoff` opens the filter
  - drone: tonic pedal; `texture` = noise/detune
  - bass: root/fifth on strong beats; `density` adds motion; `register` picks octave
  - lead: random-walk melody; `step` interval size, `density` note probability, `register` window
  - arp: arpeggiates the chord; `density` → subdivision, `register` → span
  - pluck / bells: event-triggered single notes (short / long ringing) — need `on_event`
  - perc: event-triggered hits (`on_event.x.drum`) and/or signal-driven patterns via `density`
  - texture: unpitched filtered noise bed; `texture`, `cutoff`, `pan`
- `instrument = "pad.glass"` — a preset name from the list below (or an `[instruments.x]` key). Omit for the role default.
- `source` — ONE of: an inline table `{ kind = "<source kind>", <param> = <value>, ... }`; the string name of another layer whose source to share (`source = "traffic"`; that layer must hold the inline table); or omitted (no data; the generator runs on the conductor alone — fine for a pad/bass/lead that just follows the harmony).
- `signals.<name> = "<expr>"` — named 0..1 signals over the source (stateful functions allowed). Signals may reference other signals of the same layer (no cycles).
- `map.<key> = "<expr>"` — expression over this layer's signals (and constants). Keys: energy, tension, brightness (contributions to the conductor's mood); density, register, step, texture, volume, pan, cutoff, resonance, reverb, delay (this layer). All 0..1.
- `on_event.<event>.<key> = "..."` — per event name (`"*"` matches any event; quote it: `on_event."*".pitch`). Keys: pitch (0..1 in the register window, quantized), velocity (0..1), pan (0..1), length (beats), chance (0..1 probability), quantize (string grid: "off", "1/4", "1/8", "1/16", "1/8t", "1/8."), drum (string: kick | snare | hat | click | noise | tom — perc only), snap (string: chord | scale | none). `pitch`, `velocity`, `pan`, `length`, `chance` are expressions; `quantize`, `drum`, `snap` are plain strings.
- `muted = false`, `mood_weight = 1.0`, `gain = 1.0` (0..4), `caption = "{attr} text"` (optional event caption template).

Event names a layer can use in `on_event`: the source's events (listed per kind below), plus any *edge signal* of the same layer (a signal whose top-level function is `onset`, `every` or `change` fires an event with the signal's name).
"#;

const EXPR: &str = r#"
## Expression language (used in signals, map and on_event)

Grammar: numbers (`1.5`, `10s`, `250ms`, `2m` — time suffixes become seconds), strings (`'single'` or `"double"`), `+ - * / % ^`, comparisons (`< <= > >= == !=` → 1/0), `&& || !`, `if(cond, a, b)`, parentheses, `#` comments. Identifiers resolve to: a signal of this layer, a constant (`time`, `dt`, `beat`, `bar`, `bpm`, `pi`, `tau`, `e`; `intensity` in on_event), or a source channel (shorthand for `field("name")`). Unknown names are errors.

Sources of truth: `field("ch")` latest reading (holds last value, 0 before first); `event("name")` impulse train (sum of intensities this tick; `"*"` any, `"blob*"` prefix); on_event only: `intensity`, `attr("k")` / `attr("k", default)` (number; booleans 0/1; text parsed or hashed to 0..1), `attr_text("k") == "lit"` (1/0), `attr_len("k")`, `has("k")`.

Pure: abs, min, max, clamp(x, lo, hi), lerp(a, b, t), pow, sqrt, exp, log, log2, log10, floor, ceil, round, fract, sign, step(edge, x), smoothstep(e0, e1, x), if, sin, cos, tan, tanh, wrap, norm(x, lo, hi) (fixed-range, clamped), hash(x) (stable 0..1).

Stateful (signals only; windows/times are constant seconds): smooth(x, seconds) low-pass; ema(x, alpha); derivative(x); norm(x) ADAPTIVE normalization to the signal's own recent min/max (~2 min memory; outputs 0.5 until a range exists) — the key tool for making any feed musical; rate(events, window) events/sec; count(events, window); sum(x, window); lag(x, s); hold(x, s) peak hold with decay; maxw/minw(x, window); onset(x, threshold) edge; every(seconds) edge; change(x[, eps]) edge; random(); walk(step, seconds) seeded random walk 0..1.

Idioms: `norm(smooth(rate(event('x'), 10), 3))` = how busy, relative to itself; `norm(smooth(field('v'), 4))` = a reading, relative to itself; `norm(field('cpu'), 0, 100)` when the range is known; `smooth(..)` time 2–8 s for musical parameters; `hold(norm(event('quake'), 2, 7), 10m)` to let rare big events linger.

on_event expressions are PURE and stateless: they may use `intensity`, `attr*`, `has`, `time`, `beat`, pure functions and this layer's signals by name — NOT `field`, `event` or any stateful function (compute those as signals first and reference the signal).
"#;

const RULES: &str = r#"
## Hard rules

1. Use ONLY source kinds and recipe kinds listed above, with their listed parameter names; ONLY channel/event names they list (plus edge signals you define); ONLY instrument preset names listed above (or omit `instrument`); ONLY the map/on_event keys, roles, modes and pitch classes listed in the schema.
2. Signals before map: every name used in `map.*` or `on_event.*` must be a signal of that same layer, a constant, or (map only) a channel of that layer's source. A layer that shares a source (`source = "other"`) must define its own signals — signals are per layer.
3. `on_event` expressions must be pure (rule above). `quantize`, `drum`, `snap` are plain strings, not expressions.
4. Every event-triggered role (pluck, bells, and perc without `map.density`) needs at least one `on_event` table whose event name the source actually emits.
5. Keep numbers in range: 0..1 for signals/map/mood, `tempo` inside `tempo_range`, `gain` 0..4.
6. Quote keys that contain `*` or `.`: `on_event."*".pitch`. Use `[[layer]]` (array of tables), `signals.x = ".."` dotted keys, inline tables for `source`.
7. Aim for 3–6 layers, each with a one-line purpose, and a `description` that says what the listener will hear.
8. Return ONLY the scene, as a single fenced ```toml block, no prose before or after. If given validation errors, return the whole corrected scene in the same form.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sources() -> Vec<SourceInfoLike> {
        vec![SourceInfoLike {
            kind: "synthetic.freeway".into(),
            summary: "Simulated traffic".into(),
            params: vec![ParamLike::new("lanes", "number of lanes", Some("4".into())), ParamLike::new("url", "camera", None)],
            channels: vec!["count".into(), "mean_speed".into()],
            events: vec!["car".into()],
            network: false,
        }]
    }

    #[test]
    fn prompt_mentions_everything() {
        let p = system_prompt(&fake_sources(), &recipes_catalog(), &presets_catalog());
        for needle in [
            "synthetic.freeway",
            "`lanes`=4",
            "`url` required",
            "kraken.trades",
            "pad.glass",
            "texture.noise",
            "minor_pentatonic",
            "on_event",
            "```toml",
            "Hard rules",
            "norm(x)",
        ] {
            assert!(p.contains(needle), "prompt lacks {needle}");
        }
        // Compact: a few tens of KB, not hundreds.
        assert!(p.len() < 60_000, "prompt is {} bytes", p.len());
    }

    #[test]
    fn example_scenes_parse_and_validate_structure() {
        for (i, ex) in EXAMPLE_SCENES.iter().enumerate() {
            let scene = crate::parse_scene(ex).unwrap_or_else(|e| panic!("example {}: {e:#}", i + 1));
            scene.validate_structure().unwrap_or_else(|e| panic!("example {}: {}", i + 1, e.join("; ")));
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn example_scenes_validate_fully() {
        for (i, ex) in EXAMPLE_SCENES.iter().enumerate() {
            let scene = crate::parse_scene(ex).unwrap();
            if let Err(errs) = crate::validate(&scene) {
                panic!("example {}:\n  {}", i + 1, errs.join("\n  "));
            }
        }
    }

    #[test]
    fn catalog_json_round_trips_and_carries_prompt() {
        let json = catalog_json(&fake_sources());
        let cat: Catalog = serde_json::from_str(&json).unwrap();
        assert_eq!(cat.sources, fake_sources());
        assert!(!cat.recipes.is_empty() && !cat.presets.is_empty());
        assert_eq!(cat.system_prompt, system_prompt(&cat.sources, &cat.recipes, &cat.presets));
        let kraken = cat.recipes.iter().find(|r| r.kind == "kraken.trades").unwrap();
        assert_eq!(kraken.events, vec!["trade"]);
        assert!(kraken.channels.contains(&"price".to_string()));
    }
}
