//! Hosting a shared room: publish this engine's performance to a `sonder-relay`
//! (docs/ROOMS.md) so browsers can join with the same scene, seed and data.
//!
//! The control thread owns a [`RoomLink`]: ingested samples are teed into it through a
//! [`SampleSink`], `hello` is (re)announced on start and after hot reloads, chat said in
//! this UI goes out, and chat / presence from the relay come back in via [`RoomLink::pump`].
//! A tokio task ([`run`]) holds the WebSocket, batches samples every [`BATCH_MS`], sends a
//! light `snapshot` at [`SNAPSHOT_HZ`] and reconnects with backoff when the relay drops.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sonder_core::{ChatMessage, Datum, Sample, Scene};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::control::{ControlLoop, SampleSink};

/// How to reach the relay, from `--room`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomConfig {
    /// `wss://relay.example/r/<room>` (the relay adds `/host`).
    pub url: String,
    /// Host's display name (`--room-name`).
    pub name: String,
    /// Listed on the relay's index (`--room-public`).
    pub public: bool,
}

/// Sample batches go out this often.
pub const BATCH_MS: u64 = 100;
/// Light snapshots (time, captions, form) per second.
pub const SNAPSHOT_HZ: f64 = 2.0;
/// Samples queued for the publisher before the newest are dropped (≈ a few seconds of a
/// busy scene).
const QUEUE: usize = 8192;
/// Largest batch before an early flush.
const MAX_BATCH: usize = 2000;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// What the relay sent us.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Chat(ChatMessage),
    Presence(u32),
    /// Connected to the relay (true) or lost it (false).
    Connected(bool),
}

/// The control thread's end of a hosted room.
pub struct RoomLink {
    cfg: RoomConfig,
    sample_tx: mpsc::Sender<Sample>,
    hello_tx: watch::Sender<Option<Arc<str>>>,
    chat_tx: mpsc::Sender<ChatMessage>,
    inbound: Arc<Mutex<Vec<Inbound>>>,
    announced_reloads: usize,
    connected: bool,
}

impl RoomLink {
    /// Start the publisher task on `rt`; it stops when `shutdown` turns true.
    pub fn spawn(cfg: RoomConfig, snapshots: crate::SnapshotRx, shutdown: watch::Receiver<bool>, rt: &tokio::runtime::Handle) -> Self {
        let (sample_tx, sample_rx) = mpsc::channel(QUEUE);
        let (hello_tx, hello_rx) = watch::channel(None);
        let (chat_tx, chat_rx) = mpsc::channel(64);
        let inbound = Arc::new(Mutex::new(Vec::new()));
        let io = Io { url: host_url(&cfg.url), sample_rx, hello_rx, chat_rx, snapshots, shutdown, inbound: inbound.clone() };
        rt.spawn(run(io));
        Self { cfg, sample_tx, hello_tx, chat_tx, inbound, announced_reloads: 0, connected: false }
    }

    pub fn config(&self) -> &RoomConfig {
        &self.cfg
    }

    /// Room id: the last path segment of the URL.
    pub fn id(&self) -> String {
        room_id(&self.cfg.url)
    }

    /// A sink that tees every non-audio sample to the relay (dropping the newest when the
    /// publisher is behind, so the music never waits on the network).
    pub fn sink(&self) -> SampleSink {
        let tx = self.sample_tx.clone();
        Box::new(move |s: &Sample| {
            if !matches!(s.datum, Datum::Audio { .. }) {
                let _ = tx.try_send(s.clone());
            }
        })
    }

    /// (Re)announce the scene: sent now if connected, and again on every reconnect.
    pub fn announce(&self, scene: &Scene, seed: u64, engine_now: f64) {
        let hello = hello_frame(scene, seed, engine_now, &self.cfg);
        let _ = self.hello_tx.send(Some(Arc::from(hello.to_string())));
    }

    /// Once per control tick: relay messages into the control loop, this UI's chat out,
    /// and a fresh `hello` after a hot reload (`reloads` is the engine's reload counter).
    pub fn pump(&mut self, control: &mut ControlLoop, reloads: usize, engine_now: f64) {
        if reloads != self.announced_reloads {
            self.announced_reloads = reloads;
            self.announce(control.scene(), control.seed(), engine_now);
        }
        let inbound: Vec<Inbound> = std::mem::take(&mut *self.inbound.lock());
        for m in inbound {
            match m {
                Inbound::Chat(mut msg) => {
                    msg.at = engine_now;
                    control.chat_received(msg);
                }
                Inbound::Presence(n) => control.set_room_listeners(n),
                Inbound::Connected(on) => {
                    if on != self.connected {
                        self.connected = on;
                        control.push_caption(if on { format!("room {}: connected to the relay", self.id()) } else { "room: relay lost, reconnecting…".to_string() }, 0.5);
                    }
                }
            }
        }
        for msg in control.take_outbound_chat() {
            let _ = self.chat_tx.try_send(msg);
        }
    }
}

/// `…/r/<id>` → `…/r/<id>/host`.
pub fn host_url(url: &str) -> String {
    let base = url.trim_end_matches('/');
    if base.ends_with("/host") {
        base.to_string()
    } else {
        format!("{base}/host")
    }
}

/// The room id in a `--room` URL (its last path segment, ignoring a `/host` suffix).
pub fn room_id(url: &str) -> String {
    let path = url.split("://").nth(1).unwrap_or(url);
    let path = path.split(['?', '#']).next().unwrap_or(path).trim_end_matches('/');
    let path = path.strip_suffix("/host").unwrap_or(path);
    path.rsplit('/').next().unwrap_or("").to_string()
}

/// The `hello` frame: everything a guest needs to run the same performance.
pub fn hello_frame(scene: &Scene, seed: u64, engine_now: f64, cfg: &RoomConfig) -> Value {
    let scene_toml = toml::to_string_pretty(scene).unwrap_or_default();
    json!({
        "type": "hello",
        "scene_toml": scene_toml,
        "title": scene.name,
        "seed": seed,
        // u64 seeds lose precision in JSON.parse (doubles); browsers use this one.
        "seed_str": seed.to_string(),
        "epoch_unix": crate::datalog::unix_now() - engine_now,
        "engine_time": engine_now,
        "name": cfg.name,
        "public": cfg.public,
    })
}

/// One sample as a batch item: `{"t","s","d"}`.
pub fn sample_item(s: &Sample) -> Value {
    json!({ "t": s.at, "s": s.source.as_str(), "d": s.datum })
}

/// The light snapshot guests without an engine can show.
pub fn snapshot_frame(snap: &sonder_core::EngineSnapshot) -> Value {
    json!({ "type": "snapshot", "time": snap.time, "scene": snap.scene_name, "captions": snap.captions, "form": snap.form })
}

/// Parse a relay → host frame.
pub fn parse_inbound(text: &str) -> Option<Inbound> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "chat" => Some(Inbound::Chat(ChatMessage {
            at: v.get("at").and_then(Value::as_f64).unwrap_or(f64::NAN),
            from: v.get("from").and_then(Value::as_str).unwrap_or("anon").to_string(),
            text: v.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        })),
        "presence" => Some(Inbound::Presence(v.get("listeners").and_then(Value::as_u64).unwrap_or(0) as u32)),
        "error" => {
            let why = v.get("error").and_then(Value::as_str).unwrap_or("error").to_string();
            tracing::warn!("relay: {why}");
            None
        }
        _ => None,
    }
}

struct Io {
    url: String,
    sample_rx: mpsc::Receiver<Sample>,
    hello_rx: watch::Receiver<Option<Arc<str>>>,
    chat_rx: mpsc::Receiver<ChatMessage>,
    snapshots: crate::SnapshotRx,
    shutdown: watch::Receiver<bool>,
    inbound: Arc<Mutex<Vec<Inbound>>>,
}

/// The publisher task: connect, announce, stream, reconnect.
async fn run(mut io: Io) {
    let mut backoff = BACKOFF_MIN;
    loop {
        if *io.shutdown.borrow() {
            return;
        }
        let connecting = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(&io.url));
        let ws = tokio::select! {
            r = connecting => r,
            _ = io.shutdown.changed() => return,
        };
        let ws = match ws {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                tracing::warn!(url = %io.url, "room relay: {e}; retrying in {backoff:?}");
                wait_draining(&mut io, backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
            Err(_) => {
                tracing::warn!(url = %io.url, "room relay: connect timed out; retrying in {backoff:?}");
                wait_draining(&mut io, backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        backoff = BACKOFF_MIN;
        tracing::info!(url = %io.url, "room relay connected");
        io.inbound.lock().push(Inbound::Connected(true));
        let dropped = session(&mut io, ws).await;
        io.inbound.lock().push(Inbound::Connected(false));
        if *io.shutdown.borrow() {
            return;
        }
        if dropped {
            tracing::warn!(url = %io.url, "room relay dropped; reconnecting");
        }
    }
}

/// Sleep for `d` while throwing away queued samples (nobody is listening).
async fn wait_draining(io: &mut Io, d: Duration) {
    let deadline = tokio::time::sleep(d);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return,
            _ = io.shutdown.changed() => return,
            _ = io.sample_rx.recv() => {}
            _ = io.chat_rx.recv() => {}
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One connection's worth of traffic. Returns true when the relay closed on us.
async fn session(io: &mut Io, ws: Ws) -> bool {
    let (mut sink, mut stream) = ws.split();
    let hello = io.hello_rx.borrow_and_update().clone();
    if let Some(h) = hello {
        if sink.send(Message::text(h.to_string())).await.is_err() {
            return true;
        }
    }
    let mut batch: Vec<Value> = Vec::new();
    let mut flush = tokio::time::interval(Duration::from_millis(BATCH_MS));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut snap_tick = tokio::time::interval(Duration::from_secs_f64(1.0 / SNAPSHOT_HZ));
    snap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = io.shutdown.changed() => {
                let _ = sink.send(Message::Close(None)).await;
                return false;
            }
            changed = io.hello_rx.changed() => {
                if changed.is_err() { return false; }
                let hello = io.hello_rx.borrow_and_update().clone();
                if let Some(h) = hello {
                    if sink.send(Message::text(h.to_string())).await.is_err() { return true; }
                }
            }
            s = io.sample_rx.recv() => {
                let Some(s) = s else { return false };
                batch.push(sample_item(&s));
                if batch.len() >= MAX_BATCH && !send_batch(&mut sink, &mut batch).await { return true; }
            }
            _ = flush.tick() => {
                if !send_batch(&mut sink, &mut batch).await { return true; }
            }
            _ = snap_tick.tick() => {
                let frame = snapshot_frame(&io.snapshots.borrow().clone());
                if sink.send(Message::text(frame.to_string())).await.is_err() { return true; }
            }
            c = io.chat_rx.recv() => {
                let Some(c) = c else { return false };
                let frame = json!({ "type": "chat", "at": c.at, "from": c.from, "text": c.text });
                if sink.send(Message::text(frame.to_string())).await.is_err() { return true; }
            }
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    if let Some(i) = parse_inbound(&t) {
                        io.inbound.lock().push(i);
                    }
                }
                Some(Ok(Message::Close(_))) | None => return true,
                Some(Ok(_)) => {}
                Some(Err(e)) => { tracing::debug!("room relay ws error: {e}"); return true; }
            },
        }
    }
}

async fn send_batch(sink: &mut futures::stream::SplitSink<Ws, Message>, batch: &mut Vec<Value>) -> bool {
    if batch.is_empty() {
        return true;
    }
    let frame = json!({ "type": "samples", "items": std::mem::take(batch) });
    sink.send(Message::text(frame.to_string())).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{start, AudioBackendChoice, EngineOptions};
    use sonder_core::UserCommand;

    #[test]
    fn urls_and_ids() {
        assert_eq!(host_url("wss://relay.example/r/abc"), "wss://relay.example/r/abc/host");
        assert_eq!(host_url("ws://127.0.0.1:8788/r/abc/"), "ws://127.0.0.1:8788/r/abc/host");
        assert_eq!(host_url("ws://x/r/abc/host"), "ws://x/r/abc/host");
        assert_eq!(room_id("wss://relay.example/r/abc"), "abc");
        assert_eq!(room_id("ws://127.0.0.1:8788/r/test-1/host"), "test-1");
        assert_eq!(room_id("ws://x/r/q?x=1"), "q");
    }

    #[test]
    fn hello_round_trips_the_scene() {
        let scene = crate::load_scene("night-freeway").unwrap();
        let cfg = RoomConfig { url: "ws://x/r/a".into(), name: "me".into(), public: true };
        let h = hello_frame(&scene, 42, 10.0, &cfg);
        assert_eq!(h["type"], "hello");
        assert_eq!(h["seed"], 42);
        assert_eq!(h["title"], "Night Freeway");
        assert_eq!(h["name"], "me");
        assert_eq!(h["public"], true);
        let back = crate::parse_scene(h["scene_toml"].as_str().unwrap()).unwrap();
        assert_eq!(back.name, scene.name);
        assert_eq!(back.layers.len(), scene.layers.len());
        assert!(h["epoch_unix"].as_f64().unwrap() > 1.6e9);
    }

    #[test]
    fn inbound_frames_parse() {
        assert_eq!(parse_inbound(r#"{"type":"presence","listeners":3}"#), Some(Inbound::Presence(3)));
        let Some(Inbound::Chat(c)) = parse_inbound(r#"{"type":"chat","at":1.5,"from":"g","text":"hi"}"#) else { panic!() };
        assert_eq!((c.from.as_str(), c.text.as_str(), c.at), ("g", "hi", 1.5));
        assert_eq!(parse_inbound(r#"{"type":"samples"}"#), None);
        assert_eq!(parse_inbound("nope"), None);
    }

    /// A mock relay: accepts one host, records what it gets, and talks back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_publishes_hello_samples_and_chat_round_trips() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel::<Value>();
        let (to_host_tx, mut to_host_rx) = mpsc::unbounded_channel::<Value>();
        let (path_tx, path_rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut path = String::new();
            #[allow(clippy::result_large_err)] // tungstenite's handshake callback signature
            let ws = tokio_tungstenite::accept_hdr_async(tcp, |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                path = req.uri().path().to_string();
                Ok(resp)
            })
            .await
            .unwrap();
            let _ = path_tx.send(path);
            let (mut sink, mut stream) = ws.split();
            loop {
                tokio::select! {
                    m = stream.next() => match m {
                        Some(Ok(Message::Text(t))) => { let _ = frames_tx.send(serde_json::from_str(&t).unwrap()); }
                        Some(Ok(_)) => {}
                        _ => return,
                    },
                    Some(v) = to_host_rx.recv() => { sink.send(Message::text(v.to_string())).await.unwrap(); }
                }
            }
        });

        let scene = crate::load_scene("night-freeway").unwrap();
        let opts = EngineOptions {
            backend: AudioBackendChoice::Null,
            seed: Some(3),
            room: Some(RoomConfig { url: format!("ws://127.0.0.1:{port}/r/mock"), name: "host".into(), public: false }),
            ..EngineOptions::default()
        };
        let mut handle = start(scene, opts).await.unwrap();
        let mut rx = handle.snapshot.clone();
        assert_eq!(tokio::time::timeout(Duration::from_secs(5), path_rx).await.unwrap().unwrap(), "/r/mock/host");

        async fn next(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("relay got nothing").unwrap()
        }
        let hello = next(&mut frames_rx).await;
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["seed"], 3);
        assert_eq!(hello["title"], "Night Freeway");
        assert_eq!(hello["name"], "host");
        // Samples and snapshots follow (night-freeway's synthetic traffic emits steadily).
        let (mut saw_samples, mut saw_snapshot) = (false, false);
        for _ in 0..60 {
            let f = next(&mut frames_rx).await;
            match f["type"].as_str().unwrap() {
                "samples" => {
                    let items = f["items"].as_array().unwrap();
                    assert!(!items.is_empty());
                    assert!(items[0]["t"].is_number() && items[0]["s"].is_string() && items[0]["d"]["type"].is_string(), "{f}");
                    saw_samples = true;
                }
                "snapshot" => {
                    assert!(f["time"].is_number() && f["captions"].is_array());
                    saw_snapshot = true;
                }
                _ => {}
            }
            if saw_samples && saw_snapshot {
                break;
            }
        }
        assert!(saw_samples && saw_snapshot);

        // Presence and a guest's chat land in the snapshot (room + captions).
        to_host_tx.send(json!({ "type": "presence", "listeners": 4 })).unwrap();
        to_host_tx.send(json!({ "type": "chat", "at": 1.0, "from": "guest", "text": "hello host" })).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                rx.changed().await.unwrap();
                let snap = rx.borrow().clone();
                if let Some(room) = &snap.room {
                    if room.listeners == 4 && room.chat.iter().any(|c| c.from == "guest") {
                        assert_eq!(room.role, "host");
                        assert_eq!(room.id, "mock");
                        assert!(snap.captions.iter().any(|c| c.text == "guest: hello host" && (c.importance - 0.6).abs() < 1e-9), "{:?}", snap.captions);
                        break;
                    }
                }
            }
        })
        .await
        .expect("room state never reached the snapshot");

        // Chat said here goes out to the relay (and is captioned locally).
        handle.commands.send(UserCommand::Chat { from: "host".into(), text: "welcome".into() }).await.unwrap();
        let chat = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = frames_rx.recv().await.unwrap();
                if f["type"] == "chat" {
                    break f;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(chat["from"], "host");
        assert_eq!(chat["text"], "welcome");
        assert!(chat["at"].is_number());

        // A hot reload re-announces.
        let bare = "name = \"Bare\"\n[[layer]]\nname = \"drone\"\nrole = \"pad\"\ninstrument = \"pad.glass\"\n";
        handle.commands.send(UserCommand::LoadScene { toml: bare.into() }).await.unwrap();
        let hello2 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = frames_rx.recv().await.unwrap();
                if f["type"] == "hello" {
                    break f;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(hello2["title"], "Bare");

        handle.commands.send(UserCommand::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut handle.done).await.unwrap().unwrap().unwrap();
    }
}
