//! `sonder probe <url>`: poll a JSON endpoint a few times, see which numeric fields
//! move, suggest feature expressions, and build a starter scene.
//!
//! [`analyze`] and [`starter_scene`] are pure so they can be tested with canned JSON.

use std::fmt;
use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::{ConductorConfig, LayerConfig, LayerSource, Role, Scene, SourceSpec};

use crate::jsonpath;
use crate::util::{self, USER_AGENT};
use crate::SourceError;

/// How a probed field behaves over the polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Never changed.
    Constant,
    /// Only 0/1 values.
    Boolean,
    /// Only ever increases (a total, a sequence number…).
    Counter,
    /// Mostly flat with occasional jumps.
    Sparse,
    /// Changes most polls.
    Continuous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbedField {
    pub path: String,
    pub values: Vec<f64>,
    pub changed: bool,
    pub kind: FieldKind,
    /// A feature expression using `field('<path>')`.
    pub suggestion: String,
    /// Relative variation (std/|mean|), used to rank fields.
    pub variation: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    pub url: String,
    pub polls: usize,
    pub fields: Vec<ProbedField>,
    /// Suggested `events_on` policy for the source, if events make sense.
    pub events_suggestion: Option<String>,
    /// Top-level keys that were strings (useful as event attrs).
    pub text_keys: Vec<String>,
}

/// Classify a series of values.
pub fn classify(values: &[f64]) -> FieldKind {
    if values.len() < 2 {
        return FieldKind::Constant;
    }
    let changes = values.windows(2).filter(|w| (w[0] - w[1]).abs() > 1e-12).count();
    if changes == 0 {
        return FieldKind::Constant;
    }
    if values.iter().all(|v| *v == 0.0 || *v == 1.0) {
        return FieldKind::Boolean;
    }
    let non_decreasing = values.windows(2).all(|w| w[1] >= w[0] - 1e-12);
    if non_decreasing && changes >= 1 && values.len() >= 3 && changes * 2 >= values.len() - 1 {
        return FieldKind::Counter;
    }
    let pairs = values.len() - 1;
    if changes * 3 <= pairs {
        return FieldKind::Sparse;
    }
    if non_decreasing {
        return FieldKind::Counter;
    }
    FieldKind::Continuous
}

/// The suggested feature expression for a field of `kind`.
pub fn suggest(path: &str, kind: FieldKind) -> String {
    let f = format!("field('{}')", path.replace('\'', ""));
    match kind {
        FieldKind::Constant => format!("{f}  # constant"),
        FieldKind::Boolean => format!("onset({f}, 0.5)"),
        FieldKind::Counter => format!("norm(smooth(derivative({f}), 2))"),
        FieldKind::Sparse => format!("onset(abs(derivative({f})), 0.001)"),
        FieldKind::Continuous => format!("norm(smooth({f}, 2))"),
    }
}

/// Analyze successive JSON payloads of the same endpoint.
pub fn analyze(samples: &[Json], url: &str) -> ProbeReport {
    let mut order: Vec<String> = Vec::new();
    let mut series: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
    for s in samples {
        for (path, v) in jsonpath::flatten_numeric(s, "") {
            if !series.contains_key(&path) {
                order.push(path.clone());
            }
            series.entry(path).or_default().push(v);
        }
    }
    let mut fields = Vec::new();
    for path in order {
        let values = series.remove(&path).unwrap_or_default();
        let kind = classify(&values);
        let changed = kind != FieldKind::Constant;
        let variation = variation(&values);
        let suggestion = suggest(&path, kind);
        fields.push(ProbedField { path, values, changed, kind, suggestion, variation });
    }
    fields.truncate(crate::adapters::generic::MAX_FIELDS);
    let any_sparse = fields.iter().any(|f| matches!(f.kind, FieldKind::Sparse | FieldKind::Boolean | FieldKind::Counter));
    let any_change = fields.iter().any(|f| f.changed);
    let events_suggestion = if any_sparse || any_change {
        Some("change".to_string())
    } else if samples.len() < 2 {
        Some("change".to_string())
    } else {
        None
    };
    let text_keys = samples
        .first()
        .and_then(Json::as_object)
        .map(|m| m.iter().filter(|(_, v)| v.is_string()).map(|(k, _)| k.clone()).collect())
        .unwrap_or_default();
    ProbeReport { url: url.to_string(), polls: samples.len(), fields, events_suggestion, text_keys }
}

fn variation(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt();
    if mean.abs() > 1e-12 {
        sd / mean.abs()
    } else if sd > 0.0 {
        1.0
    } else {
        0.0
    }
}

impl ProbeReport {
    /// The field that moved most (continuous first, then counters), if any moved.
    pub fn most_varying(&self) -> Option<&ProbedField> {
        let rank = |f: &ProbedField| match f.kind {
            FieldKind::Continuous => 3,
            FieldKind::Counter => 2,
            FieldKind::Sparse | FieldKind::Boolean => 1,
            FieldKind::Constant => 0,
        };
        self.fields
            .iter()
            .filter(|f| f.changed)
            .max_by(|a, b| rank(a).cmp(&rank(b)).then(a.variation.partial_cmp(&b.variation).unwrap_or(std::cmp::Ordering::Equal)))
    }
}

impl fmt::Display for ProbeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Polled {}× {}", self.polls, self.url)?;
        let changed: Vec<&ProbedField> = self.fields.iter().filter(|x| x.changed).collect();
        if changed.is_empty() {
            writeln!(f, "  No numeric field changed over {} polls ({} numeric fields seen).", self.polls, self.fields.len())?;
        } else {
            writeln!(f, "  Numeric fields that changed:")?;
            for x in changed {
                let vals: Vec<String> = x.values.iter().map(|v| fmt_num(*v)).collect();
                writeln!(f, "    .{:<28} {}   ({:?}; suggest: {})", x.path, vals.join(" → "), x.kind, x.suggestion)?;
            }
        }
        let constant = self.fields.iter().filter(|x| !x.changed).count();
        if constant > 0 {
            writeln!(f, "  {constant} constant field(s) omitted.")?;
        }
        if let Some(e) = &self.events_suggestion {
            writeln!(f, "  Suggested events_on = \"{e}\"")?;
        }
        Ok(())
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.3}")
    }
}

/// Poll `url` `polls` times, `interval` apart, and analyze.
pub async fn probe_json(url: &str, polls: usize, interval: Duration) -> Result<ProbeReport, SourceError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(util::HTTP_TIMEOUT)
        .build()
        .map_err(|e| SourceError::Other(format!("http client: {e}")))?;
    let mut samples = Vec::new();
    for i in 0..polls.max(1) {
        if i > 0 {
            tokio::time::sleep(interval).await;
        }
        let resp = client.get(url).send().await.map_err(|e| SourceError::Other(format!("{url}: {}", util::short_err(&e))))?;
        if !resp.status().is_success() {
            return Err(SourceError::Other(format!("{url}: HTTP {}", resp.status())));
        }
        let text = resp.text().await.map_err(|e| SourceError::Other(format!("{url}: {}", util::short_err(&e))))?;
        let v: Json = serde_json::from_str(&text).map_err(|e| SourceError::Other(format!("{url}: not JSON: {e}")))?;
        samples.push(v);
    }
    Ok(analyze(&samples, url))
}

/// Build a reasonable starter scene for the probed endpoint: a pad driven by the
/// most-varying field, a pluck on `update` events, and a bass riding the conductor.
pub fn starter_scene(report: &ProbeReport, url: &str) -> Scene {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("feed").to_string();
    let lead = report.most_varying().or_else(|| report.fields.first());
    let lead_path = lead.map(|f| f.path.clone()).unwrap_or_else(|| "value".to_string());
    let lead_expr = lead.map(|f| suggest(&f.path, if f.kind == FieldKind::Constant { FieldKind::Continuous } else { f.kind }))
        .unwrap_or_else(|| "norm(smooth(field('value'), 2))".to_string());
    // Edge detectors make poor pad drivers; smooth the rate of them instead.
    let level_expr = if lead_expr.starts_with("onset(") {
        format!("norm(smooth(rate(event('update'), 30), 5))")
    } else {
        lead_expr
    };
    let mut source = SourceSpec::new("http-json").with("url", url).with("interval", 10);
    if let Some(e) = &report.events_suggestion {
        source = source.with("events_on", e.as_str());
    }
    let mut pad = LayerConfig { name: "feed".into(), role: Role::Pad, instrument: "pad.glass".into(), ..Default::default() };
    pad.source = LayerSource::Spec(source);
    pad.signals.insert("level".into(), level_expr);
    pad.map.insert("energy".into(), "level".into());
    pad.map.insert("brightness".into(), "0.3 + 0.6 * level".into());
    pad.map.insert("density".into(), "0.2 + 0.5 * level".into());

    let mut pluck = LayerConfig { name: "changes".into(), role: Role::Pluck, instrument: "pluck.glass".into(), ..Default::default() };
    pluck.source = LayerSource::Shared("feed".into());
    let pitch = if lead_path.contains('.') || lead_path.contains('[') {
        // Nested paths aren't event attrs; wander instead.
        "walk(0.2, 1)".to_string()
    } else {
        format!("hash(attr('{lead_path}'))")
    };
    let mut on_update = std::collections::BTreeMap::new();
    on_update.insert("pitch".into(), pitch);
    on_update.insert("velocity".into(), "0.5 + 0.4 * intensity".into());
    on_update.insert("quantize".into(), "1/16".into());
    pluck.on_event.insert("update".into(), on_update);

    let mut bass = LayerConfig { name: "ground".into(), role: Role::Bass, instrument: "bass.sub".into(), ..Default::default() };
    bass.map.insert("density".into(), "0.3".into());

    Scene {
        name: format!("probe {host}"),
        description: format!("Starter scene generated by `sonder probe {url}`"),
        tags: vec!["probe".into(), "live".into()],
        conductor: ConductorConfig { key: "D".into(), mode: "lydian".into(), tempo: 66.0, ..Default::default() },
        layers: vec![pad, pluck, bass],
        viz: "generic".into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn samples() -> Vec<Json> {
        vec![
            json!({"requests_per_sec": 12.1, "active_users": 812, "errors": 0, "version": "1.2", "up": true, "nested": {"q": 5}}),
            json!({"requests_per_sec": 14.8, "active_users": 815, "errors": 0, "version": "1.2", "up": true, "nested": {"q": 5}}),
            json!({"requests_per_sec": 13.9, "active_users": 819, "errors": 1, "version": "1.2", "up": true, "nested": {"q": 5}}),
        ]
    }

    #[test]
    fn classifies_fields() {
        let r = analyze(&samples(), "https://api.example.com/metrics.json");
        let by: std::collections::BTreeMap<_, _> = r.fields.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by["requests_per_sec"].kind, FieldKind::Continuous);
        assert!(by["requests_per_sec"].suggestion.contains("norm(smooth(field('requests_per_sec'), 2))"));
        assert_eq!(by["active_users"].kind, FieldKind::Counter);
        assert_eq!(by["errors"].kind, FieldKind::Boolean);
        assert_eq!(by["nested.q"].kind, FieldKind::Constant);
        assert!(!by["nested.q"].changed);
        assert_eq!(by["up"].kind, FieldKind::Constant);
        assert_eq!(r.events_suggestion.as_deref(), Some("change"));
        assert_eq!(r.text_keys, vec!["version".to_string()]);
        assert_eq!(r.most_varying().unwrap().path, "requests_per_sec");
        let text = r.to_string();
        assert!(text.contains("requests_per_sec"));
        assert!(text.contains("12.100 → 14.800 → 13.900"));
    }

    #[test]
    fn sparse_and_counter_detection() {
        assert_eq!(classify(&[0.0, 0.0, 0.0, 0.0, 3.0, 3.0, 3.0]), FieldKind::Sparse);
        assert_eq!(classify(&[1.0, 2.0, 3.0, 4.0]), FieldKind::Counter);
        assert_eq!(classify(&[1.0, 1.0]), FieldKind::Constant);
        assert_eq!(classify(&[1.0, 0.5, 2.0]), FieldKind::Continuous);
        assert_eq!(classify(&[5.0]), FieldKind::Constant);
    }

    #[test]
    fn builds_a_valid_starter_scene() {
        let r = analyze(&samples(), "https://api.example.com/metrics.json");
        let s = starter_scene(&r, "https://api.example.com/metrics.json");
        s.validate_structure().unwrap();
        assert_eq!(s.layers.len(), 3);
        assert!(s.layers[0].signals["level"].contains("requests_per_sec"));
        assert_eq!(s.layers[0].map["energy"], "level");
        match &s.layers[0].source {
            LayerSource::Spec(spec) => {
                assert_eq!(spec.kind, "http-json");
                assert_eq!(spec.get_str("events_on"), Some("change"));
            }
            _ => panic!(),
        }
        assert_eq!(s.layers[1].on_event["update"]["pitch"], "hash(attr('requests_per_sec'))");
        assert_eq!(s.layers[2].role, Role::Bass);
        // Round-trips through JSON (the TOML model is identical).
        let j = serde_json::to_string(&s).unwrap();
        let back: Scene = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn single_poll_report() {
        let r = analyze(&samples()[..1], "u");
        assert!(r.fields.iter().all(|f| !f.changed));
        let s = starter_scene(&r, "https://x/y");
        s.validate_structure().unwrap();
    }

    /// `cargo test -p sonder-sources -- --ignored probe`
    #[tokio::test]
    #[ignore]
    async fn live_probe() {
        let r = probe_json("http://api.open-notify.org/iss-now.json", 2, Duration::from_secs(2)).await.unwrap();
        assert!(r.fields.iter().any(|f| f.path == "iss_position.latitude"));
        println!("{r}");
    }
}
