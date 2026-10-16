//! The replay source (`kind = "replay.log"`): re-emits a [`DataLog`]'s samples under their
//! original source names at their original relative timing (scaled by `speed`), stepped
//! by the control loop in engine time. Lives in the engine, not in `sonder-sources`, so
//! native and wasm share it.
//!
//! Spec parameters: `path` (native), `speed` (1.0), `from` / `to` (log seconds), `loop`
//! (false), `rename` (table: logged source name → scene source name). In the browser the
//! bytes come from `Engine::load_log` instead of `path`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sonder_core::{ReplaySnapshot, Sample, Seconds, SourceId, SourceSpec};

use crate::datalog::{DataLog, LogEntry};

/// The source kind a scene uses to play a log.
pub const KIND: &str = "replay.log";

/// How far before a seek target the features are warmed (fed without rendering).
pub const WARM_SECONDS: f64 = 30.0;

/// Playback options.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayConfig {
    pub speed: f64,
    /// Log seconds to start from.
    pub from: f64,
    /// Log seconds to stop at (None = the end).
    pub to: Option<f64>,
    pub looping: bool,
    /// Logged source name → scene source name.
    pub rename: BTreeMap<String, String>,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self { speed: 1.0, from: 0.0, to: None, looping: false, rename: BTreeMap::new() }
    }
}

impl ReplayConfig {
    /// Read the options from a `replay.log` spec (`path` is left to the caller).
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Some(v) = spec.params.get("speed") {
            cfg.speed = v.as_f64().filter(|s| s.is_finite() && *s >= 0.0).ok_or("replay.log: `speed` must be a number ≥ 0")?;
        }
        if let Some(v) = spec.params.get("from") {
            cfg.from = v.as_f64().filter(|s| s.is_finite() && *s >= 0.0).ok_or("replay.log: `from` must be seconds ≥ 0")?;
        }
        if let Some(v) = spec.params.get("to") {
            cfg.to = Some(v.as_f64().filter(|s| s.is_finite() && *s > cfg.from).ok_or("replay.log: `to` must be seconds > `from`")?);
        }
        if let Some(v) = spec.params.get("loop") {
            cfg.looping = v.as_bool().ok_or("replay.log: `loop` must be true or false")?;
        }
        if let Some(v) = spec.params.get("rename") {
            let map = v.as_object().ok_or("replay.log: `rename` must be a table of logged name → scene name")?;
            for (k, v) in map {
                let to = v.as_str().ok_or("replay.log: `rename` values must be strings")?;
                cfg.rename.insert(k.clone(), to.to_string());
            }
        }
        Ok(cfg)
    }
}

/// A log being played.
#[derive(Debug, Clone)]
pub struct Replay {
    log: Arc<DataLog>,
    title: String,
    cfg: ReplayConfig,
    speed: f64,
    /// Log position (seconds into the log).
    pos: f64,
    /// Engine time at which `pos` was `pos0` (None until the next tick anchors it).
    anchor: Option<(Seconds, f64)>,
    /// Next entry to emit.
    cursor: usize,
    ended: bool,
}

impl Replay {
    pub fn new(log: Arc<DataLog>, title: impl Into<String>, cfg: ReplayConfig) -> Self {
        let from = cfg.from.min(log.length_seconds());
        let cursor = log.index_at(from);
        Self { speed: cfg.speed, pos: from, anchor: None, cursor, ended: false, log, title: title.into(), cfg }
    }

    pub fn log(&self) -> &Arc<DataLog> {
        &self.log
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn config(&self) -> &ReplayConfig {
        &self.cfg
    }
    /// Seconds into the log.
    pub fn position(&self) -> f64 {
        self.pos
    }
    pub fn length(&self) -> f64 {
        self.log.length_seconds()
    }
    /// Where playback stops (`to`, clamped to the log).
    pub fn end(&self) -> f64 {
        self.cfg.to.unwrap_or(f64::INFINITY).min(self.length())
    }
    pub fn speed(&self) -> f64 {
        self.speed
    }
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Change speed (0 pauses). Takes effect from the next tick.
    pub fn set_speed(&mut self, speed: f64) {
        if speed.is_finite() && speed >= 0.0 {
            self.speed = speed;
            self.anchor = None;
        }
    }

    /// Jump to `to` seconds into the log. Emission resumes from there on the next tick.
    pub fn seek(&mut self, to: f64) {
        let to = if to.is_finite() { to.clamp(0.0, self.length()) } else { 0.0 };
        self.pos = to;
        self.cursor = self.log.index_at(to);
        self.anchor = None;
        self.ended = false;
    }

    /// The scene source name a logged sample is emitted under.
    pub fn rename(&self, logged: &SourceId) -> SourceId {
        match self.cfg.rename.get(logged.as_str()) {
            Some(to) => SourceId::new(to),
            None => logged.clone(),
        }
    }

    /// Entries in `[to - WARM_SECONDS, to)` — what a seek feeds to the features before
    /// resuming, so adaptive signals have history.
    pub fn warm_entries(&self, to: f64) -> &[LogEntry] {
        let start = self.log.index_at((to - WARM_SECONDS).max(0.0));
        let end = self.log.index_at(to);
        &self.log.entries()[start.min(end)..end]
    }

    /// Step to engine time `now`; returns the samples due, stamped in engine time.
    pub fn advance_to(&mut self, now: Seconds) -> Vec<Sample> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        let (anchor_t, pos0) = match self.anchor {
            Some(a) => a,
            None => {
                self.anchor = Some((now, self.pos));
                (now, self.pos)
            }
        };
        let end = self.end();
        let mut new_pos = (pos0 + (now - anchor_t).max(0.0) * self.speed).min(end);
        self.emit_until(new_pos, now, &mut out);
        if new_pos >= end {
            if self.cfg.looping && end > self.cfg.from {
                // Wrap: continue from `from` with the leftover time.
                let over = if self.speed > 0.0 { (pos0 + (now - anchor_t) * self.speed - end).max(0.0) } else { 0.0 };
                let from = self.cfg.from.min(end);
                self.cursor = self.log.index_at(from);
                new_pos = (from + over).min(end);
                self.emit_until(new_pos, now, &mut out);
                self.anchor = Some((now, new_pos));
            } else {
                self.ended = true;
            }
        }
        self.pos = new_pos;
        out
    }

    /// Emit entries with `t <= pos` (from the cursor), stamped by their engine-time
    /// equivalent (never later than `now`, never more than a second early).
    fn emit_until(&mut self, pos: f64, now: Seconds, out: &mut Vec<Sample>) {
        let entries = self.log.entries();
        let end = self.end();
        while let Some(e) = entries.get(self.cursor) {
            if e.t > pos || e.t > end {
                break;
            }
            let at = if self.speed > 0.0 { (now - (pos - e.t) / self.speed).max(now - 1.0) } else { now };
            out.push(Sample { source: self.rename(&e.source), at, datum: e.datum.clone() });
            self.cursor += 1;
        }
    }

    pub fn snapshot(&self) -> ReplaySnapshot {
        let h = self.log.header();
        ReplaySnapshot {
            title: self.title.clone(),
            position: self.pos,
            length: self.length(),
            speed: self.speed,
            epoch_unix: h.epoch_unix,
            scene: h.scene.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datalog::LogHeader;
    use sonder_core::Datum;

    fn log() -> Arc<DataLog> {
        let header = LogHeader::new("T", 1, 0.0, &[]);
        let mut s = serde_json::to_string(&header).unwrap();
        s.push('\n');
        let a = SourceId::new("a");
        for i in 0..100 {
            let t = i as f64 * 0.1;
            s.push_str(&crate::datalog::sample_line(&Sample::event(&a, t, "tick", 0.5).with_attr("n", i as f64)).unwrap());
            s.push('\n');
        }
        Arc::new(DataLog::from_text(&s).unwrap())
    }

    fn n_of(s: &Sample) -> f64 {
        match &s.datum {
            Datum::Event { attrs, .. } => attrs["n"].as_f64().unwrap(),
            _ => panic!(),
        }
    }

    #[test]
    fn plays_in_order_at_original_timing() {
        let mut r = Replay::new(log(), "t", ReplayConfig::default());
        // Engine starts at 100 s; first tick anchors and emits what's due at pos 0.
        let s = r.advance_to(100.0);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![0.0]);
        assert_eq!(s[0].at, 100.0);
        let s = r.advance_to(100.02);
        assert!(s.is_empty());
        let s = r.advance_to(100.25);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![1.0, 2.0]);
        assert!((s[0].at - 100.1).abs() < 1e-9 && (s[1].at - 100.2).abs() < 1e-9);
        assert!((r.position() - 0.25).abs() < 1e-9);
        assert_eq!(r.length(), 9.9);
        assert!(!r.ended());
        // Runs out and stops.
        let s = r.advance_to(200.0);
        assert_eq!(s.len(), 97);
        assert!(r.ended());
        assert_eq!(r.position(), 9.9);
        assert!(r.advance_to(300.0).is_empty());
    }

    #[test]
    fn speed_from_to_loop_and_rename() {
        let spec = SourceSpec::new(KIND)
            .with("speed", 2.0)
            .with("from", 1.0)
            .with("to", 2.0)
            .with("loop", true)
            .with("rename", serde_json::json!({"a": "traffic"}));
        let cfg = ReplayConfig::from_spec(&spec).unwrap();
        assert_eq!(cfg, ReplayConfig { speed: 2.0, from: 1.0, to: Some(2.0), looping: true, rename: [("a".to_string(), "traffic".to_string())].into() });
        let mut r = Replay::new(log(), "t", cfg);
        let s = r.advance_to(0.0);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![10.0]);
        assert_eq!(s[0].source.as_str(), "traffic");
        // 0.25 s of engine time at 2× = 0.5 s of log: entries 11..=15.
        let s = r.advance_to(0.25);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![11.0, 12.0, 13.0, 14.0, 15.0]);
        // Past `to` (2.0) it wraps to `from` and keeps going.
        let s = r.advance_to(0.6);
        let ns: Vec<f64> = s.iter().map(n_of).collect();
        assert!(ns.starts_with(&[16.0, 17.0, 18.0, 19.0, 20.0, 10.0]), "{ns:?}");
        assert!(!r.ended());
        assert!(r.position() >= 1.0 && r.position() <= 2.0, "{}", r.position());
        // Bad params are rejected.
        assert!(ReplayConfig::from_spec(&SourceSpec::new(KIND).with("speed", -1.0)).is_err());
        assert!(ReplayConfig::from_spec(&SourceSpec::new(KIND).with("from", 5.0).with("to", 2.0)).is_err());
    }

    #[test]
    fn seek_set_speed_and_warm_window() {
        let mut r = Replay::new(log(), "t", ReplayConfig::default());
        r.advance_to(0.0);
        r.seek(5.0);
        assert_eq!(r.position(), 5.0);
        let s = r.advance_to(10.0);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![50.0]);
        r.set_speed(0.0);
        assert!(r.advance_to(20.0).is_empty());
        assert_eq!(r.position(), 5.0);
        r.set_speed(1.0);
        let s = r.advance_to(21.0);
        assert!(s.is_empty(), "re-anchored: nothing due yet");
        let s = r.advance_to(21.15);
        assert_eq!(s.iter().map(n_of).collect::<Vec<_>>(), vec![51.0]);
        let w = r.warm_entries(5.0);
        assert_eq!(w.len(), 50, "everything before 5 s lies within the 30 s warm window");
        assert!(w.iter().all(|e| e.t < 5.0));
        r.seek(1e9);
        assert_eq!(r.position(), 9.9);
        let snap = r.snapshot();
        assert_eq!((snap.position, snap.length, snap.speed), (9.9, 9.9, 1.0));
        assert_eq!(snap.scene, "T");
    }
}
