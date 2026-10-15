//! Engine-side source mapping: reshaping the raw events a generic transport emits
//! (`websocket`, `sse`, `http-json`, …) into named, weighted, attributed events — in the
//! shared engine, so native and browser builds behave identically.
//!
//! Declared on a source spec:
//!
//! ```toml
//! [spec.event]
//! name = "trade"
//! when = "attr('qty') > 0"
//! intensity = "clamp(log10(1 + attr('price') * attr('qty')) / 5, 0, 1)"
//! attrs = { product = "symbol", price = "price" }      # new name = raw attr key (type kept)
//! derive = { usd = "attr('price') * attr('qty')" }     # new name = expression (number)
//! readings = { price = "attr('price')" }               # channel = expression → a Reading too
//! keep_raw = false
//! ```
//!
//! `event` may also be a list of such tables (first whose `when` passes wins).

use std::collections::BTreeMap;

use serde::Deserialize;
use sonder_core::{Datum, Sample, SourceSpec, Value};
use sonder_expr::{EventCtx, EventExpr};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct EventMappingConfig {
    name: Option<String>,
    when: Option<String>,
    intensity: Option<String>,
    attrs: BTreeMap<String, String>,
    derive: BTreeMap<String, String>,
    readings: BTreeMap<String, String>,
    keep_raw: bool,
}

#[derive(Debug)]
struct EventMapping {
    name: Option<String>,
    when: Option<EventExpr>,
    intensity: Option<EventExpr>,
    attrs: Vec<(String, String)>,
    derive: Vec<(String, EventExpr)>,
    readings: Vec<(String, EventExpr)>,
    keep_raw: bool,
}

/// Compiled mapping for one source.
#[derive(Debug, Default)]
pub struct SourceMapping {
    events: Vec<EventMapping>,
}

impl SourceMapping {
    /// Compile the `event` key of a spec, if present. `Ok(None)` when the spec has none.
    pub fn from_spec(spec: &SourceSpec) -> Result<Option<Self>, String> {
        let Some(raw) = spec.params.get("event") else { return Ok(None) };
        let configs: Vec<EventMappingConfig> = match raw {
            serde_json::Value::Array(items) => items
                .iter()
                .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("event mapping: {e}")))
                .collect::<Result<_, _>>()?,
            serde_json::Value::Object(_) => vec![serde_json::from_value(raw.clone()).map_err(|e| format!("event mapping: {e}"))?],
            _ => return Err("`event` must be a table or a list of tables".into()),
        };
        let no_signals: Vec<String> = Vec::new();
        let compile = |key: &str, src: &str| -> Result<EventExpr, String> {
            EventExpr::compile(src, &no_signals).map_err(|e| format!("event mapping `{key}`: {e}"))
        };
        let mut events = Vec::new();
        for c in configs {
            events.push(EventMapping {
                name: c.name,
                when: c.when.as_deref().map(|s| compile("when", s)).transpose()?,
                intensity: c.intensity.as_deref().map(|s| compile("intensity", s)).transpose()?,
                attrs: c.attrs.into_iter().collect(),
                derive: c.derive.into_iter().map(|(k, s)| compile(&k, &s).map(|e| (k, e))).collect::<Result<_, _>>()?,
                readings: c.readings.into_iter().map(|(k, s)| compile(&k, &s).map(|e| (k, e))).collect::<Result<_, _>>()?,
                keep_raw: c.keep_raw,
            });
        }
        Ok(Some(Self { events }))
    }

    /// Apply to a sample. Events may be rewritten (or dropped when no `when` passes and
    /// a `when` was declared); readings may be added. Non-events pass through untouched.
    pub fn apply(&self, sample: Sample, beat: f64) -> Vec<Sample> {
        let Datum::Event { name, intensity, attrs } = &sample.datum else { return vec![sample] };
        let signals = BTreeMap::new();
        let ctx = EventCtx { intensity: *intensity, attrs, signals: &signals, time: sample.at, beat };
        let Some(m) = self.events.iter().find(|m| m.when.as_ref().is_none_or(|w| w.eval(&ctx) != 0.0)) else {
            // Every candidate had a `when` and none passed → the event is filtered out.
            return Vec::new();
        };
        let mut new_attrs: BTreeMap<String, Value> = if m.keep_raw { attrs.clone() } else { BTreeMap::new() };
        for (new, raw) in &m.attrs {
            if let Some(v) = attrs.get(raw) {
                new_attrs.insert(new.clone(), v.clone());
            }
        }
        for (k, e) in &m.derive {
            new_attrs.insert(k.clone(), Value::Num(sanitize(e.eval(&ctx))));
        }
        let new_intensity = m.intensity.as_ref().map(|e| sanitize(e.eval(&ctx)).clamp(0.0, 1.0)).unwrap_or(*intensity);
        let mut out = Vec::with_capacity(2);
        if !m.readings.is_empty() {
            let values: BTreeMap<String, f64> = m.readings.iter().map(|(k, e)| (k.clone(), sanitize(e.eval(&ctx)))).collect();
            out.push(Sample { source: sample.source.clone(), at: sample.at, datum: Datum::Reading { values } });
        }
        out.push(Sample {
            source: sample.source.clone(),
            at: sample.at,
            datum: Datum::Event { name: m.name.clone().unwrap_or_else(|| name.clone()), intensity: new_intensity, attrs: new_attrs },
        });
        out
    }
}

fn sanitize(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::SourceId;

    fn spec(event: serde_json::Value) -> SourceSpec {
        let mut s = SourceSpec::new("websocket");
        s.params.insert("event".into(), event);
        s
    }

    #[test]
    fn maps_a_kraken_style_trade() {
        let m = SourceMapping::from_spec(&spec(serde_json::json!({
            "name": "trade",
            "intensity": "clamp(log10(1 + attr('price') * attr('qty')) / 5, 0, 1)",
            "attrs": {"product": "symbol", "price": "price", "side": "side"},
            "derive": {"usd": "attr('price') * attr('qty')", "buy": "if(attr_text('side') == 'buy', 1, 0)"},
            "readings": {"price": "attr('price')"}
        })))
        .unwrap()
        .unwrap();
        let raw = Sample::event(&SourceId::new("k"), 1.0, "message", 1.0)
            .with_attr("symbol", "BTC/USD")
            .with_attr("price", "80000.5")
            .with_attr("qty", 0.25)
            .with_attr("side", "buy");
        let out = m.apply(raw, 0.0);
        assert_eq!(out.len(), 2);
        match &out[0].datum {
            Datum::Reading { values } => assert!((values["price"] - 80000.5).abs() < 1e-6),
            _ => panic!(),
        }
        match &out[1].datum {
            Datum::Event { name, intensity, attrs } => {
                assert_eq!(name, "trade");
                assert!((attrs["usd"].as_f64().unwrap() - 20000.125).abs() < 1e-3);
                assert_eq!(attrs["buy"].as_f64(), Some(1.0));
                assert_eq!(attrs["product"].as_str(), Some("BTC/USD"));
                assert!(!attrs.contains_key("qty"), "raw attrs dropped unless keep_raw");
                assert!((intensity - (1.0 + 20000.125f64).log10() / 5.0).abs() < 1e-6);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn when_filters_and_list_picks_first_match() {
        let m = SourceMapping::from_spec(&spec(serde_json::json!([
            {"name": "big", "when": "attr('v') > 10"},
            {"name": "small", "when": "attr('v') <= 10"}
        ])))
        .unwrap()
        .unwrap();
        let big = m.apply(Sample::event(&SourceId::new("s"), 0.0, "x", 0.5).with_attr("v", 50.0), 0.0);
        let small = m.apply(Sample::event(&SourceId::new("s"), 0.0, "x", 0.5).with_attr("v", 1.0), 0.0);
        assert!(matches!(&big[0].datum, Datum::Event { name, .. } if name == "big"));
        assert!(matches!(&small[0].datum, Datum::Event { name, .. } if name == "small"));
        let only_big = SourceMapping::from_spec(&spec(serde_json::json!({"name": "big", "when": "attr('v') > 10"}))).unwrap().unwrap();
        assert!(only_big.apply(Sample::event(&SourceId::new("s"), 0.0, "x", 0.5).with_attr("v", 1.0), 0.0).is_empty());
    }

    #[test]
    fn bad_expression_is_an_error_and_readings_pass_through() {
        assert!(SourceMapping::from_spec(&spec(serde_json::json!({"intensity": "nope("}))).is_err());
        let m = SourceMapping::from_spec(&spec(serde_json::json!({"name": "t"}))).unwrap().unwrap();
        let r = Sample::reading(&SourceId::new("s"), 0.0, [("a", 1.0)]);
        assert_eq!(m.apply(r.clone(), 0.0), vec![r]);
    }
}
