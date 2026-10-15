//! Client-side UI state: everything that cannot be derived from a single
//! [`EngineSnapshot`] — selection, signal histories, car trails, and bounded
//! buffers of events/notes built up across snapshots with dedupe cursors.

use std::collections::{BTreeMap, VecDeque};

use sonder_core::{EngineSnapshot, EventSnapshot, NoteSnapshot};

/// Points kept per layer signal history (one per snapshot tick).
pub const HISTORY_LEN: usize = 60;
/// Positions kept per freeway car for the trail effect.
pub const TRAIL_LEN: usize = 8;
/// Seconds of notes shown in the piano roll.
pub const ROLL_SECONDS: f64 = 16.0;
/// Seconds a map event stays visible while fading.
pub const MAP_FADE_SECONDS: f64 = 10.0;
/// Upper bound on buffered events.
pub const EVENT_BUFFER: usize = 128;
/// Upper bound on buffered notes.
pub const NOTE_BUFFER: usize = 1024;

/// Fixed-capacity ring buffer of samples, oldest first when iterated.
#[derive(Debug, Clone)]
pub struct History {
    buf: VecDeque<f64>,
    cap: usize,
}

impl History {
    /// A history that keeps the last `cap` values.
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.max(1)),
            cap: cap.max(1),
        }
    }

    /// Push a value, evicting the oldest if full. Non-finite values become 0.
    pub fn push(&mut self, v: f64) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(if v.is_finite() { v } else { 0.0 });
    }

    /// Number of stored values.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Oldest-first iterator over the stored values.
    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        self.buf.iter().copied()
    }

    /// The most recent `n` values, oldest first.
    pub fn tail(&self, n: usize) -> Vec<f64> {
        let skip = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(skip).copied().collect()
    }

    /// The last `n` values as sparkline units (`0..=100`), oldest first.
    pub fn tail_u64(&self, n: usize) -> Vec<u64> {
        self.tail(n)
            .into_iter()
            .map(|v| (v.clamp(0.0, 1.0) * 100.0).round() as u64)
            .collect()
    }
}

/// A geo event remembered for the `map` visualization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapEvent {
    pub at: f64,
    pub lat01: f64,
    pub lon01: f64,
    pub intensity: f64,
    pub layer: u16,
}

/// Everything the TUI remembers between frames.
#[derive(Debug)]
pub struct UiState {
    /// Selected row in the layers table.
    pub selected: usize,
    /// Whether the help popup is open.
    pub show_help: bool,
    /// Per-layer history of the layer's first signal (by layer index).
    pub signal_hist: Vec<History>,
    /// Name of the signal tracked per layer (first key in `signals`).
    pub signal_name: Vec<String>,
    /// Master RMS history for the generic viz / spectrum fallback.
    pub master_hist: History,
    /// Trail positions per freeway car index, newest last.
    pub trails: Vec<VecDeque<(f64, f64)>>,
    /// Events accumulated across snapshots, oldest first.
    pub events: VecDeque<EventSnapshot>,
    /// Notes accumulated across snapshots, oldest first.
    pub notes: VecDeque<NoteSnapshot>,
    /// Geo events for the map viz.
    pub map_events: VecDeque<MapEvent>,
    /// Latest `at` ingested for events (dedupe cursor).
    pub event_cursor: f64,
    /// Latest `at` ingested for notes (dedupe cursor).
    pub note_cursor: f64,
    /// Engine time of the latest snapshot.
    pub now: f64,
    /// Snapshot ticks seen (drives the generic scroll).
    pub ticks: u64,
    /// Transient status line (e.g. "opened web UI"), shown in the footer.
    pub status: Option<(String, f64)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            selected: 0,
            show_help: false,
            signal_hist: Vec::new(),
            signal_name: Vec::new(),
            master_hist: History::new(HISTORY_LEN),
            trails: Vec::new(),
            events: VecDeque::new(),
            notes: VecDeque::new(),
            map_events: VecDeque::new(),
            event_cursor: f64::NEG_INFINITY,
            note_cursor: f64::NEG_INFINITY,
            now: 0.0,
            ticks: 0,
            status: None,
        }
    }
}

impl UiState {
    /// Fresh state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a new snapshot into the client-side buffers.
    pub fn ingest(&mut self, snap: &EngineSnapshot) {
        self.ticks = self.ticks.wrapping_add(1);
        // Engine restarts (time goes backwards) reset the cursors.
        if snap.time + 1e-9 < self.now {
            self.event_cursor = f64::NEG_INFINITY;
            self.note_cursor = f64::NEG_INFINITY;
            self.events.clear();
            self.notes.clear();
            self.map_events.clear();
        }
        self.now = snap.time;

        // Signal histories.
        self.signal_hist
            .resize_with(snap.layers.len(), || History::new(HISTORY_LEN));
        self.signal_name.resize_with(snap.layers.len(), String::new);
        for (i, layer) in snap.layers.iter().enumerate() {
            let (name, value) = layer
                .signals
                .iter()
                .next()
                .map(|(k, v)| (k.as_str(), *v))
                .unwrap_or(("rms", layer.meter.rms as f64));
            if self.signal_name[i] != name {
                self.signal_name[i] = name.to_string();
            }
            self.signal_hist[i].push(value);
        }
        self.master_hist.push(snap.master.rms as f64);
        if self.selected >= snap.layers.len() {
            self.selected = snap.layers.len().saturating_sub(1);
        }

        // Events: dedupe by `at` (snapshot is newest-last).
        let mut max_at = self.event_cursor;
        for ev in snap.events.iter().filter(|e| e.at > self.event_cursor) {
            max_at = max_at.max(ev.at);
            if let Some(me) = map_event(ev) {
                self.map_events.push_back(me);
            }
            self.events.push_back(ev.clone());
        }
        self.event_cursor = max_at;
        while self.events.len() > EVENT_BUFFER {
            self.events.pop_front();
        }
        while self
            .map_events
            .front()
            .is_some_and(|m| self.now - m.at > MAP_FADE_SECONDS)
        {
            self.map_events.pop_front();
        }
        while self.map_events.len() > EVENT_BUFFER {
            self.map_events.pop_front();
        }

        // Notes: new ones appended; buffered active ones refreshed from the snapshot.
        let mut max_at = self.note_cursor;
        for n in snap.notes.iter().filter(|n| n.at > self.note_cursor) {
            max_at = max_at.max(n.at);
            self.notes.push_back(n.clone());
        }
        self.note_cursor = max_at;
        for buffered in self.notes.iter_mut().filter(|n| n.active) {
            let fresh = snap.notes.iter().find(|n| {
                n.layer == buffered.layer && n.at == buffered.at && n.midi == buffered.midi
            });
            match fresh {
                Some(f) => buffered.active = f.active,
                None => buffered.active = false,
            }
        }
        let horizon = self.now - ROLL_SECONDS - 1.0;
        while self
            .notes
            .front()
            .is_some_and(|n| !n.active && n.at < horizon)
        {
            self.notes.pop_front();
        }
        while self.notes.len() > NOTE_BUFFER {
            self.notes.pop_front();
        }

        // Car trails for the freeway viz.
        let cars = freeway_cars(snap);
        if self.trails.len() < cars.len() {
            self.trails
                .resize_with(cars.len(), || VecDeque::with_capacity(TRAIL_LEN));
        }
        for (i, trail) in self.trails.iter_mut().enumerate() {
            match cars.get(i) {
                Some(&(x, y)) => {
                    // A jump of more than half the frame means the slot was reused.
                    if trail
                        .back()
                        .is_some_and(|&(px, py)| (px - x).abs() > 0.5 || (py - y).abs() > 0.5)
                    {
                        trail.clear();
                    }
                    if trail.len() == TRAIL_LEN {
                        trail.pop_front();
                    }
                    trail.push_back((x, y));
                }
                None => trail.clear(),
            }
        }

        if self
            .status
            .as_ref()
            .is_some_and(|(_, until)| self.now > *until)
        {
            self.status = None;
        }
    }

    /// Move the selection by `delta` rows, clamped to `len`.
    pub fn move_selection(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as i64;
        self.selected = (cur + delta as i64).clamp(0, len as i64 - 1) as usize;
    }

    /// Show a transient footer message for a few seconds of engine time.
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), self.now + 3.0));
    }

    /// The last `n` buffered events, newest first.
    pub fn recent_events(&self, n: usize) -> impl Iterator<Item = &EventSnapshot> {
        self.events.iter().rev().take(n)
    }
}

/// Extract `(x, y)` car positions from any viz payload carrying a `cars` array.
pub fn freeway_cars(snap: &EngineSnapshot) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for payload in snap.viz.values() {
        if let Some(cars) = payload.get("cars").and_then(|c| c.as_array()) {
            for car in cars {
                let x = car.get("x").and_then(|v| v.as_f64()).unwrap_or(0.5);
                let y = car.get("y").and_then(|v| v.as_f64()).unwrap_or(0.5);
                out.push((x, y));
            }
        }
    }
    out
}

/// Number of lanes declared by a freeway payload, or inferred from car lanes.
pub fn freeway_lanes(viz: &BTreeMap<String, serde_json::Value>) -> usize {
    let mut lanes = 0usize;
    for payload in viz.values() {
        if let Some(n) = payload.get("lanes").and_then(|v| v.as_u64()) {
            lanes = lanes.max(n as usize);
        }
        if let Some(cars) = payload.get("cars").and_then(|c| c.as_array()) {
            for car in cars {
                if let Some(l) = car.get("lane").and_then(|v| v.as_u64()) {
                    lanes = lanes.max(l as usize + 1);
                }
            }
        }
    }
    lanes
}

fn map_event(ev: &EventSnapshot) -> Option<MapEvent> {
    let lat01 = ev.attrs.get("lat01")?.as_f64()?;
    let lon01 = ev.attrs.get("lon01")?.as_f64()?;
    Some(MapEvent {
        at: ev.at,
        lat01,
        lon01,
        intensity: ev.intensity,
        layer: ev.layer.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{LayerId, LayerSnapshot, Value};

    #[test]
    fn history_ring_evicts_oldest() {
        let mut h = History::new(3);
        assert!(h.is_empty());
        for v in [0.1, 0.2, 0.3, 0.4] {
            h.push(v);
        }
        assert_eq!(h.len(), 3);
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![0.2, 0.3, 0.4]);
        assert_eq!(h.tail(2), vec![0.3, 0.4]);
        assert_eq!(h.tail(10), vec![0.2, 0.3, 0.4]);
        assert_eq!(h.tail_u64(3), vec![20, 30, 40]);
        h.push(f64::NAN);
        h.push(5.0);
        assert_eq!(h.tail_u64(2), vec![0, 100]);
    }

    fn ev(at: f64, name: &str) -> EventSnapshot {
        EventSnapshot {
            at,
            layer: LayerId(0),
            name: name.into(),
            intensity: 0.5,
            attrs: BTreeMap::new(),
                    label: String::new(),
        }
    }

    fn note(at: f64, midi: f32, active: bool) -> NoteSnapshot {
        NoteSnapshot {
            at,
            layer: LayerId(1),
            midi,
            velocity: 0.8,
            pan: 0.5,
            duration: Some(0.5),
            active,
         cause: None, reason: String::new(), }
    }

    #[test]
    fn events_and_notes_are_deduped_by_at() {
        let mut st = UiState::new();
        let mut snap = EngineSnapshot {
            time: 1.0,
            ..Default::default()
        };
        snap.events = vec![ev(0.2, "a"), ev(0.5, "b")];
        snap.notes = vec![note(0.3, 60.0, true)];
        st.ingest(&snap);
        assert_eq!(st.events.len(), 2);
        assert_eq!(st.notes.len(), 1);

        // Same snapshot again: nothing new.
        st.ingest(&snap);
        assert_eq!(st.events.len(), 2);
        assert_eq!(st.notes.len(), 1);

        // Overlapping window with one new event and the note now released.
        snap.time = 1.5;
        snap.events = vec![ev(0.5, "b"), ev(0.9, "c")];
        snap.notes = vec![note(0.3, 60.0, false), note(1.2, 64.0, true)];
        st.ingest(&snap);
        assert_eq!(
            st.events
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(st.notes.len(), 2);
        assert!(!st.notes[0].active, "buffered note should pick up release");
        assert!(st.notes[1].active);

        // A note that falls out of the snapshot entirely is treated as released.
        snap.time = 2.0;
        snap.notes = vec![];
        st.ingest(&snap);
        assert!(!st.notes[1].active);
    }

    #[test]
    fn old_notes_are_dropped_and_restart_resets() {
        let mut st = UiState::new();
        let mut snap = EngineSnapshot {
            time: 1.0,
            ..Default::default()
        };
        snap.notes = vec![note(0.5, 60.0, false)];
        st.ingest(&snap);
        snap.time = 1.0 + ROLL_SECONDS + 5.0;
        snap.notes = vec![];
        st.ingest(&snap);
        assert!(st.notes.is_empty());

        // Time went backwards → cursors reset so the same `at` ingests again.
        snap.time = 0.5;
        snap.notes = vec![note(0.1, 60.0, true)];
        st.ingest(&snap);
        assert_eq!(st.notes.len(), 1);
    }

    #[test]
    fn map_events_fade_out() {
        let mut st = UiState::new();
        let mut snap = EngineSnapshot {
            time: 1.0,
            ..Default::default()
        };
        let mut e = ev(0.5, "quake");
        e.attrs.insert("lat01".into(), Value::Num(0.4));
        e.attrs.insert("lon01".into(), Value::Num(0.6));
        snap.events = vec![e];
        st.ingest(&snap);
        assert_eq!(st.map_events.len(), 1);
        snap.time = 0.5 + MAP_FADE_SECONDS + 0.1;
        st.ingest(&snap);
        assert!(st.map_events.is_empty());
    }

    #[test]
    fn signal_histories_track_layers_and_selection_clamps() {
        let mut st = UiState::new();
        let mut snap = EngineSnapshot::default();
        let mut l = LayerSnapshot {
            name: "a".into(),
            ..Default::default()
        };
        l.signals.insert("density".into(), 0.7);
        snap.layers = vec![l.clone(), LayerSnapshot::default()];
        st.selected = 5;
        st.ingest(&snap);
        assert_eq!(st.signal_hist.len(), 2);
        assert_eq!(st.signal_name[0], "density");
        assert_eq!(st.signal_name[1], "rms");
        assert_eq!(st.signal_hist[0].tail(1), vec![0.7]);
        assert_eq!(st.selected, 1);
        st.move_selection(-5, 2);
        assert_eq!(st.selected, 0);
        st.move_selection(1, 2);
        assert_eq!(st.selected, 1);
        st.move_selection(1, 0);
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn trails_follow_cars() {
        let mut st = UiState::new();
        let mut snap = EngineSnapshot::default();
        for i in 0..(TRAIL_LEN + 3) {
            let y = i as f64 * 0.05;
            snap.viz.insert(
                "traffic".into(),
                serde_json::json!({ "lanes": 3, "cars": [{ "x": 0.2, "y": y, "lane": 0 }] }),
            );
            snap.time = i as f64;
            st.ingest(&snap);
        }
        assert_eq!(st.trails.len(), 1);
        assert_eq!(st.trails[0].len(), TRAIL_LEN);
        assert_eq!(freeway_lanes(&snap.viz), 3);
        // Slot reused for a car at the far end: trail resets.
        snap.viz.insert(
            "traffic".into(),
            serde_json::json!({ "cars": [{ "x": 0.9, "y": 0.0, "lane": 2 }] }),
        );
        snap.time += 1.0;
        st.ingest(&snap);
        assert_eq!(st.trails[0].len(), 1);
        assert_eq!(freeway_lanes(&snap.viz), 3);
        // Car gone: trail cleared.
        snap.viz
            .insert("traffic".into(), serde_json::json!({ "cars": [] }));
        st.ingest(&snap);
        assert!(st.trails[0].is_empty());
    }
}
