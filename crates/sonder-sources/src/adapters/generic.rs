//! Shared logic for the generic adapters (`http-json`, `sse`, `websocket`,
//! `file-tail`, `stdin`): turning an arbitrary JSON object into a Reading of its
//! numeric leaves plus (per policy) an Event carrying its top-level scalars.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use sonder_core::{Datum, SourceSpec, Value};

use crate::jsonpath;
use crate::util::{invalid, param_str, param_str_list};
use crate::SourceError;

/// Maximum numeric channels emitted per object.
pub const MAX_FIELDS: usize = 64;
/// Maximum event attributes per object.
pub const MAX_ATTRS: usize = 16;

/// When a generic adapter emits an Event for an incoming object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventsOn {
    /// Only when the flattened reading differs from the previous one.
    Change,
    /// On every object.
    Every,
    /// Never.
    None,
}

impl EventsOn {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "change" | "changed" => EventsOn::Change,
            "every" | "always" | "all" => EventsOn::Every,
            "none" | "never" | "off" => EventsOn::None,
            _ => return None,
        })
    }
}

/// Field selection: explicit paths or everything numeric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fields {
    Auto,
    Paths(Vec<String>),
}

/// Configuration shared by the generic adapters.
#[derive(Debug, Clone)]
pub struct GenericConfig {
    pub fields: Fields,
    pub events_on: EventsOn,
    pub event_name: String,
    /// Optional sub-path of each message to treat as the object.
    pub data_path: Option<String>,
    /// Optional `(path, value)` filter: only objects whose `path` equals `value` pass.
    pub filter: Option<(String, Json)>,
}

impl GenericConfig {
    /// Parse `fields`, `events_on`, `event_name`, `data_path`, `filter` from a spec.
    pub fn from_spec(spec: &SourceSpec, default_events_on: EventsOn, default_event: &str) -> Result<Self, SourceError> {
        let fields = match param_str_list(spec, "fields") {
            None => Fields::Auto,
            Some(list) if list.len() == 1 && list[0].eq_ignore_ascii_case("auto") => Fields::Auto,
            Some(list) if list.is_empty() => Fields::Auto,
            Some(list) => {
                for p in &list {
                    if jsonpath::parse(p).is_none() {
                        return Err(invalid(spec, "fields", &format!("bad path `{p}`")));
                    }
                }
                Fields::Paths(list)
            }
        };
        let events_on = match spec.get_str("events_on") {
            None => default_events_on,
            Some(s) => EventsOn::parse(s)
                .ok_or_else(|| invalid(spec, "events_on", "expected one of change|every|none"))?,
        };
        let event_name = param_str(spec, "event_name", default_event);
        let data_path = spec.get_str("data_path").map(str::to_string).filter(|s| !s.trim().is_empty());
        if let Some(p) = &data_path {
            if jsonpath::parse(p).is_none() {
                return Err(invalid(spec, "data_path", "bad path"));
            }
        }
        let filter = match spec.params.get("filter") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => {
                // "path==value" shorthand.
                let (p, v) = s.split_once("==").ok_or_else(|| invalid(spec, "filter", "expected `path==value`"))?;
                let v = v.trim();
                let vj = serde_json::from_str::<Json>(v).unwrap_or_else(|_| Json::String(v.to_string()));
                Some((p.trim().to_string(), vj))
            }
            Some(Json::Object(m)) => {
                let p = m.get("path").and_then(Json::as_str).ok_or_else(|| invalid(spec, "filter", "missing `path`"))?;
                let v = m.get("value").cloned().ok_or_else(|| invalid(spec, "filter", "missing `value`"))?;
                Some((p.to_string(), v))
            }
            Some(_) => return Err(invalid(spec, "filter", "expected `path==value` or {path, value}")),
        };
        Ok(Self { fields, events_on, event_name, data_path, filter })
    }
}

/// Stateful converter: remembers the previous reading for `events_on = change`.
#[derive(Debug, Clone)]
pub struct Converter {
    pub cfg: GenericConfig,
    prev: Option<BTreeMap<String, f64>>,
}

impl Converter {
    pub fn new(cfg: GenericConfig) -> Self {
        Self { cfg, prev: None }
    }

    /// Convert one incoming message into zero or more data. Returns an empty vec if
    /// the message is filtered out or has no usable content.
    pub fn convert(&mut self, msg: &Json) -> Vec<Datum> {
        let mut out = Vec::new();
        let obj = match &self.cfg.data_path {
            Some(p) => match jsonpath::extract(msg, p) {
                Some(v) => v,
                None => return out,
            },
            None => msg,
        };
        if let Some((p, want)) = &self.cfg.filter {
            match jsonpath::extract(obj, p) {
                Some(got) if json_eq(got, want) => {}
                _ => return out,
            }
        }
        let values = self.reading_of(obj);
        let changed = self.prev.as_ref() != Some(&values);
        let emit_event = match self.cfg.events_on {
            EventsOn::Every => true,
            EventsOn::Change => changed && self.prev.is_some(),
            EventsOn::None => false,
        };
        if !values.is_empty() {
            out.push(Datum::Reading { values: values.clone() });
        }
        if emit_event {
            out.push(Datum::Event { name: self.cfg.event_name.clone(), intensity: 1.0, attrs: attrs_of(obj) });
        }
        self.prev = Some(values);
        out
    }

    /// Numeric channels of `obj` according to the field selection.
    pub fn reading_of(&self, obj: &Json) -> BTreeMap<String, f64> {
        match &self.cfg.fields {
            Fields::Auto => {
                let flat = jsonpath::flatten_numeric(obj, "");
                flat.into_iter().take(MAX_FIELDS).collect()
            }
            Fields::Paths(paths) => {
                let mut m = BTreeMap::new();
                for p in paths.iter().take(MAX_FIELDS) {
                    if let Some(v) = jsonpath::extract_f64(obj, p) {
                        m.insert(channel_name(p), v);
                    }
                }
                m
            }
        }
    }
}

/// Reading channel name for a path: strip leading `.`/`$` and the brackets
/// (`.a.b[0]` → `a.b[0]`, `["weird key"]` → `weird key`).
pub fn channel_name(path: &str) -> String {
    let segs = jsonpath::parse(path).unwrap_or_default();
    let mut s = String::new();
    for seg in segs {
        match seg {
            jsonpath::Seg::Key(k) => {
                if !s.is_empty() {
                    s.push('.');
                }
                s.push_str(&k);
            }
            jsonpath::Seg::Index(i) => s.push_str(&format!("[{i}]")),
        }
    }
    if s.is_empty() {
        path.trim().trim_start_matches(['.', '$']).to_string()
    } else {
        s
    }
}

/// Top-level scalar attributes of an object (numbers, bools, strings; capped).
pub fn attrs_of(obj: &Json) -> BTreeMap<String, Value> {
    let mut attrs = BTreeMap::new();
    match obj {
        Json::Object(m) => {
            for (k, v) in m {
                if attrs.len() >= MAX_ATTRS {
                    break;
                }
                let val = match v {
                    Json::Number(n) => n.as_f64().map(Value::Num),
                    Json::Bool(b) => Some(Value::Bool(*b)),
                    Json::String(s) => Some(Value::Text(s.chars().take(256).collect())),
                    _ => None,
                };
                if let Some(val) = val {
                    attrs.insert(k.clone(), val);
                }
            }
        }
        Json::Number(n) => {
            if let Some(f) = n.as_f64() {
                attrs.insert("value".into(), Value::Num(f));
            }
        }
        Json::String(s) => {
            attrs.insert("text".into(), Value::Text(s.chars().take(256).collect()));
        }
        Json::Bool(b) => {
            attrs.insert("value".into(), Value::Bool(*b));
        }
        _ => {}
    }
    attrs
}

/// Loose equality: numbers compare numerically, strings compare to the stringified other side.
pub fn json_eq(a: &Json, b: &Json) -> bool {
    if a == b {
        return true;
    }
    match (jsonpath::to_f64(a), jsonpath::to_f64(b)) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
        _ => match (a, b) {
            (Json::String(s), other) | (other, Json::String(s)) => match other {
                Json::String(o) => s == o,
                Json::Bool(bo) => s == &bo.to_string(),
                Json::Number(n) => s == &n.to_string(),
                _ => false,
            },
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(events: EventsOn) -> GenericConfig {
        GenericConfig { fields: Fields::Auto, events_on: events, event_name: "update".into(), data_path: None, filter: None }
    }

    #[test]
    fn auto_fields_and_change_policy() {
        let mut c = Converter::new(cfg(EventsOn::Change));
        let d1 = c.convert(&json!({"a": 1, "b": {"c": 2}, "s": "x"}));
        assert_eq!(d1.len(), 1); // first object: reading only, no "change" yet
        let d2 = c.convert(&json!({"a": 1, "b": {"c": 2}, "s": "y"}));
        assert_eq!(d2.len(), 1); // same numerics → no event
        let d3 = c.convert(&json!({"a": 2, "b": {"c": 2}, "s": "y"}));
        assert_eq!(d3.len(), 2);
        match &d3[1] {
            Datum::Event { name, intensity, attrs } => {
                assert_eq!(name, "update");
                assert_eq!(*intensity, 1.0);
                assert_eq!(attrs.get("s"), Some(&Value::Text("y".into())));
                assert_eq!(attrs.get("a"), Some(&Value::Num(2.0)));
                assert!(!attrs.contains_key("b"));
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn explicit_paths_filter_and_data_path() {
        let mut c = Converter::new(GenericConfig {
            fields: Fields::Paths(vec![".payload.x".into(), "payload.arr[1]".into()]),
            events_on: EventsOn::Every,
            event_name: "tick".into(),
            data_path: Some(".msg".into()),
            filter: Some((".type".into(), json!("data"))),
        });
        let d = c.convert(&json!({"msg": {"type": "data", "payload": {"x": 5, "arr": [1, 2]}}}));
        assert_eq!(d.len(), 2);
        match &d[0] {
            Datum::Reading { values } => {
                assert_eq!(values["payload.x"], 5.0);
                assert_eq!(values["payload.arr[1]"], 2.0);
            }
            _ => panic!(),
        }
        let none = c.convert(&json!({"msg": {"type": "heartbeat"}}));
        assert!(none.is_empty());
    }

    #[test]
    fn parses_spec() {
        let spec = SourceSpec::new("http-json").with("fields", json!(["a", ".b[0]"])).with("events_on", "every")
            .with("filter", "type==match");
        let g = GenericConfig::from_spec(&spec, EventsOn::Change, "update").unwrap();
        assert_eq!(g.events_on, EventsOn::Every);
        assert_eq!(g.filter, Some(("type".into(), json!("match"))));
        assert!(matches!(g.fields, Fields::Paths(ref p) if p.len() == 2));
        let bad = SourceSpec::new("http-json").with("events_on", "sometimes");
        assert!(GenericConfig::from_spec(&bad, EventsOn::Change, "update").is_err());
    }

    #[test]
    fn channel_names() {
        assert_eq!(channel_name(".a.b[0]"), "a.b[0]");
        assert_eq!(channel_name(r#"["weird key"].x"#), "weird key.x");
    }
}
