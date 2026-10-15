//! Captions: turning events into one-liners a listener can read.
//!
//! A caption *template* is a string with `{attr}` placeholders — `"{size} {product} {side}
//! on Kraken"`. The engine resolves one template per layer (`LayerConfig.caption`, else the
//! source spec's `caption` param — recipes carry one — else a built-in default keyed by the
//! event name) and formats it into [`EventSnapshot::label`], [`NoteCause::label`] and, for
//! salient moments, [`EngineSnapshot::captions`].
//!
//! Placeholders: any event attribute by name, plus `{intensity}`, `{name}` (event name),
//! `{layer}` and `{source}`. Numbers are formatted compactly (`0.13`, `81,542`, `1.2M`);
//! text is used as-is; a missing attribute renders empty and the surrounding whitespace
//! collapses so `"{size} {product}"` with no size still reads `"BTC/USD"`.

use std::collections::{BTreeMap, HashMap, VecDeque};

use sonder_core::{Caption, LayerId, Scene, Seconds, SourceSpec, Value};

/// Captions kept for the snapshot.
pub const MAX_CAPTIONS: usize = 16;
/// An event name not heard on a layer for this long is "first of its kind" again.
const FIRST_OF_KIND_WINDOW: f64 = 60.0;
/// Events at or above this intensity are always captioned.
const SALIENT_INTENSITY: f64 = 0.8;

/// Default templates for the events coded adapters emit, keyed by event name.
const DEFAULTS: &[(&str, &str)] = &[
    ("quake", "M{mag} {place}"),
    ("strike", "lightning at {lat}, {lon}"),
    ("edit", "{wiki}: {title} ({bytes} bytes)"),
    ("car", "car in lane {lane}"),
    ("item", "HN {type} by {by}"),
    ("trade", "{size} {product} {side}"),
    ("message", "{topic}"),
    ("arrival", "{route} at {stop}"),
    ("onset", "sound onset"),
    ("post", "post in {lang}"),
];

/// Built-in template for an event name, if any.
pub fn default_template(event: &str) -> Option<&'static str> {
    DEFAULTS.iter().find(|(k, _)| *k == event).map(|(_, t)| *t)
}

/// Everything a template may reference.
#[derive(Debug, Clone, Copy)]
pub struct CaptionCtx<'a> {
    pub attrs: &'a BTreeMap<String, Value>,
    pub intensity: f64,
    pub name: &'a str,
    pub layer: &'a str,
    pub source: &'a str,
}

/// Compact number formatting: `0.13`, `4.1`, `42`, `81,542`, `1.2M`.
pub fn format_number(x: f64) -> String {
    if !x.is_finite() {
        return String::new();
    }
    let a = x.abs();
    let sign = if x < 0.0 { "-" } else { "" };
    if a >= 1e15 {
        return format!("{sign}{:.1e}", a);
    }
    if a.fract() == 0.0 && a < 1e6 {
        return format!("{sign}{}", group_thousands(a as u64));
    }
    if a >= 1000.0 {
        let (div, suffix) = if a >= 1e9 { (1e9, "B") } else if a >= 1e6 { (1e6, "M") } else { (1e3, "k") };
        let v = a / div;
        let s = if v >= 100.0 { format!("{v:.0}") } else { trim_zeros(format!("{v:.1}")) };
        return format!("{sign}{s}{suffix}");
    }
    if a >= 100.0 {
        return format!("{sign}{a:.0}");
    }
    if a >= 10.0 {
        return format!("{sign}{}", trim_zeros(format!("{a:.1}")));
    }
    if a >= 0.01 {
        return format!("{sign}{}", trim_zeros(format!("{a:.2}")));
    }
    if a == 0.0 {
        return "0".into();
    }
    // Tiny values: 3 significant digits.
    let digits = (-a.log10().floor()) as usize + 2;
    format!("{sign}{}", trim_zeros(format!("{a:.*}", digits.min(12))))
}

fn trim_zeros(s: String) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A value as a caption would print it.
pub fn format_value(v: &Value) -> String {
    match v {
        Value::Num(n) => format_number(*n),
        Value::Text(t) => t.trim().to_string(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.to_string(),
    }
}

/// Format `template` against `ctx`. Unknown placeholders render empty; whitespace runs
/// collapse; the result is trimmed.
pub fn format(template: &str, ctx: &CaptionCtx<'_>) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                if key.is_empty() || key.contains(char::is_whitespace) {
                    // Not a placeholder: keep the braces literally.
                    out.push_str(&rest[start..start + end + 2]);
                } else {
                    out.push_str(&lookup(key, ctx));
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    collapse_spaces(&out)
}

fn lookup(key: &str, ctx: &CaptionCtx<'_>) -> String {
    if let Some(v) = ctx.attrs.get(key) {
        return format_value(v);
    }
    match key {
        "intensity" => format_number(ctx.intensity),
        "name" | "event" => ctx.name.to_string(),
        "layer" => ctx.layer.to_string(),
        "source" => ctx.source.to_string(),
        _ => String::new(),
    }
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let t = out.trim_end();
    // Tidy punctuation left dangling by empty placeholders: "( bytes)" → "(bytes)", "M " → "M".
    t.replace("( ", "(").replace(" )", ")").replace(" ,", ",").replace("()", "").trim().to_string()
}

/// Per-scene caption state: resolved templates, first-of-kind memory, and the bounded
/// ring of captions for the snapshot.
#[derive(Debug, Default)]
pub struct Captioner {
    /// Per layer: explicit template (layer, else source spec), or None → defaults by event.
    templates: Vec<Option<String>>,
    layer_names: Vec<String>,
    source_names: Vec<String>,
    last_seen: HashMap<(u16, String), Seconds>,
    captions: VecDeque<Caption>,
    last_section: String,
    last_key: String,
}

impl Captioner {
    /// Resolve templates for every layer. `expanded` maps a layer index to the (recipe-
    /// expanded) spec of the source it reads, when it has one.
    pub fn new(scene: &Scene, expanded: impl Fn(usize) -> Option<SourceSpec>) -> Self {
        let mut templates = Vec::with_capacity(scene.layers.len());
        let mut source_names = Vec::with_capacity(scene.layers.len());
        for (i, layer) in scene.layers.iter().enumerate() {
            let owner = scene.source_owner(layer);
            source_names.push(owner.map(|o| o.name.clone()).unwrap_or_default());
            let t = if !layer.caption.trim().is_empty() {
                Some(layer.caption.clone())
            } else {
                expanded(i).and_then(|s| s.get_str("caption").map(str::to_string)).filter(|t| !t.trim().is_empty())
            };
            templates.push(t);
        }
        Self {
            templates,
            layer_names: scene.layers.iter().map(|l| l.name.clone()).collect(),
            source_names,
            ..Default::default()
        }
    }

    /// The template a layer uses for `event`, if any.
    pub fn template_for(&self, layer: usize, event: &str) -> Option<&str> {
        self.templates.get(layer).and_then(|t| t.as_deref()).or_else(|| default_template(event))
    }

    /// Caption an event on a layer (empty when there is no template at all).
    pub fn label(&self, layer: LayerId, name: &str, intensity: f64, attrs: &BTreeMap<String, Value>) -> String {
        let i = layer.0 as usize;
        let Some(t) = self.template_for(i, name) else { return String::new() };
        let ctx = CaptionCtx {
            attrs,
            intensity,
            name,
            layer: self.layer_names.get(i).map(String::as_str).unwrap_or(""),
            source: self.source_names.get(i).map(String::as_str).unwrap_or(""),
        };
        format(t, &ctx)
    }

    /// Note an event; returns `Some(importance)` when it deserves a caption (big, or the
    /// first of its name on this layer in a while).
    pub fn salience(&mut self, layer: LayerId, name: &str, intensity: f64, at: Seconds) -> Option<f64> {
        let key = (layer.0, name.to_string());
        let first = match self.last_seen.get(&key) {
            Some(&t) => at - t > FIRST_OF_KIND_WINDOW || at < t,
            None => true,
        };
        self.last_seen.insert(key, at);
        if intensity >= SALIENT_INTENSITY {
            Some(intensity.clamp(0.8, 1.0))
        } else if first {
            Some(0.35 + 0.4 * intensity.clamp(0.0, 1.0))
        } else {
            None
        }
    }

    /// Add a caption (bounded).
    pub fn push(&mut self, at: Seconds, layer: Option<LayerId>, text: impl Into<String>, importance: f64) {
        let text = text.into();
        if text.trim().is_empty() {
            return;
        }
        if self.captions.len() >= MAX_CAPTIONS {
            self.captions.pop_front();
        }
        self.captions.push_back(Caption { at, layer, text, importance: importance.clamp(0.0, 1.0) });
    }

    /// Caption a form-section change (first call only records the section).
    pub fn note_section(&mut self, at: Seconds, section: &str, note: &str) {
        if section.is_empty() || section == self.last_section {
            return;
        }
        let first = self.last_section.is_empty();
        self.last_section = section.to_string();
        if first {
            return;
        }
        let text = if note.is_empty() { format!("section: {section}") } else { format!("section: {section} — {note}") };
        self.push(at, None, text, 0.6);
    }

    /// Caption a key/mode change (first call only records it).
    pub fn note_key(&mut self, at: Seconds, key_name: &str, mode: &str) {
        if key_name.is_empty() {
            return;
        }
        let k = format!("{key_name} {mode}");
        if k == self.last_key {
            return;
        }
        let first = self.last_key.is_empty();
        self.last_key = k.clone();
        if !first {
            self.push(at, None, format!("now in {k}"), 0.5);
        }
    }

    /// Captions newest last.
    pub fn captions(&self) -> impl Iterator<Item = &Caption> {
        self.captions.iter()
    }

    /// Keep the caption ring across a scene reload.
    pub fn inherit(&mut self, old: &Captioner) {
        self.captions = old.captions.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn numbers_format_compactly() {
        assert_eq!(format_number(0.13), "0.13");
        assert_eq!(format_number(0.1304), "0.13");
        assert_eq!(format_number(4.1), "4.1");
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(81_542.0), "81,542");
        assert_eq!(format_number(1234.5), "1.2k");
        assert_eq!(format_number(1_500_000.5), "1.5M");
        assert_eq!(format_number(2_000_000.0), "2M");
        assert_eq!(format_number(-3.25), "-3.25");
        assert_eq!(format_number(0.00042), "0.00042");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(f64::NAN), "");
    }

    #[test]
    fn template_fills_attrs_and_collapses_missing() {
        let a = attrs(&[("size", Value::Num(0.13)), ("product", Value::Text("BTC/USD".into())), ("side", Value::Text("sell".into()))]);
        let ctx = CaptionCtx { attrs: &a, intensity: 0.7, name: "trade", layer: "ticks", source: "kraken" };
        assert_eq!(format("{size} {product} {side} on Kraken", &ctx), "0.13 BTC/USD sell on Kraken");
        assert_eq!(format("{size} {product} {missing} {side}", &ctx), "0.13 BTC/USD sell");
        assert_eq!(format("{name} on {layer} from {source} ({intensity})", &ctx), "trade on ticks from kraken (0.7)");
        let none = BTreeMap::new();
        let ctx = CaptionCtx { attrs: &none, intensity: 0.0, name: "edit", layer: "", source: "" };
        assert_eq!(format("{wiki}: {title} ({bytes} bytes)", &ctx), ": (bytes)");
        assert_eq!(format("literal { braces } stay", &ctx), "literal { braces } stay");
    }

    #[test]
    fn captioner_resolves_layer_then_spec_then_default() {
        let scene: Scene = toml::from_str(
            r#"
            name = "t"
            [[layer]]
            name = "a"
            role = "pluck"
            source = { kind = "usgs.earthquakes" }
            [[layer]]
            name = "b"
            role = "pluck"
            source = "a"
            caption = "boom {mag}"
            [[layer]]
            name = "c"
            role = "pluck"
            source = { kind = "websocket", url = "wss://x", caption = "ws {foo}" }
            "#,
        )
        .unwrap();
        let mut c = Captioner::new(&scene, |i| match &scene.layers[i].source {
            sonder_core::LayerSource::Spec(s) => Some(s.clone()),
            _ => None,
        });
        let a = attrs(&[("mag", Value::Num(4.1)), ("place", Value::Text("off Honshu".into())), ("foo", Value::Num(2.0))]);
        assert_eq!(c.label(LayerId(0), "quake", 0.5, &a), "M4.1 off Honshu");
        assert_eq!(c.label(LayerId(1), "quake", 0.5, &a), "boom 4.1");
        assert_eq!(c.label(LayerId(2), "anything", 0.5, &a), "ws 2");
        assert_eq!(c.label(LayerId(0), "unknown-event", 0.5, &a), "");

        // Salience: first of kind, then quiet, then big.
        assert!(c.salience(LayerId(0), "quake", 0.3, 0.0).is_some());
        assert!(c.salience(LayerId(0), "quake", 0.3, 1.0).is_none());
        assert!(c.salience(LayerId(0), "quake", 0.9, 2.0).is_some());
        assert!(c.salience(LayerId(0), "quake", 0.3, 70.0).is_some(), "first again after 60 s");

        c.note_section(0.0, "intro", "");
        c.note_section(1.0, "intro", "");
        c.note_section(2.0, "build", "");
        c.note_key(0.0, "D", "lydian");
        c.note_key(3.0, "E", "lydian");
        let texts: Vec<&str> = c.captions().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["section: build", "now in E lydian"]);
        for i in 0..40 {
            c.push(i as f64, None, format!("c{i}"), 0.5);
        }
        assert_eq!(c.captions().count(), MAX_CAPTIONS);
    }
}
