//! Compiled `on_event` mappings. This is the only module that talks to
//! `sonder-expr`, so the rest of the composer is insulated from its API.

use std::collections::BTreeMap;

use sonder_core::{event_keys, Drum, LayerConfig, LayerEvent, Role, Seconds};
use sonder_expr::{parse_grid, EventCtx, EventExpr};

use crate::ComposeError;

/// Which pitch set an event's `pitch` snaps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Snap {
    #[default]
    Chord,
    Scale,
    None,
}

impl Snap {
    fn parse(s: &str) -> Option<Snap> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "chord" => Snap::Chord,
            "scale" => Snap::Scale,
            "none" | "off" | "free" => Snap::None,
            _ => return None,
        })
    }
}

/// A resolved trigger for one event: everything a pluck / bell / drum needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    /// 0..1 position in the register window.
    pub pitch: f64,
    /// 0..1
    pub velocity: f64,
    /// 0..1
    pub pan: f64,
    /// Note length in beats; None = role default.
    pub length_beats: Option<f64>,
    /// 0..1 probability of sounding.
    pub chance: f64,
    /// Grid in beats to defer onto; None = play now.
    pub quantize: Option<f64>,
    pub drum: Option<Drum>,
    pub snap: Snap,
}

impl Trigger {
    /// The role default when a layer has no mapping for an event:
    /// pitch = intensity, velocity = 0.5 + 0.5·intensity, centre pan.
    pub fn default_for(ev: &LayerEvent) -> Self {
        let i = if ev.intensity.is_finite() {
            ev.intensity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            pitch: i,
            velocity: 0.5 + 0.5 * i,
            pan: 0.5,
            length_beats: None,
            chance: 1.0,
            quantize: None,
            drum: None,
            snap: Snap::Chord,
        }
    }
}

/// One compiled `on_event.<name>` table.
#[derive(Debug)]
pub struct Mapping {
    pitch: Option<EventExpr>,
    velocity: Option<EventExpr>,
    pan: Option<EventExpr>,
    length: Option<EventExpr>,
    chance: Option<EventExpr>,
    quantize: Option<f64>,
    drum: Option<Drum>,
    snap: Snap,
}

impl Mapping {
    fn compile(
        layer: &str,
        event: &str,
        table: &BTreeMap<String, String>,
        signals: &[String],
    ) -> Result<Self, ComposeError> {
        let expr = |key: &str| -> Result<Option<EventExpr>, ComposeError> {
            match table.get(key) {
                None => Ok(None),
                Some(src) => EventExpr::compile(src, signals).map(Some).map_err(|e| {
                    ComposeError::EventExpr {
                        layer: layer.to_string(),
                        event: event.to_string(),
                        key: key.to_string(),
                        message: e.to_string(),
                    }
                }),
            }
        };
        let quantize = match table.get(event_keys::QUANTIZE) {
            None => None,
            Some(g) => parse_grid(g).map_err(|e| ComposeError::EventExpr {
                layer: layer.to_string(),
                event: event.to_string(),
                key: event_keys::QUANTIZE.to_string(),
                message: e.to_string(),
            })?,
        };
        let drum = match table.get(event_keys::DRUM) {
            None => None,
            Some(d) => Some(Drum::parse(d).ok_or_else(|| ComposeError::UnknownDrum {
                layer: layer.to_string(),
                event: event.to_string(),
                drum: d.clone(),
            })?),
        };
        let snap = match table.get(event_keys::SNAP) {
            None => Snap::Chord,
            Some(s) => Snap::parse(s).ok_or_else(|| ComposeError::UnknownSnap {
                layer: layer.to_string(),
                event: event.to_string(),
                snap: s.clone(),
            })?,
        };
        Ok(Self {
            pitch: expr(event_keys::PITCH)?,
            velocity: expr(event_keys::VELOCITY)?,
            pan: expr(event_keys::PAN)?,
            length: expr(event_keys::LENGTH)?,
            chance: expr(event_keys::CHANCE)?,
            quantize,
            drum,
            snap,
        })
    }

    fn eval(
        &self,
        ev: &LayerEvent,
        signals: &BTreeMap<String, f64>,
        time: Seconds,
        beat: f64,
    ) -> Trigger {
        let ctx = EventCtx {
            intensity: ev.intensity,
            attrs: &ev.attrs,
            signals,
            time,
            beat,
        };
        let mut t = Trigger::default_for(ev);
        let ev01 = |e: &Option<EventExpr>, fallback: f64| -> f64 {
            match e {
                Some(x) => {
                    let v = x.eval(&ctx);
                    if v.is_finite() {
                        v.clamp(0.0, 1.0)
                    } else {
                        fallback
                    }
                }
                None => fallback,
            }
        };
        t.pitch = ev01(&self.pitch, t.pitch);
        t.velocity = ev01(&self.velocity, t.velocity);
        t.pan = ev01(&self.pan, t.pan);
        t.chance = ev01(&self.chance, 1.0);
        if let Some(l) = &self.length {
            let v = l.eval(&ctx);
            if v.is_finite() && v > 0.0 {
                t.length_beats = Some(v.min(64.0));
            }
        }
        t.quantize = self.quantize;
        t.drum = self.drum;
        t.snap = self.snap;
        t
    }
}

/// All compiled mappings of one layer. A specific event name wins over `"*"`.
#[derive(Debug, Default)]
pub struct LayerMappings {
    by_name: BTreeMap<String, Mapping>,
    wildcard: Option<Mapping>,
}

impl LayerMappings {
    pub fn compile(cfg: &LayerConfig) -> Result<Self, ComposeError> {
        let signals: Vec<String> = cfg.signals.keys().cloned().collect();
        let mut out = Self::default();
        for (event, table) in &cfg.on_event {
            let m = Mapping::compile(&cfg.name, event, table, &signals)?;
            if event == "*" {
                out.wildcard = Some(m);
            } else {
                out.by_name.insert(event.clone(), m);
            }
        }
        Ok(out)
    }

    /// Does the layer define any `on_event` table at all?
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty() && self.wildcard.is_none()
    }

    /// Resolve an event to a trigger: named mapping, else wildcard, else the
    /// role default.
    pub fn trigger(
        &self,
        ev: &LayerEvent,
        signals: &BTreeMap<String, f64>,
        time: Seconds,
        beat: f64,
    ) -> Trigger {
        let m = self.by_name.get(&ev.name).or(self.wildcard.as_ref());
        match m {
            Some(m) => m.eval(ev, signals, time, beat),
            None => Trigger::default_for(ev),
        }
    }
}

/// Default note length (beats) for an event-driven role.
pub fn default_length_beats(role: Role) -> f64 {
    match role {
        Role::Bells => 4.0,
        _ => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{LayerId, Value};

    fn ev(name: &str, intensity: f64) -> LayerEvent {
        let mut attrs = BTreeMap::new();
        attrs.insert("lane".to_string(), Value::Num(2.0));
        LayerEvent {
            layer: LayerId(0),
            name: name.into(),
            at: 1.0,
            intensity,
            attrs,
        }
    }

    #[test]
    fn specific_beats_wildcard_beats_default() {
        let mut cfg = LayerConfig {
            name: "l".into(),
            role: Role::Pluck,
            ..Default::default()
        };
        let mut car = BTreeMap::new();
        car.insert("pitch".to_string(), "attr('lane') / 4".to_string());
        car.insert("quantize".to_string(), "1/16".to_string());
        car.insert("length".to_string(), "2".to_string());
        cfg.on_event.insert("car".into(), car);
        let mut any = BTreeMap::new();
        any.insert("velocity".to_string(), "0.1".to_string());
        any.insert("snap".to_string(), "scale".to_string());
        cfg.on_event.insert("*".into(), any);
        let m = LayerMappings::compile(&cfg).unwrap();
        let sig = BTreeMap::new();
        let t = m.trigger(&ev("car", 0.9), &sig, 1.0, 2.0);
        assert!((t.pitch - 0.5).abs() < 1e-9);
        assert_eq!(t.quantize, Some(0.25));
        assert_eq!(t.length_beats, Some(2.0));
        assert_eq!(t.snap, Snap::Chord);
        let t = m.trigger(&ev("truck", 0.9), &sig, 1.0, 2.0);
        assert!((t.velocity - 0.1).abs() < 1e-9);
        assert_eq!(t.snap, Snap::Scale);
        let empty = LayerMappings::default();
        let t = empty.trigger(&ev("x", 0.4), &sig, 0.0, 0.0);
        assert!((t.pitch - 0.4).abs() < 1e-9 && (t.velocity - 0.7).abs() < 1e-9);
    }

    #[test]
    fn non_finite_intensity_yields_finite_trigger() {
        let t = Trigger::default_for(&ev("x", f64::NAN));
        assert!(t.pitch.is_finite() && t.velocity.is_finite(), "{t:?}");
        let mut cfg = LayerConfig {
            name: "l".into(),
            role: Role::Pluck,
            ..Default::default()
        };
        let mut car = BTreeMap::new();
        car.insert("pan".to_string(), "attr('lane') / 4".to_string());
        cfg.on_event.insert("car".into(), car);
        let m = LayerMappings::compile(&cfg).unwrap();
        let t = m.trigger(&ev("car", f64::INFINITY), &BTreeMap::new(), 0.0, 0.0);
        assert!(t.pitch.is_finite() && t.velocity.is_finite(), "{t:?}");
    }

    #[test]
    fn errors_name_the_offending_key() {
        let mut cfg = LayerConfig {
            name: "l".into(),
            ..Default::default()
        };
        let mut bad = BTreeMap::new();
        bad.insert("drum".to_string(), "cowbell".to_string());
        cfg.on_event.insert("hit".into(), bad);
        assert!(matches!(
            LayerMappings::compile(&cfg),
            Err(ComposeError::UnknownDrum { .. })
        ));
        let mut cfg = LayerConfig {
            name: "l".into(),
            ..Default::default()
        };
        let mut bad = BTreeMap::new();
        bad.insert("pitch".to_string(), "nope(".to_string());
        cfg.on_event.insert("hit".into(), bad);
        match LayerMappings::compile(&cfg) {
            Err(ComposeError::EventExpr { key, .. }) => assert_eq!(key, "pitch"),
            other => panic!("{other:?}"),
        }
    }
}
