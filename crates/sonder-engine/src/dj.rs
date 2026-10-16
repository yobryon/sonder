//! The DJ: rule-based scene switching on world events.
//!
//! The DJ watches the *world*, not the playing scene: for every source kind its rules
//! mention it runs a lightweight **watcher** source of its own (default parameters, or the
//! rule's `params`), feeds the samples through [`Dj::observe`] and evaluates the rules once
//! a second in [`Dj::tick`]. When a rule fires and the current scene has held for at least
//! `min_hold`, the DJ asks for a hot reload into the rule's scene (the live runner applies
//! it through the same path as `LoadScene`), records a [`DjSnapshot`] and a caption, and
//! keeps watching; once the rule's `hold` is over with nothing else firing it returns to
//! `default`. `SetDj { enabled }` toggles it; a scene the user picks pauses it.
//!
//! Configuration is `dj.toml` (see `dj/default.toml`, embedded as [`DEFAULT_CONFIG`]):
//!
//! ```toml
//! [dj]
//! min_hold = 600
//! cooldown = 1800
//! default = "social-firehose"
//!
//! [[rule]]
//! name = "big quake"
//! when = "usgs.earthquakes: event quake with mag >= 5.5"
//! scene = "seismic"
//! hold = 900
//! priority = 10
//! params = { feed = "all_hour" }
//! ```
//!
//! The rule DSL is two forms:
//!
//! ```text
//! <kind>: event <name> [with <attr> <op> <value>]
//! <kind>: reading <channel> <op> <value> [for <seconds>]
//! ```
//!
//! `<op>` is one of `< <= > >= == !=`; `<value>` is a number or `'quoted text'` (text only
//! with `==` / `!=`). `intensity` works as an event attribute. `event *` matches any event.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sonder_core::{Datum, DjSnapshot, Sample, Scene, Seconds, SourceId, SourceSpec, Value};

use crate::mapping::SourceMapping;

/// The embedded `dj/default.toml`.
pub const DEFAULT_CONFIG: &str = include_str!("../../../dj/default.toml");

/// How often rules are evaluated (seconds).
pub const EVAL_PERIOD: f64 = 1.0;

// ── configuration ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DjConfig {
    #[serde(default)]
    pub dj: DjSettings,
    #[serde(default)]
    pub rule: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DjSettings {
    /// Seconds a scene stays before the DJ may switch.
    pub min_hold: f64,
    /// Seconds before the same rule may fire again.
    pub cooldown: f64,
    /// Scene to return to when nothing is happening.
    pub default: String,
}

impl Default for DjSettings {
    fn default() -> Self {
        Self { min_hold: 600.0, cooldown: 1800.0, default: "social-firehose".into() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    pub when: String,
    pub scene: String,
    /// Seconds the rule's scene plays before returning to `default` (default: `min_hold`).
    #[serde(default)]
    pub hold: Option<f64>,
    /// Higher wins when several rules fire in the same second.
    #[serde(default)]
    pub priority: i32,
    /// Parameters for the watcher source of this rule's kind.
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
}

impl DjConfig {
    pub fn parse(toml_src: &str) -> anyhow::Result<Self> {
        let cfg: DjConfig = toml::from_str(toml_src).map_err(|e| anyhow::anyhow!("dj.toml: {e}"))?;
        if cfg.rule.is_empty() {
            anyhow::bail!("dj.toml: no [[rule]] tables");
        }
        Ok(cfg)
    }
}

// ── the rule DSL ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "==" | "=" => Op::Eq,
            "!=" => Op::Ne,
            _ => return None,
        })
    }
    fn test_num(self, a: f64, b: f64) -> bool {
        match self {
            Op::Lt => a < b,
            Op::Le => a <= b,
            Op::Gt => a > b,
            Op::Ge => a >= b,
            Op::Eq => a == b,
            Op::Ne => a != b,
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Eq => "==",
            Op::Ne => "!=",
        })
    }
}

/// The right-hand side of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Num(f64),
    Text(String),
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Num(n) => write!(f, "{}", crate::captions::format_number(*n)),
            Operand::Text(t) => write!(f, "'{t}'"),
        }
    }
}

/// A parsed `when`.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// `<kind>: event <name> [with <attr> <op> <value>]`
    Event { kind: String, name: String, filter: Option<(String, Op, Operand)> },
    /// `<kind>: reading <channel> <op> <value> [for <seconds>]`
    Reading { kind: String, channel: String, op: Op, value: f64, for_secs: f64 },
}

impl Condition {
    pub fn kind(&self) -> &str {
        match self {
            Condition::Event { kind, .. } | Condition::Reading { kind, .. } => kind,
        }
    }
}

/// Split the part after `<kind>:` into words, keeping `'quoted text'` together.
fn words(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '\'' || c == '"' {
            chars.next();
            let mut w = String::new();
            loop {
                match chars.next() {
                    Some(ch) if ch == c => break,
                    Some(ch) => w.push(ch),
                    None => return Err("unterminated quote".into()),
                }
            }
            out.push(format!("'{w}"));
        } else {
            let mut w = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                w.push(ch);
                chars.next();
            }
            out.push(w);
        }
    }
    Ok(out)
}

fn operand(w: &str) -> Result<Operand, String> {
    if let Some(t) = w.strip_prefix('\'') {
        return Ok(Operand::Text(t.to_string()));
    }
    w.parse::<f64>().map(Operand::Num).map_err(|_| format!("`{w}` is not a number (quote text: 'like this')"))
}

/// Parse a rule's `when`.
pub fn parse_condition(src: &str) -> Result<Condition, String> {
    let (kind, rest) = src.split_once(':').ok_or_else(|| "expected `<kind>: event …` or `<kind>: reading …`".to_string())?;
    let kind = kind.trim();
    if kind.is_empty() || kind.contains(char::is_whitespace) {
        return Err(format!("`{kind}` is not a source kind"));
    }
    let w = words(rest)?;
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    match w.first().copied() {
        Some("event") => {
            let name = w.get(1).filter(|n| !n.starts_with('\'')).ok_or("event needs a name: `event quake`")?;
            let filter = match &w[2..] {
                [] => None,
                ["with", attr, op, value] => {
                    let op = Op::parse(op).ok_or_else(|| format!("unknown operator `{op}`"))?;
                    let value = operand(value)?;
                    if matches!(value, Operand::Text(_)) && !matches!(op, Op::Eq | Op::Ne) {
                        return Err(format!("text can only be compared with == or !=, not {op}"));
                    }
                    Some((attr.to_string(), op, value))
                }
                _ => return Err("after the event name only `with <attr> <op> <value>` may follow".into()),
            };
            Ok(Condition::Event { kind: kind.to_string(), name: name.to_string(), filter })
        }
        Some("reading") => {
            let (channel, op, value, tail) = match &w[1..] {
                [channel, op, value, tail @ ..] => (channel, op, value, tail),
                _ => return Err("reading needs `reading <channel> <op> <value>`".into()),
            };
            let op = Op::parse(op).ok_or_else(|| format!("unknown operator `{op}`"))?;
            let value = match operand(value)? {
                Operand::Num(n) => n,
                Operand::Text(_) => return Err("readings are numbers; compare with a number".into()),
            };
            let for_secs = match tail {
                [] => 0.0,
                ["for", secs] => secs.parse::<f64>().ok().filter(|s| *s >= 0.0).ok_or_else(|| format!("`for` needs seconds, got `{secs}`"))?,
                _ => return Err("after the value only `for <seconds>` may follow".into()),
            };
            Ok(Condition::Reading { kind: kind.to_string(), channel: channel.to_string(), op, value, for_secs })
        }
        Some(other) => Err(format!("expected `event` or `reading` after `{kind}:`, got `{other}`")),
        None => Err(format!("nothing after `{kind}:`")),
    }
}

// ── the engine ──────────────────────────────────────────────────────────────────────

struct Rule {
    cfg: RuleConfig,
    cond: Condition,
    scene: Scene,
    hold: f64,
    last_fired: Option<Seconds>,
    /// An event matched since the last evaluation (its reason).
    pending: Option<String>,
    /// Since when a reading condition has held.
    true_since: Option<Seconds>,
}

/// What the DJ wants the runner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Hot-reload into this scene, then show the caption (importance 0.9).
    Load { scene: Box<Scene>, caption: String },
    /// Just say something.
    Caption(String, f64),
}

/// The rule engine. Pure: feed it samples and ticks, apply the actions it returns.
pub struct Dj {
    settings: DjSettings,
    rules: Vec<Rule>,
    default_scene: Scene,
    watchers: Vec<(SourceId, SourceSpec)>,
    mappings: HashMap<SourceId, SourceMapping>,
    readings: HashMap<SourceId, BTreeMap<String, f64>>,
    enabled: bool,
    /// Rule whose scene is playing (None: the default or the user's scene).
    current: Option<usize>,
    held_since: Option<Seconds>,
    reason: String,
    /// A rule that fired while the hold was still running: (rule, reason).
    candidate: Option<(usize, String)>,
    last_eval: Option<Seconds>,
}

impl Dj {
    /// Build from TOML. Scenes are resolved like `sonder play` does (built-in name or
    /// path), plus relative to `base_dir` (the directory of the dj.toml) when given.
    pub fn from_toml(toml_src: &str, base_dir: Option<&Path>) -> anyhow::Result<Self> {
        let cfg = DjConfig::parse(toml_src)?;
        let base = base_dir.map(Path::to_path_buf);
        Self::new(cfg, move |name| resolve_scene(name, base.as_deref()))
    }

    /// Build from a parsed config; `resolve` turns a rule's `scene` into a [`Scene`].
    pub fn new(cfg: DjConfig, resolve: impl Fn(&str) -> anyhow::Result<Scene>) -> anyhow::Result<Self> {
        let resolve_valid = |name: &str| -> anyhow::Result<Scene> {
            let scene = resolve(name)?;
            if let Err(errs) = crate::validate(&scene) {
                anyhow::bail!("scene `{name}` is invalid:\n  {}", errs.join("\n  "));
            }
            Ok(scene)
        };
        let default_scene = resolve_valid(&cfg.dj.default).map_err(|e| anyhow::anyhow!("dj.default: {e:#}"))?;
        let mut rules = Vec::with_capacity(cfg.rule.len());
        let mut specs: BTreeMap<String, (SourceSpec, String)> = BTreeMap::new();
        for r in cfg.rule {
            let cond = parse_condition(&r.when).map_err(|e| anyhow::anyhow!("rule `{}`: `{}`: {e}", r.name, r.when))?;
            let scene = resolve_valid(&r.scene).map_err(|e| anyhow::anyhow!("rule `{}`: {e:#}", r.name))?;
            let kind = cond.kind().to_string();
            match specs.get_mut(&kind) {
                None => {
                    let mut spec = SourceSpec::new(&kind);
                    spec.params = r.params.clone();
                    specs.insert(kind, (spec, r.name.clone()));
                }
                Some((spec, owner)) if !r.params.is_empty() => {
                    if spec.params.is_empty() {
                        spec.params = r.params.clone();
                        *owner = r.name.clone();
                    } else if spec.params != r.params {
                        anyhow::bail!("rules `{owner}` and `{}` give different params for `{kind}`; one watcher runs per kind", r.name);
                    }
                }
                Some(_) => {}
            }
            let hold = r.hold.unwrap_or(cfg.dj.min_hold).max(0.0);
            rules.push(Rule { cfg: r, cond, scene, hold, last_fired: None, pending: None, true_since: None });
        }
        let mut watchers = Vec::new();
        let mut mappings = HashMap::new();
        for (kind, (spec, _)) in specs {
            let id = SourceId::new(&kind);
            let expanded = crate::recipes::expand(&spec).map_err(|e| anyhow::anyhow!("watcher `{kind}`: {e}"))?;
            if let Some(m) = SourceMapping::from_spec(&expanded).map_err(|e| anyhow::anyhow!("watcher `{kind}`: {e}"))? {
                mappings.insert(id.clone(), m);
            }
            watchers.push((id, expanded));
        }
        Ok(Self {
            settings: cfg.dj,
            rules,
            default_scene,
            watchers,
            mappings,
            readings: HashMap::new(),
            enabled: true,
            current: None,
            held_since: None,
            reason: String::new(),
            candidate: None,
            last_eval: None,
        })
    }

    /// The watcher sources to run: one per kind the rules reference (recipes expanded),
    /// keyed by the kind name.
    pub fn watch_specs(&self) -> &[(SourceId, SourceSpec)] {
        &self.watchers
    }

    pub fn settings(&self) -> &DjSettings {
        &self.settings
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turn the DJ on or off. Turning it on restarts the hold from `now`.
    pub fn set_enabled(&mut self, on: bool, now: Seconds) {
        if on && !self.enabled {
            self.held_since = Some(now);
            self.current = None;
            self.candidate = None;
            self.reason = "watching".into();
        } else if !on {
            self.reason = "off".into();
            self.candidate = None;
        }
        self.enabled = on;
    }

    /// The user picked a scene: the DJ steps back until `SetDj { enabled: true }`.
    pub fn pause_for_user(&mut self) {
        self.enabled = false;
        self.current = None;
        self.candidate = None;
        self.reason = "paused — you picked the scene".into();
    }

    /// Feed a watcher sample (any source id the DJ does not watch is ignored).
    pub fn observe(&mut self, sample: Sample) {
        if !self.watchers.iter().any(|(id, _)| *id == sample.source) {
            return;
        }
        let samples = match self.mappings.get(&sample.source) {
            Some(m) if matches!(sample.datum, Datum::Event { .. }) => m.apply(sample, 0.0),
            _ => vec![sample],
        };
        for s in samples {
            match &s.datum {
                Datum::Reading { values } => {
                    self.readings.entry(s.source.clone()).or_default().extend(values.iter().map(|(k, v)| (k.clone(), *v)));
                }
                Datum::Event { name, intensity, attrs } => {
                    for r in &mut self.rules {
                        let Condition::Event { kind, name: want, filter } = &r.cond else { continue };
                        if kind.as_str() != s.source.as_str() || (want != "*" && want != name) {
                            continue;
                        }
                        if let Some((attr, op, value)) = filter {
                            let actual = if attr == "intensity" { Some(Value::Num(*intensity)) } else { attrs.get(attr).cloned() };
                            let Some(actual) = actual else { continue };
                            let pass = match value {
                                Operand::Num(b) => actual.as_f64().is_some_and(|a| op.test_num(a, *b)),
                                Operand::Text(b) => {
                                    let same = actual.as_str().is_some_and(|a| a == b);
                                    if *op == Op::Eq { same } else { !same }
                                }
                            };
                            if !pass {
                                continue;
                            }
                        }
                        r.pending = Some(event_reason(name, *intensity, attrs, filter.as_ref()));
                    }
                }
                _ => {}
            }
        }
    }

    /// Evaluate the rules (at most once per [`EVAL_PERIOD`]) and return what to do.
    /// `current_scene` is the name of the scene playing now.
    pub fn tick(&mut self, now: Seconds, current_scene: &str) -> Vec<Action> {
        if self.held_since.is_none() {
            self.held_since = Some(now);
            if self.reason.is_empty() {
                self.reason = "watching".into();
            }
        }
        if self.last_eval.is_some_and(|t| now - t < EVAL_PERIOD && now >= t) {
            return Vec::new();
        }
        self.last_eval = Some(now);
        if !self.enabled {
            for r in &mut self.rules {
                r.pending = None;
                r.true_since = None;
            }
            return Vec::new();
        }

        // Which rules fire this second?
        let mut best: Option<(usize, i32, String)> = None;
        for (i, r) in self.rules.iter_mut().enumerate() {
            let fired = match &r.cond {
                Condition::Event { .. } => r.pending.take(),
                Condition::Reading { kind, channel, op, value, for_secs } => {
                    let actual = self.readings.get(&SourceId::new(kind)).and_then(|m| m.get(channel)).copied();
                    match actual {
                        Some(a) if op.test_num(a, *value) => {
                            let since = *r.true_since.get_or_insert(now);
                            if now - since >= *for_secs {
                                Some(reading_reason(kind, channel, a, *op, *value))
                            } else {
                                None
                            }
                        }
                        _ => {
                            r.true_since = None;
                            None
                        }
                    }
                }
            };
            let Some(reason) = fired else { continue };
            if r.last_fired.is_some_and(|t| now - t < self.settings.cooldown) {
                continue;
            }
            if best.as_ref().is_none_or(|(_, p, _)| r.cfg.priority > *p) {
                best = Some((i, r.cfg.priority, reason));
            }
        }
        if let Some((i, _, reason)) = best {
            let supersedes = self.candidate.as_ref().is_none_or(|(j, _)| self.rules[i].cfg.priority >= self.rules[*j].cfg.priority);
            if supersedes {
                self.candidate = Some((i, reason));
            }
        }

        let held_since = self.held_since.unwrap_or(now);
        let held = now - held_since;
        let mut actions = Vec::new();
        if let Some((i, reason)) = self.candidate.clone() {
            if held >= self.settings.min_hold {
                self.candidate = None;
                let rule = &mut self.rules[i];
                rule.last_fired = Some(now);
                rule.true_since = None;
                self.current = Some(i);
                self.held_since = Some(now);
                self.reason = reason.clone();
                if rule.scene.name != current_scene {
                    actions.push(Action::Load { scene: Box::new(rule.scene.clone()), caption: format!("DJ → {}: {reason}", rule.scene.name) });
                } else {
                    actions.push(Action::Caption(format!("DJ stays on {}: {reason}", rule.scene.name), 0.6));
                }
            }
        } else if let Some(i) = self.current {
            if held >= self.rules[i].hold {
                self.current = None;
                self.held_since = Some(now);
                self.reason = "quiet again".into();
                if self.default_scene.name != current_scene {
                    actions.push(Action::Load { scene: Box::new(self.default_scene.clone()), caption: format!("DJ → {}: quiet again", self.default_scene.name) });
                }
            }
        }
        actions
    }

    /// What the UI shows.
    pub fn snapshot(&self, now: Seconds) -> DjSnapshot {
        let held_since = self.held_since.unwrap_or(now);
        let hold = match self.current {
            Some(i) => self.rules[i].hold,
            None => self.settings.min_hold,
        };
        DjSnapshot {
            enabled: self.enabled,
            rule: self.current.map(|i| self.rules[i].cfg.name.clone()).unwrap_or_default(),
            reason: self.reason.clone(),
            hold_remaining: if self.enabled { (held_since + hold - now).max(0.0) } else { 0.0 },
            candidate: self.candidate.as_ref().map(|(i, _)| self.rules[*i].cfg.name.clone()).unwrap_or_default(),
        }
    }
}

/// "M5.8 Tonga region" from the caption template when there is one, else
/// "quake (mag 5.8)".
fn event_reason(name: &str, intensity: f64, attrs: &BTreeMap<String, Value>, filter: Option<&(String, Op, Operand)>) -> String {
    if let Some(t) = crate::captions::default_template(name) {
        let ctx = crate::captions::CaptionCtx { attrs, intensity, name, layer: "", source: "" };
        let s = crate::captions::format(t, &ctx);
        if !s.trim().is_empty() {
            return s;
        }
    }
    match filter {
        Some((attr, _, _)) => {
            let v = if attr == "intensity" { Some(Value::Num(intensity)) } else { attrs.get(attr).cloned() };
            match v {
                Some(v) => format!("{name} ({attr} {})", crate::captions::format_value(&v)),
                None => name.to_string(),
            }
        }
        None => name.to_string(),
    }
}

/// "lightning rate 3.2 > 2".
fn reading_reason(kind: &str, channel: &str, actual: f64, op: Op, value: f64) -> String {
    let short = kind.rsplit('.').next().unwrap_or(kind);
    format!("{short} {channel} {} {op} {}", crate::captions::format_number(actual), crate::captions::format_number(value))
}

/// A rule's `scene`: built-in name or path (as `sonder play` takes it), else relative to
/// the dj.toml's directory.
fn resolve_scene(name: &str, base_dir: Option<&Path>) -> anyhow::Result<Scene> {
    match crate::load_scene(name) {
        Ok(s) => Ok(s),
        Err(e) => {
            if let Some(dir) = base_dir {
                let p: PathBuf = dir.join(name);
                for cand in [p.clone(), p.with_extension("toml")] {
                    if cand.is_file() {
                        return crate::load_scene(&cand.to_string_lossy());
                    }
                }
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(s: &str) -> Condition {
        parse_condition(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn dsl_parses_every_form() {
        assert_eq!(
            cond("usgs.earthquakes: event quake"),
            Condition::Event { kind: "usgs.earthquakes".into(), name: "quake".into(), filter: None }
        );
        assert_eq!(
            cond("usgs.earthquakes: event quake with mag >= 5.5"),
            Condition::Event {
                kind: "usgs.earthquakes".into(),
                name: "quake".into(),
                filter: Some(("mag".into(), Op::Ge, Operand::Num(5.5)))
            }
        );
        assert_eq!(
            cond("wikipedia.recent-changes:   event edit with wiki == 'enwiki'"),
            Condition::Event {
                kind: "wikipedia.recent-changes".into(),
                name: "edit".into(),
                filter: Some(("wiki".into(), Op::Eq, Operand::Text("enwiki".into())))
            }
        );
        assert_eq!(
            cond("x: event * with place != \"Tonga region\""),
            Condition::Event { kind: "x".into(), name: "*".into(), filter: Some(("place".into(), Op::Ne, Operand::Text("Tonga region".into()))) }
        );
        assert_eq!(
            cond("blitzortung.lightning: reading rate > 2"),
            Condition::Reading { kind: "blitzortung.lightning".into(), channel: "rate".into(), op: Op::Gt, value: 2.0, for_secs: 0.0 }
        );
        assert_eq!(
            cond("noaa.space-weather: reading kp >= 6 for 120"),
            Condition::Reading { kind: "noaa.space-weather".into(), channel: "kp".into(), op: Op::Ge, value: 6.0, for_secs: 120.0 }
        );
        for (op, want) in [("<", Op::Lt), ("<=", Op::Le), ("=", Op::Eq), ("==", Op::Eq), ("!=", Op::Ne)] {
            assert!(matches!(cond(&format!("k: reading c {op} 1")), Condition::Reading { op: got, .. } if got == want));
        }
    }

    #[test]
    fn dsl_rejects_bad_input() {
        for (src, needle) in [
            ("usgs.earthquakes event quake", "expected `<kind>:"),
            ("usgs earthquakes: event quake", "not a source kind"),
            (": event quake", "not a source kind"),
            ("usgs.earthquakes:", "nothing after"),
            ("usgs.earthquakes: dance quake", "expected `event` or `reading`"),
            ("usgs.earthquakes: event", "event needs a name"),
            ("usgs.earthquakes: event quake mag >= 5", "only `with"),
            ("usgs.earthquakes: event quake with mag >= 5 extra", "only `with"),
            ("usgs.earthquakes: event quake with mag ~ 5", "unknown operator"),
            ("usgs.earthquakes: event quake with mag >= big", "not a number"),
            ("usgs.earthquakes: event quake with place > 'Tonga'", "text can only"),
            ("usgs.earthquakes: event quake with place == 'Tonga", "unterminated quote"),
            ("b: reading rate", "reading needs"),
            ("b: reading rate > 'two'", "readings are numbers"),
            ("b: reading rate > 2 for", "only `for"),
            ("b: reading rate > 2 for soon", "`for` needs seconds"),
            ("b: reading rate > 2 for -1", "`for` needs seconds"),
            ("b: reading rate > 2 since 3", "only `for"),
        ] {
            let err = parse_condition(src).expect_err(src);
            assert!(err.contains(needle), "{src}: `{err}` lacks `{needle}`");
        }
    }

    fn scene(name: &str) -> Scene {
        crate::parse_scene(&format!("name = \"{name}\"\n[[layer]]\nname = \"drone\"\nrole = \"pad\"\ninstrument = \"pad.glass\"\n")).unwrap()
    }

    fn dj(toml_src: &str) -> Dj {
        Dj::new(DjConfig::parse(toml_src).unwrap(), |n| Ok(scene(n))).unwrap()
    }

    const QUAKES_AND_STORMS: &str = r#"
[dj]
min_hold = 60
cooldown = 300
default = "Home"

[[rule]]
name = "quake"
when = "usgs.earthquakes: event quake with mag >= 5.5"
scene = "Seismic"
hold = 120
priority = 10
params = { feed = "all_hour" }

[[rule]]
name = "storm"
when = "blitzortung.lightning: reading rate > 2 for 10"
scene = "Storm"
hold = 90
priority = 5
"#;

    fn quake(at: Seconds, mag: f64) -> Sample {
        Sample::event(&SourceId::new("usgs.earthquakes"), at, "quake", 0.7).with_attr("mag", mag).with_attr("place", "near Tonga")
    }
    fn rate(at: Seconds, rate: f64) -> Sample {
        Sample::reading(&SourceId::new("blitzortung.lightning"), at, [("rate", rate)])
    }
    fn loads(actions: &[Action]) -> Vec<&str> {
        actions.iter().filter_map(|a| match a {
            Action::Load { scene, .. } => Some(scene.name.as_str()),
            _ => None,
        }).collect()
    }

    #[test]
    fn config_builds_one_watcher_per_kind_with_rule_params() {
        let d = dj(QUAKES_AND_STORMS);
        let specs = d.watch_specs();
        assert_eq!(specs.len(), 2);
        let usgs = specs.iter().find(|(id, _)| id.as_str() == "usgs.earthquakes").unwrap();
        assert_eq!(usgs.1.kind, "usgs.earthquakes");
        assert_eq!(usgs.1.get_str("feed"), Some("all_hour"));
        assert!(specs.iter().any(|(id, s)| id.as_str() == "blitzortung.lightning" && s.params.is_empty()));
        assert_eq!(d.settings().min_hold, 60.0);
    }

    #[test]
    fn config_errors_are_specific() {
        let no_rules = DjConfig::parse("[dj]\nmin_hold = 1\n").unwrap_err().to_string();
        assert!(no_rules.contains("no [[rule]]"), "{no_rules}");
        let bad_when = Dj::new(DjConfig::parse("[[rule]]\nname='x'\nwhen='nope'\nscene='S'\n").unwrap(), |n| Ok(scene(n))).err().expect("error").to_string();
        assert!(bad_when.contains("rule `x`") && bad_when.contains("expected `<kind>:"), "{bad_when}");
        let missing = Dj::new(DjConfig::parse("[[rule]]\nname='x'\nwhen='k: event e'\nscene='Nope'\n").unwrap(), |n| {
            if n == "Nope" { anyhow::bail!("no scene `Nope`") } else { Ok(scene(n)) }
        })
        .err()
        .expect("error")
        .to_string();
        assert!(missing.contains("rule `x`") && missing.contains("no scene `Nope`"), "{missing}");
        let conflict = "[[rule]]\nname='a'\nwhen='k: event e'\nscene='S'\nparams = { x = 1 }\n[[rule]]\nname='b'\nwhen='k: event f'\nscene='S'\nparams = { x = 2 }\n";
        let err = Dj::new(DjConfig::parse(conflict).unwrap(), |n| Ok(scene(n))).err().expect("error").to_string();
        assert!(err.contains("different params"), "{err}");
        let unknown = DjConfig::parse("[dj]\nbogus = 1\n[[rule]]\nname='a'\nwhen='k: event e'\nscene='S'\n").unwrap_err().to_string();
        assert!(unknown.contains("bogus"), "{unknown}");
    }

    #[test]
    fn event_rule_waits_for_min_hold_then_switches_and_returns_to_default() {
        let mut d = dj(QUAKES_AND_STORMS);
        assert!(d.tick(0.0, "Home").is_empty());
        let snap = d.snapshot(0.0);
        assert!(snap.enabled && snap.rule.is_empty() && snap.reason == "watching");
        assert_eq!(snap.hold_remaining, 60.0);

        // A small quake does nothing; a big one at t=10 becomes the candidate (hold running).
        d.observe(quake(5.0, 3.0));
        d.observe(quake(10.0, 5.8));
        assert!(d.tick(10.0, "Home").is_empty());
        assert_eq!(d.snapshot(10.0).candidate, "quake");
        assert!(d.tick(10.5, "Home").is_empty(), "evaluates at 1 Hz, not per call");
        // Once the scene has held `min_hold`, the switch happens.
        let actions = d.tick(60.0, "Home");
        assert_eq!(loads(&actions), vec!["Seismic"]);
        let Action::Load { caption, .. } = &actions[0] else { panic!() };
        assert_eq!(caption, "DJ → Seismic: M5.8 near Tonga");
        let snap = d.snapshot(61.0);
        assert_eq!(snap.rule, "quake");
        assert_eq!(snap.reason, "M5.8 near Tonga");
        assert!(snap.candidate.is_empty());
        assert!((snap.hold_remaining - 119.0).abs() < 1e-9, "{}", snap.hold_remaining);
        // Nothing else happens: after `hold` (120 s) it returns to the default.
        assert!(d.tick(179.0, "Seismic").is_empty());
        assert_eq!(loads(&d.tick(180.0, "Seismic")), vec!["Home"]);
        assert_eq!(d.snapshot(180.0).reason, "quiet again");
        assert!(d.snapshot(180.0).rule.is_empty());
    }

    #[test]
    fn cooldown_blocks_the_same_rule_but_a_higher_priority_rule_wins() {
        let mut d = dj(QUAKES_AND_STORMS);
        d.tick(0.0, "Home");
        // Storm: the rate must hold above 2 for 10 s.
        d.observe(rate(0.0, 3.0));
        assert!(d.tick(5.0, "Home").is_empty());
        assert!(d.snapshot(5.0).candidate.is_empty(), "`for 10` not yet satisfied");
        d.observe(rate(7.0, 1.0));
        d.tick(8.0, "Home");
        d.observe(rate(9.0, 3.0)); // the clock restarts after a dip…
        d.tick(18.0, "Home"); // …from the evaluation that first sees it true again (1 Hz)
        assert!(d.snapshot(18.0).candidate.is_empty());
        d.tick(27.0, "Home");
        assert!(d.snapshot(27.0).candidate.is_empty());
        d.tick(28.0, "Home");
        assert_eq!(d.snapshot(28.0).candidate, "storm");
        // A quake outranks the pending storm.
        d.observe(quake(30.0, 6.1));
        d.tick(30.0, "Home");
        assert_eq!(d.snapshot(30.0).candidate, "quake");
        assert_eq!(loads(&d.tick(60.0, "Home")), vec!["Seismic"]);
        // Storm keeps firing (rate still high) but the quake's scene holds for min_hold.
        assert!(d.tick(100.0, "Seismic").is_empty());
        assert_eq!(d.snapshot(100.0).candidate, "storm");
        assert_eq!(loads(&d.tick(120.0, "Seismic")), vec!["Storm"]);
        assert_eq!(d.snapshot(120.0).reason, "lightning rate 3 > 2");
        // Another big quake inside the cooldown is ignored; the storm scene stays.
        d.observe(quake(200.0, 7.0));
        assert!(d.tick(200.0, "Storm").is_empty());
        assert!(d.snapshot(200.0).candidate.is_empty());
        // After the storm's hold (90 s) with the rate still high, storm is in cooldown too:
        // back to the default.
        assert_eq!(loads(&d.tick(210.0, "Storm")), vec!["Home"]);
        // Past the quake rule's cooldown (300 s from t=60) a quake fires again.
        d.observe(quake(400.0, 6.0));
        assert_eq!(loads(&d.tick(400.0, "Home")), vec!["Seismic"]);
    }

    #[test]
    fn firing_while_already_in_the_scene_extends_the_hold_without_reloading() {
        let mut d = dj(QUAKES_AND_STORMS);
        d.tick(0.0, "Seismic");
        d.observe(quake(70.0, 5.9));
        let actions = d.tick(70.0, "Seismic");
        assert!(loads(&actions).is_empty());
        assert!(matches!(&actions[0], Action::Caption(t, _) if t.starts_with("DJ stays on Seismic")));
        assert_eq!(d.snapshot(70.0).rule, "quake");
    }

    #[test]
    fn disabling_pausing_and_reenabling() {
        let mut d = dj(QUAKES_AND_STORMS);
        d.tick(0.0, "Home");
        d.set_enabled(false, 1.0);
        d.observe(quake(70.0, 6.0));
        assert!(d.tick(70.0, "Home").is_empty());
        let snap = d.snapshot(70.0);
        assert!(!snap.enabled && snap.reason == "off" && snap.hold_remaining == 0.0);
        // Re-enabled: the hold restarts, the old event was dropped.
        d.set_enabled(true, 71.0);
        assert!(d.tick(140.0, "Home").is_empty());
        assert_eq!(d.snapshot(140.0).reason, "watching");
        d.observe(quake(141.0, 6.0));
        assert_eq!(loads(&d.tick(141.0, "Home")), vec!["Seismic"]);
        // The user picks a scene: paused until told otherwise.
        d.pause_for_user();
        assert!(!d.enabled());
        assert!(d.snapshot(142.0).reason.starts_with("paused"));
        d.observe(quake(500.0, 7.0));
        assert!(d.tick(500.0, "Storm").is_empty());
    }

    #[test]
    fn reasons_fall_back_when_there_is_no_caption_template() {
        let mut attrs = BTreeMap::new();
        attrs.insert("n".to_string(), Value::Num(3.0));
        let f = ("n".to_string(), Op::Ge, Operand::Num(1.0));
        assert_eq!(event_reason("tick", 0.5, &attrs, Some(&f)), "tick (n 3)");
        assert_eq!(event_reason("tick", 0.5, &attrs, None), "tick");
        let fi = ("intensity".to_string(), Op::Ge, Operand::Num(0.1));
        assert_eq!(event_reason("tick", 0.5, &attrs, Some(&fi)), "tick (intensity 0.5)");
        assert_eq!(reading_reason("noaa.space-weather", "kp", 6.33, Op::Ge, 6.0), "space-weather kp 6.33 >= 6");
    }

    #[test]
    fn default_config_parses_and_resolves_builtin_scenes() {
        let d = Dj::from_toml(DEFAULT_CONFIG, None).unwrap();
        assert!(d.watch_specs().len() >= 2);
        assert_eq!(d.settings().default, "social-firehose");
    }

    #[cfg(feature = "native")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watchers_spawn_from_the_registry_and_feed_the_rules() {
        let src = r#"
[dj]
min_hold = 0
cooldown = 0
default = "Home"
[[rule]]
name = "ticks"
when = "synthetic.walk: event tick with n >= 2"
scene = "Walk"
hold = 5
params = { event_every = 0.2, rate_hz = 5 }
"#;
        let mut d = dj(src);
        let registry = crate::live::registry();
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let clock: std::sync::Arc<dyn sonder_core::Clock> = std::sync::Arc::new(sonder_core::WallClock::new());
        let tasks = crate::control::spawn_sources(&registry, d.watch_specs(), tx, clock.clone(), shutdown_rx, 1).unwrap();
        assert_eq!(tasks.len(), 1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut switched = None;
        while switched.is_none() && tokio::time::Instant::now() < deadline {
            let Ok(Some(s)) = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await else { continue };
            assert_eq!(s.source.as_str(), "synthetic.walk");
            d.observe(s);
            let now = clock.now();
            if let Some(Action::Load { scene, caption }) = d.tick(now, "Home").into_iter().next() {
                switched = Some((scene.name.clone(), caption));
            }
        }
        let (name, caption) = switched.expect("the walk's tick events never fired the rule");
        assert_eq!(name, "Walk");
        assert!(caption.starts_with("DJ → Walk: tick (n "), "{caption}");
        let _ = shutdown.send(true);
        for t in tasks {
            t.abort();
        }
    }
}
