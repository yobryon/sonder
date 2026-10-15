//! Raw data as it leaves a source.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::time::Seconds;

/// Identifies a source instance within a running engine. Sources are shared by
/// name between layers (see [`crate::scene::LayerSource`]), so this is the name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub Arc<str>);

impl SourceId {
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(Arc::from(s.as_ref()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SourceId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// An attribute value attached to an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Num(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    /// Numeric view: numbers as-is, bools as 0/1, text parsed if it parses.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Text(t) => t.trim().parse().ok(),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Num(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Num(v as f64)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

/// Health of a source as reported by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Connecting,
    Live,
    /// Connected but the upstream is quiet (no data in a while).
    Idle,
    /// Lost connection; retrying.
    Reconnecting,
    /// Gave up or misconfigured.
    Failed,
}

/// What a sample carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Datum {
    /// Latest values of continuous channels, e.g. `{ "mean_speed": 61.2, "count": 14 }`.
    /// Only channels present are updated; others keep their previous value.
    Reading { values: BTreeMap<String, f64> },
    /// A discrete occurrence, e.g. a car crossing the frame or an edit landing.
    Event {
        name: String,
        /// Rough "how big was this" in 0..1-ish units (sources are free to exceed 1;
        /// features normalize).
        intensity: f64,
        #[serde(default)]
        attrs: BTreeMap<String, Value>,
    },
    /// Source health / log line for the UI.
    Status { state: SourceState, message: String },
    /// Raw audio from a source (hydrophone, mic, radio) for granular instruments on the
    /// layers that read it. Mono, `rate` Hz, chunks of ~20–100 ms.
    Audio { rate: u32, samples: Vec<f32> },
    /// Opaque visualization payload the source wants the UI to draw
    /// (e.g. freeway car positions). Passed through to [`crate::snapshot::EngineSnapshot::viz`].
    Viz { payload: serde_json::Value },
}

/// A timestamped datum from a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub source: SourceId,
    /// Engine time the sample was produced (see [`crate::time`]).
    pub at: Seconds,
    pub datum: Datum,
}

impl Sample {
    pub fn reading(source: &SourceId, at: Seconds, values: impl IntoIterator<Item = (impl Into<String>, f64)>) -> Self {
        Self {
            source: source.clone(),
            at,
            datum: Datum::Reading { values: values.into_iter().map(|(k, v)| (k.into(), v)).collect() },
        }
    }
    pub fn event(source: &SourceId, at: Seconds, name: impl Into<String>, intensity: f64) -> Self {
        Self {
            source: source.clone(),
            at,
            datum: Datum::Event { name: name.into(), intensity, attrs: BTreeMap::new() },
        }
    }
    /// Builder: attach an attribute to an event sample (no-op on non-events).
    pub fn with_attr(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        if let Datum::Event { attrs, .. } = &mut self.datum {
            attrs.insert(key.into(), value.into());
        }
        self
    }
    pub fn status(source: &SourceId, at: Seconds, state: SourceState, message: impl Into<String>) -> Self {
        Self { source: source.clone(), at, datum: Datum::Status { state, message: message.into() } }
    }
    pub fn viz(source: &SourceId, at: Seconds, payload: serde_json::Value) -> Self {
        Self { source: source.clone(), at, datum: Datum::Viz { payload } }
    }
}

/// A discrete event after it has been attributed to a layer (post-features).
/// This is what generators react to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerEvent {
    pub layer: crate::music::LayerId,
    pub name: String,
    pub at: Seconds,
    pub intensity: f64,
    pub attrs: BTreeMap<String, Value>,
}
