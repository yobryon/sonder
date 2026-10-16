//! `human.midi` — a person at a MIDI keyboard (hardware, via `midir`).
//!
//! Emits `note { key, velocity, channel }`, `note_off { key, channel }`,
//! `pedal { on }` (CC 64) and `cc { number, value }` events, plus readings at 10 Hz:
//! `activity` (notes/s over the last 4 s, normalised so 8/s = 1), `register` (mean
//! key of the held notes / 127; holds its last value when nothing is held) and
//! `pressure` (channel / poly aftertouch, 0..1).
//!
//! This is the hardware path. On-screen pianos, computer keyboards and Web MIDI in
//! the browser send [`sonder_core::UserCommand::HumanNote`] instead, which the
//! composer feeds to every `Role::Human` layer without any source at all.
//!
//! Parameters: `port` (case-insensitive substring of the input port name; default:
//! the first input port; `"virtual"` creates a virtual input port named "Sonder In" on
//! Linux/macOS), `channel` (1..16, optional: ignore messages on other channels).

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sonder_core::{Seconds, SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Window for the `activity` reading, in seconds.
pub const ACTIVITY_WINDOW_S: f64 = 4.0;
/// Notes per second that read as full activity.
pub const ACTIVITY_FULL_RATE: f64 = 8.0;
/// Readings per second.
const READING_HZ: f64 = 10.0;
/// Sustain pedal controller number.
pub const CC_SUSTAIN: u8 = 64;

/// A decoded channel-voice message. Channels are 0-based here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMsg {
    NoteOn { channel: u8, key: u8, velocity: u8 },
    NoteOff { channel: u8, key: u8 },
    Cc { channel: u8, number: u8, value: u8 },
    ChannelPressure { channel: u8, value: u8 },
    PolyPressure { channel: u8, key: u8, value: u8 },
}

impl MidiMsg {
    pub fn channel(&self) -> u8 {
        match self {
            MidiMsg::NoteOn { channel, .. }
            | MidiMsg::NoteOff { channel, .. }
            | MidiMsg::Cc { channel, .. }
            | MidiMsg::ChannelPressure { channel, .. }
            | MidiMsg::PolyPressure { channel, .. } => *channel,
        }
    }
}

/// Decode one MIDI message (no running status; `midir` delivers complete messages).
/// NoteOn with velocity 0 is a NoteOff. System messages and anything short return None.
pub fn parse_message(bytes: &[u8]) -> Option<MidiMsg> {
    let &status = bytes.first()?;
    if !(0x80..0xF0).contains(&status) {
        return None;
    }
    let channel = status & 0x0F;
    let d1 = *bytes.get(1)? & 0x7F;
    let d2 = bytes.get(2).map(|b| b & 0x7F);
    Some(match status & 0xF0 {
        0x80 => MidiMsg::NoteOff { channel, key: d1 },
        0x90 => match d2? {
            0 => MidiMsg::NoteOff { channel, key: d1 },
            velocity => MidiMsg::NoteOn { channel, key: d1, velocity },
        },
        0xA0 => MidiMsg::PolyPressure { channel, key: d1, value: d2? },
        0xB0 => MidiMsg::Cc { channel, number: d1, value: d2? },
        0xD0 => MidiMsg::ChannelPressure { channel, value: d1 },
        _ => return None,
    })
}

/// What a message turns into: `(event name, intensity, attrs)`.
pub type Emitted = (&'static str, f64, Attrs);

/// Pure state behind the source: held keys, onsets, pressure → events and readings.
#[derive(Debug, Default)]
pub struct Tracker {
    /// 0-based channel filter.
    pub channel: Option<u8>,
    held: BTreeSet<u8>,
    onsets: VecDeque<Seconds>,
    pressure: f64,
    register: f64,
    pedal: bool,
}

impl Tracker {
    pub fn new(channel: Option<u8>) -> Self {
        Self { channel, register: 0.5, ..Default::default() }
    }

    /// Apply a decoded message; returns the events to emit (possibly none).
    pub fn apply(&mut self, msg: MidiMsg, now: Seconds) -> Vec<Emitted> {
        if self.channel.is_some_and(|c| c != msg.channel()) {
            return Vec::new();
        }
        let ch = msg.channel() as f64 + 1.0;
        let mut out = Vec::new();
        match msg {
            MidiMsg::NoteOn { key, velocity, .. } => {
                self.held.insert(key);
                self.onsets.push_back(now);
                if self.onsets.len() > 512 {
                    self.onsets.pop_front();
                }
                self.update_register();
                let v = velocity as f64 / 127.0;
                out.push((
                    "note",
                    v,
                    Attrs::new().num("key", key as f64).num("velocity", v).num("channel", ch),
                ));
            }
            MidiMsg::NoteOff { key, .. } => {
                self.held.remove(&key);
                self.update_register();
                out.push(("note_off", 0.0, Attrs::new().num("key", key as f64).num("channel", ch)));
            }
            MidiMsg::Cc { number, value, .. } => {
                let v = value as f64 / 127.0;
                out.push(("cc", v, Attrs::new().num("number", number as f64).num("value", v).num("channel", ch)));
                if number == CC_SUSTAIN {
                    let on = value >= 64;
                    if on != self.pedal {
                        self.pedal = on;
                        out.push(("pedal", if on { 1.0 } else { 0.0 }, Attrs::new().bool("on", on).num("channel", ch)));
                    }
                }
            }
            MidiMsg::ChannelPressure { value, .. } | MidiMsg::PolyPressure { value, .. } => {
                self.pressure = value as f64 / 127.0;
            }
        }
        out
    }

    fn update_register(&mut self) {
        if !self.held.is_empty() {
            let mean = self.held.iter().map(|&k| k as f64).sum::<f64>() / self.held.len() as f64;
            self.register = clamp01(mean / 127.0);
        }
    }

    /// Keys currently down.
    pub fn held(&self) -> Vec<u8> {
        self.held.iter().copied().collect()
    }

    /// Notes per second over the window, normalised (8/s = 1).
    pub fn activity(&mut self, now: Seconds) -> f64 {
        while self.onsets.front().is_some_and(|&t| now - t > ACTIVITY_WINDOW_S) {
            self.onsets.pop_front();
        }
        clamp01(self.onsets.len() as f64 / ACTIVITY_WINDOW_S / ACTIVITY_FULL_RATE)
    }

    /// The periodic reading.
    pub fn readings(&mut self, now: Seconds) -> Vec<(&'static str, f64)> {
        vec![("activity", self.activity(now)), ("register", self.register), ("pressure", self.pressure)]
    }
}

struct MidiSource {
    port: String,
    channel: Option<u8>,
}

/// Open the port on a dedicated thread (midir's input handle is not `Send` on every
/// backend) and forward raw messages until `stop` is set. The first message on the
/// returned receiver is the port name, or an error.
fn spawn_reader(
    port: String,
    stop: Arc<AtomicBool>,
    bytes_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) -> std::sync::mpsc::Receiver<Result<String, String>> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("sonder-midi-in".into())
        .spawn(move || {
            let result = open(&port, bytes_tx);
            match result {
                Ok((name, _conn)) => {
                    let _ = ready_tx.send(Ok(name));
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    // `_conn` drops here, closing the port.
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })
        .ok();
    ready_rx
}

type Conn = midir::MidiInputConnection<()>;

fn open(port: &str, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> Result<(String, Conn), String> {
    let mut input = midir::MidiInput::new("sonder").map_err(|e| format!("midi init: {e}"))?;
    input.ignore(midir::Ignore::All);
    let callback = move |_ts: u64, bytes: &[u8], _: &mut ()| {
        let _ = tx.send(bytes.to_vec());
    };
    if port.eq_ignore_ascii_case("virtual") {
        #[cfg(unix)]
        {
            use midir::os::unix::VirtualInput;
            let conn = input.create_virtual("Sonder In", callback, ()).map_err(|e| format!("virtual midi port: {e}"))?;
            return Ok(("Sonder In (virtual)".to_string(), conn));
        }
        #[cfg(not(unix))]
        return Err("virtual MIDI ports are not supported on this platform".to_string());
    }
    let ports = input.ports();
    let names: Vec<String> = ports.iter().filter_map(|p| input.port_name(p).ok()).collect();
    let needle = port.to_ascii_lowercase();
    let found = if port.is_empty() {
        ports.first()
    } else {
        ports.iter().find(|p| input.port_name(p).map(|n| n.to_ascii_lowercase().contains(&needle)).unwrap_or(false))
    };
    let Some(p) = found else {
        return Err(if names.is_empty() {
            "no MIDI input ports (plug in a keyboard, or use port = \"virtual\")".to_string()
        } else {
            format!("no MIDI input port matching `{port}` (available: {})", names.join(", "))
        });
    };
    let name = input.port_name(p).unwrap_or_default();
    let conn = input.connect(p, "sonder-in", callback, ()).map_err(|e| format!("connecting midi: {e}"))?;
    Ok((name, conn))
}

impl Source for MidiSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = util::Emitter::new(ctx);
            em.status(SourceState::Connecting, "opening MIDI input").await;
            let stop = Arc::new(AtomicBool::new(false));
            let (bytes_tx, mut bytes_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            let ready = spawn_reader(self.port.clone(), stop.clone(), bytes_tx);
            let opened = tokio::task::spawn_blocking(move || ready.recv_timeout(Duration::from_secs(10)))
                .await
                .ok()
                .and_then(Result::ok);
            match opened {
                Some(Ok(name)) => em.status(SourceState::Live, format!("listening on {name}")).await,
                Some(Err(e)) => {
                    em.status(SourceState::Failed, e).await;
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                None => {
                    em.status(SourceState::Failed, "MIDI input did not open").await;
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            }
            let mut tracker = Tracker::new(self.channel);
            let mut ticker = tokio::time::interval(Duration::from_secs_f64(1.0 / READING_HZ));
            loop {
                tokio::select! {
                    msg = bytes_rx.recv() => {
                        let Some(bytes) = msg else { break };
                        if let Some(m) = parse_message(&bytes) {
                            let now = em.now();
                            for (name, intensity, attrs) in tracker.apply(m, now) {
                                em.event(name, intensity, attrs.into_map()).await;
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        let now = em.now();
                        em.reading(tracker.readings(now)).await;
                    }
                    _ = em.ctx.wait_shutdown() => break,
                }
                if em.is_shutdown() {
                    break;
                }
            }
            stop.store(true, Ordering::Relaxed);
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "human.midi",
        summary: "A person at a MIDI keyboard: note/note_off/pedal/cc events, activity and register readings",
        params: vec![
            ParamInfo { name: "port", summary: "Input port name (substring), or \"virtual\"", default: Some("first input port") },
            ParamInfo { name: "channel", summary: "Only listen to this MIDI channel (1..16)", default: None },
        ],
        channels: vec!["activity", "register", "pressure"],
        events: vec!["note", "note_off", "pedal", "cc"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let channel = match spec.params.get("channel") {
        None | Some(serde_json::Value::Null) => None,
        Some(_) => {
            let c = util::param_f64_in(spec, "channel", 1.0, 1.0, 16.0)?;
            Some(c as u8 - 1)
        }
    };
    Ok(Box::new(MidiSource { port: util::param_str(spec, "port", ""), channel }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::Value;

    #[test]
    fn parses_channel_voice_messages() {
        assert_eq!(parse_message(&[0x90, 60, 100]), Some(MidiMsg::NoteOn { channel: 0, key: 60, velocity: 100 }));
        assert_eq!(parse_message(&[0x91, 60, 0]), Some(MidiMsg::NoteOff { channel: 1, key: 60 }), "vel 0 = off");
        assert_eq!(parse_message(&[0x80, 60, 64]), Some(MidiMsg::NoteOff { channel: 0, key: 60 }));
        assert_eq!(parse_message(&[0xB3, 64, 127]), Some(MidiMsg::Cc { channel: 3, number: 64, value: 127 }));
        assert_eq!(parse_message(&[0xD0, 90]), Some(MidiMsg::ChannelPressure { channel: 0, value: 90 }));
        assert_eq!(parse_message(&[0xA0, 60, 40]), Some(MidiMsg::PolyPressure { channel: 0, key: 60, value: 40 }));
        assert_eq!(parse_message(&[0xF8]), None, "clock");
        assert_eq!(parse_message(&[0xE0, 0, 64]), None, "pitch bend ignored");
        assert_eq!(parse_message(&[0x90, 60]), None, "truncated");
        assert_eq!(parse_message(&[]), None);
        assert_eq!(parse_message(&[0x3C, 0x40]), None, "data byte first");
    }

    fn names(out: &[Emitted]) -> Vec<&'static str> {
        out.iter().map(|(n, _, _)| *n).collect()
    }

    #[test]
    fn bytes_become_events_and_readings() {
        let mut t = Tracker::new(None);
        let on = t.apply(parse_message(&[0x90, 60, 127]).unwrap(), 0.0);
        assert_eq!(names(&on), vec!["note"]);
        let (_, intensity, attrs) = &on[0];
        assert!((intensity - 1.0).abs() < 1e-9);
        let a = attrs.clone().into_map();
        assert_eq!(a.get("key"), Some(&Value::Num(60.0)));
        assert_eq!(a.get("channel"), Some(&Value::Num(1.0)));
        assert!((a.get("velocity").unwrap().as_f64().unwrap() - 1.0).abs() < 1e-9);
        t.apply(parse_message(&[0x90, 72, 64]).unwrap(), 0.1);
        let r = t.readings(0.2);
        let get = |k: &str| r.iter().find(|(n, _)| *n == k).map(|(_, v)| *v).unwrap();
        // 2 notes in 4 s → 0.5/s → 0.0625; register = mean(60, 72) / 127.
        assert!((get("activity") - 2.0 / 4.0 / 8.0).abs() < 1e-9);
        assert!((get("register") - 66.0 / 127.0).abs() < 1e-9);
        assert_eq!(get("pressure"), 0.0);
        assert_eq!(t.held(), vec![60, 72]);

        let off = t.apply(parse_message(&[0x80, 60, 0]).unwrap(), 0.3);
        assert_eq!(names(&off), vec!["note_off"]);
        assert_eq!(t.held(), vec![72]);
        t.apply(parse_message(&[0x90, 72, 0]).unwrap(), 0.4);
        assert!(t.held().is_empty());
        let r = t.readings(0.5);
        assert!((r[1].1 - 72.0 / 127.0).abs() < 1e-9, "register holds the last held key: {r:?}");

        let cc = t.apply(parse_message(&[0xB0, 64, 127]).unwrap(), 0.6);
        assert_eq!(names(&cc), vec!["cc", "pedal"]);
        assert_eq!(cc[1].2.clone().into_map().get("on"), Some(&Value::Bool(true)));
        let cc = t.apply(parse_message(&[0xB0, 64, 100]).unwrap(), 0.7);
        assert_eq!(names(&cc), vec!["cc"], "pedal still down: no repeat");
        let cc = t.apply(parse_message(&[0xB0, 64, 0]).unwrap(), 0.8);
        assert_eq!(names(&cc), vec!["cc", "pedal"]);
        assert_eq!(cc[1].1, 0.0);

        t.apply(parse_message(&[0xD0, 127]).unwrap(), 0.9);
        assert!((t.readings(1.0)[2].1 - 1.0).abs() < 1e-9);
        // The activity window slides.
        assert_eq!(t.readings(10.0)[0].1, 0.0);
    }

    #[test]
    fn channel_filter_drops_other_channels() {
        let mut t = Tracker::new(Some(2));
        assert!(t.apply(parse_message(&[0x90, 60, 100]).unwrap(), 0.0).is_empty());
        assert_eq!(names(&t.apply(parse_message(&[0x92, 60, 100]).unwrap(), 0.0)), vec!["note"]);
    }

    #[test]
    fn factory_validates_params() {
        assert!(factory(&SourceSpec::new("human.midi")).is_ok());
        assert!(factory(&SourceSpec::new("human.midi").with("port", "virtual").with("channel", 10)).is_ok());
        assert!(matches!(
            factory(&SourceSpec::new("human.midi").with("channel", 0)),
            Err(SourceError::InvalidParam { param: "channel", .. })
        ));
        assert!(matches!(
            factory(&SourceSpec::new("human.midi").with("channel", 17)),
            Err(SourceError::InvalidParam { param: "channel", .. })
        ));
        let i = info();
        assert_eq!(i.kind, "human.midi");
        assert!(!i.network);
    }

    /// Needs a MIDI subsystem (`/dev/snd/seq` on Linux); absent on CI and WSL, so
    /// `#[ignore]`. Run with `cargo test -p sonder-sources -- --ignored human_midi`.
    #[tokio::test]
    #[ignore]
    async fn live_virtual_port_opens_and_reports_live() {
        use sonder_core::{Datum, SourceId, WallClock};
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: SourceId::new("you"),
            tx,
            clock: Arc::new(WallClock::new()),
            http: crate::default_http_client(),
            shutdown: srx,
            seed: 0,
        };
        let src = factory(&SourceSpec::new("human.midi").with("port", "virtual")).unwrap();
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
        let mut states = Vec::new();
        while let Ok(s) = rx.try_recv() {
            if let Datum::Status { state, message } = s.datum {
                states.push((state, message));
            }
        }
        assert!(states.iter().any(|(s, _)| *s == SourceState::Live), "{states:?}");
    }
}
