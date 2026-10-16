//! A tiny pub/sub relay for shared Sonder rooms (docs/ROOMS.md).
//!
//! One **host** (a native `sonder play|serve|radio --room …`) publishes its performance as
//! JSON text frames; any number of **guests** (browsers) receive them. The relay knows
//! nothing about music: it stores the host's latest `hello`, fans every host frame out to
//! the guests, counts listeners and passes chat around.
//!
//! ```text
//! WS  /r/{room}/host   one host at a time; a second host replaces the first
//! WS  /r/{room}/join   guests
//! GET /r/{room}        {"id","present","listeners","title","name","public","since","host_time"}
//! GET /                plain-text index of public rooms
//! ```
//!
//! Protocol (JSON text frames):
//! - host → relay: `{"type":"hello","scene_toml":…,"seed":u64,"epoch_unix":f64,"name":…,
//!   "title":…,"public":bool}`, then `{"type":"samples","items":[{"t","s","d"}]}` batches,
//!   `{"type":"snapshot","time","captions","form"}` at ~2 Hz, `{"type":"chat",…}`.
//! - relay → guests: the latest `hello` on join, then every host frame, plus
//!   `{"type":"presence","listeners":n}` on change and `{"type":"chat","at","from","text"}`
//!   from anyone. `{"type":"closed"}` when the room expires.
//! - guests → relay: `chat` (240 chars, 1/s per client) and `{"type":"ping","sent":ms}` →
//!   `{"type":"pong","sent","relay_ms","host_time","host_age_ms"}`.
//! - relay → host: guests' `chat` and `presence`.
//!
//! Buffers are bounded: a guest that cannot keep up loses the oldest frames, never the
//! connection. Rooms hold [`MAX_LISTENERS`] and expire [`ROOM_TTL`] after the host leaves.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

/// Guests per room.
pub const MAX_LISTENERS: u32 = 256;
/// A room without a host is removed after this long.
pub const ROOM_TTL: Duration = Duration::from_secs(60);
/// Longest chat line, in characters.
pub const CHAT_MAX_CHARS: usize = 240;
/// Longest chat display name, in characters.
pub const NAME_MAX_CHARS: usize = 32;
/// Minimum spacing between two chat lines from one connection.
pub const CHAT_INTERVAL: Duration = Duration::from_secs(1);
/// Frames buffered per guest before the oldest are dropped.
const BUFFER: usize = 512;
/// Largest frame a host may send (a sample batch) / a guest may send.
const HOST_MAX_FRAME: usize = 4 << 20;
const GUEST_MAX_FRAME: usize = 4 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Anything the host published (hello, samples, snapshot).
    Data,
    Chat { from_host: bool },
    Presence,
}

#[derive(Clone, Debug)]
struct Msg {
    kind: Kind,
    text: Arc<str>,
}

impl Msg {
    fn new(kind: Kind, v: &Value) -> Self {
        Self { kind, text: Arc::from(v.to_string()) }
    }
    fn message(&self) -> Message {
        Message::text(self.text.to_string())
    }
}

struct RoomState {
    hello: Option<Arc<str>>,
    title: String,
    name: String,
    public: bool,
    present: bool,
    /// Unix seconds the current host arrived (or the room was created).
    since: f64,
    listeners: u32,
    /// Latest engine time the host reported, and when.
    host_time: f64,
    host_time_at: Option<Instant>,
    /// Bumped per host connection; the previous host sees it change and leaves.
    gen: u64,
    expiry: Option<tokio::task::JoinHandle<()>>,
}

struct Room {
    id: String,
    tx: broadcast::Sender<Msg>,
    gen_tx: watch::Sender<u64>,
    state: Mutex<RoomState>,
}

/// What `GET /r/{room}` returns.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct RoomInfo {
    pub id: String,
    pub present: bool,
    pub listeners: u32,
    pub title: String,
    pub name: String,
    pub public: bool,
    pub since: f64,
    pub host_time: f64,
}

impl Room {
    fn new(id: &str) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BUFFER);
        let (gen_tx, _) = watch::channel(0);
        Arc::new(Self {
            id: id.to_string(),
            tx,
            gen_tx,
            state: Mutex::new(RoomState {
                hello: None,
                title: String::new(),
                name: String::new(),
                public: false,
                present: false,
                since: unix_now(),
                listeners: 0,
                host_time: 0.0,
                host_time_at: None,
                gen: 0,
                expiry: None,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RoomState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn info(&self) -> RoomInfo {
        let s = self.lock();
        RoomInfo {
            id: self.id.clone(),
            present: s.present,
            listeners: s.listeners,
            title: s.title.clone(),
            name: s.name.clone(),
            public: s.public,
            since: s.since,
            host_time: host_time_now(&s),
        }
    }

    fn broadcast(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    fn presence(&self, listeners: u32) {
        self.broadcast(Msg::new(Kind::Presence, &json!({ "type": "presence", "listeners": listeners })));
    }
}

/// Host engine time right now, extrapolated from the last snapshot.
fn host_time_now(s: &RoomState) -> f64 {
    match s.host_time_at {
        Some(at) if s.present => s.host_time + at.elapsed().as_secs_f64(),
        _ => s.host_time,
    }
}

/// All rooms. Build with [`Relay::new`], serve with [`router`].
pub struct Relay {
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    ttl: Duration,
}

impl Relay {
    pub fn new() -> Arc<Self> {
        Self::with_ttl(ROOM_TTL)
    }

    /// A relay whose empty rooms expire after `ttl` (tests use a short one).
    pub fn with_ttl(ttl: Duration) -> Arc<Self> {
        Arc::new(Self { rooms: Mutex::new(HashMap::new()), ttl })
    }

    fn rooms(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Room>>> {
        self.rooms.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn get(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms().get(id).cloned()
    }

    /// The room, created (hostless, with an expiry running) if it does not exist.
    fn get_or_create(self: &Arc<Self>, id: &str) -> Arc<Room> {
        let mut rooms = self.rooms();
        if let Some(r) = rooms.get(id) {
            return r.clone();
        }
        let room = Room::new(id);
        rooms.insert(id.to_string(), room.clone());
        drop(rooms);
        self.schedule_expiry(&room, 0);
        room
    }

    /// Remove the room after `ttl` unless a host (of a newer generation) has arrived.
    fn schedule_expiry(self: &Arc<Self>, room: &Arc<Room>, gen: u64) {
        let relay = self.clone();
        let room2 = room.clone();
        let ttl = self.ttl;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            let mut rooms = relay.rooms();
            {
                let s = room2.lock();
                if s.present || s.gen != gen {
                    return;
                }
            }
            if rooms.get(&room2.id).is_some_and(|r| Arc::ptr_eq(r, &room2)) {
                rooms.remove(&room2.id);
                tracing::info!(room = %room2.id, "room expired");
                room2.broadcast(Msg::new(Kind::Data, &json!({ "type": "closed" })));
            }
        });
        let mut s = room.lock();
        if let Some(old) = s.expiry.replace(handle) {
            old.abort();
        }
    }

    /// Public, hosted rooms for the index.
    pub fn public_rooms(&self) -> Vec<RoomInfo> {
        let mut out: Vec<RoomInfo> = self.rooms().values().map(|r| r.info()).filter(|i| i.public && i.present).collect();
        out.sort_by(|a, b| b.listeners.cmp(&a.listeners).then_with(|| a.id.cmp(&b.id)));
        out
    }
}

/// The axum app.
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/r/{room}", get(room_info))
        .route("/r/{room}/host", get(host_ws))
        .route("/r/{room}/join", get(join_ws))
        .with_state(relay)
}

async fn index(State(relay): State<Arc<Relay>>) -> Response {
    let rooms = relay.public_rooms();
    let mut text = String::from("sonder-relay\n\n");
    if rooms.is_empty() {
        text.push_str("no public rooms right now\n");
    }
    let now = unix_now();
    for r in rooms {
        let title = if r.title.is_empty() { "(untitled)" } else { r.title.as_str() };
        let by = if r.name.is_empty() { String::new() } else { format!(" by {}", r.name) };
        text.push_str(&format!("{:<24} {}{}  {} listening  up {}\n", r.id, title, by, r.listeners, mmss(now - r.since)));
    }
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

async fn room_info(Path(room): Path<String>, State(relay): State<Arc<Relay>>) -> Response {
    match relay.get(&room) {
        Some(r) => Json(r.info()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such room", "id": room }))).into_response(),
    }
}

async fn host_ws(ws: WebSocketUpgrade, Path(room): Path<String>, State(relay): State<Arc<Relay>>) -> Response {
    if !valid_room_id(&room) {
        return (StatusCode::BAD_REQUEST, "room ids are 1–64 of [A-Za-z0-9_-]").into_response();
    }
    ws.max_message_size(HOST_MAX_FRAME).on_upgrade(move |socket| host_loop(socket, relay, room))
}

async fn join_ws(ws: WebSocketUpgrade, Path(room): Path<String>, State(relay): State<Arc<Relay>>) -> Response {
    if !valid_room_id(&room) {
        return (StatusCode::BAD_REQUEST, "room ids are 1–64 of [A-Za-z0-9_-]").into_response();
    }
    ws.max_message_size(GUEST_MAX_FRAME).on_upgrade(move |socket| guest_loop(socket, relay, room))
}

/// `[A-Za-z0-9_-]{1,64}`.
pub fn valid_room_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

async fn host_loop(mut socket: WebSocket, relay: Arc<Relay>, id: String) {
    let room = relay.get_or_create(&id);
    let my_gen = {
        let mut s = room.lock();
        s.gen += 1;
        s.present = true;
        s.since = unix_now();
        if let Some(e) = s.expiry.take() {
            e.abort();
        }
        s.gen
    };
    let _ = room.gen_tx.send(my_gen);
    tracing::info!(room = %id, gen = my_gen, "host connected");
    let mut gen_rx = room.gen_tx.subscribe();
    let mut rx = room.tx.subscribe();
    let mut gate = ChatGate::default();
    // The host learns how many are listening right away.
    let listeners = room.lock().listeners;
    if socket.send(Message::text(json!({ "type": "presence", "listeners": listeners }).to_string())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            changed = gen_rx.changed() => {
                if changed.is_err() || *gen_rx.borrow() != my_gen {
                    let _ = socket.send(Message::text(json!({ "type": "error", "error": "replaced by another host" }).to_string())).await;
                    let _ = socket.send(Message::Close(None)).await;
                    tracing::info!(room = %id, gen = my_gen, "host replaced");
                    return;
                }
            }
            m = rx.recv() => match m {
                Ok(msg) if matches!(msg.kind, Kind::Chat { from_host: false } | Kind::Presence) => {
                    if socket.send(msg.message()).await.is_err() { break; }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => on_host_frame(&room, &text, &mut gate),
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { tracing::debug!(room = %id, "host ws error: {e}"); break; }
            },
        }
    }
    // Still the host? Then the room goes dark and starts its expiry clock.
    let mine = {
        let mut s = room.lock();
        if s.gen == my_gen {
            s.present = false;
            s.host_time = host_time_now(&s);
            s.host_time_at = None;
            true
        } else {
            false
        }
    };
    if mine {
        tracing::info!(room = %id, "host left");
        relay.schedule_expiry(&room, my_gen);
    }
}

fn on_host_frame(room: &Arc<Room>, text: &str, gate: &mut ChatGate) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v.get("type").and_then(Value::as_str) {
        Some("hello") => {
            let msg = Msg::new(Kind::Data, &v);
            let mut s = room.lock();
            s.title = v.get("title").and_then(Value::as_str).map(str::to_string).or_else(|| v.get("scene_toml").and_then(Value::as_str).and_then(scene_title)).unwrap_or_default();
            s.name = v.get("name").and_then(Value::as_str).unwrap_or_default().chars().take(NAME_MAX_CHARS).collect();
            s.public = v.get("public").and_then(Value::as_bool).unwrap_or(false);
            s.hello = Some(msg.text.clone());
            drop(s);
            room.broadcast(msg);
        }
        Some("snapshot") => {
            if let Some(t) = v.get("time").and_then(Value::as_f64) {
                let mut s = room.lock();
                s.host_time = t;
                s.host_time_at = Some(Instant::now());
            }
            room.broadcast(Msg::new(Kind::Data, &v));
        }
        Some("samples") => room.broadcast(Msg::new(Kind::Data, &v)),
        Some("chat") => {
            if !gate.allow(Instant::now()) {
                return;
            }
            let at = v.get("at").and_then(Value::as_f64).unwrap_or_else(|| host_time_now(&room.lock()));
            if let Some(chat) = chat_frame(&v, at) {
                room.broadcast(Msg::new(Kind::Chat { from_host: true }, &chat));
            }
        }
        _ => {}
    }
}

async fn guest_loop(mut socket: WebSocket, relay: Arc<Relay>, id: String) {
    let room = relay.get_or_create(&id);
    let admitted = {
        let mut s = room.lock();
        if s.listeners >= MAX_LISTENERS {
            None
        } else {
            s.listeners += 1;
            Some((s.hello.clone(), s.listeners))
        }
    };
    let Some((hello, listeners)) = admitted else {
        let _ = socket.send(Message::text(json!({ "type": "error", "error": "room full" }).to_string())).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    // Subscribe before announcing, so this guest sees its own presence update.
    let mut rx = room.tx.subscribe();
    tracing::debug!(room = %id, listeners, "guest joined");
    if let Some(h) = hello {
        if socket.send(Message::text(h.to_string())).await.is_err() {
            leave(&room, &id);
            return;
        }
    }
    room.presence(listeners);
    let mut gate = ChatGate::default();
    loop {
        tokio::select! {
            m = rx.recv() => match m {
                Ok(msg) => { if socket.send(msg.message()).await.is_err() { break; } }
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::debug!(room = %id, dropped = n, "slow guest"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = on_guest_frame(&room, &text, &mut gate) {
                        if socket.send(Message::text(reply.to_string())).await.is_err() { break; }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { tracing::debug!(room = %id, "guest ws error: {e}"); break; }
            },
        }
    }
    leave(&room, &id);
}

fn leave(room: &Arc<Room>, id: &str) {
    let n = {
        let mut s = room.lock();
        s.listeners = s.listeners.saturating_sub(1);
        s.listeners
    };
    tracing::debug!(room = %id, listeners = n, "guest left");
    room.presence(n);
}

/// Handle one guest frame; `Some` is a direct reply to that guest.
fn on_guest_frame(room: &Arc<Room>, text: &str, gate: &mut ChatGate) -> Option<Value> {
    let v = serde_json::from_str::<Value>(text).ok()?;
    match v.get("type").and_then(Value::as_str)? {
        "ping" => {
            let s = room.lock();
            let host_age_ms = s.host_time_at.map(|t| t.elapsed().as_secs_f64() * 1000.0).unwrap_or(-1.0);
            Some(json!({
                "type": "pong",
                "sent": v.get("sent").cloned().unwrap_or(Value::Null),
                "relay_ms": unix_now() * 1000.0,
                "host_time": host_time_now(&s),
                "host_age_ms": host_age_ms,
                "present": s.present,
            }))
        }
        "chat" => {
            if !gate.allow(Instant::now()) {
                return Some(json!({ "type": "error", "error": "slow down: one message per second" }));
            }
            let at = v.get("at").and_then(Value::as_f64).unwrap_or_else(|| host_time_now(&room.lock()));
            let chat = chat_frame(&v, at)?;
            room.broadcast(Msg::new(Kind::Chat { from_host: false }, &chat));
            None
        }
        _ => None,
    }
}

/// Canonical chat frame from a client's `{"from","text"}`; `None` when there is nothing to say.
pub fn chat_frame(v: &Value, at: f64) -> Option<Value> {
    let text = sanitize_chat(v.get("text").and_then(Value::as_str).unwrap_or_default());
    if text.is_empty() {
        return None;
    }
    let from = sanitize_name(v.get("from").and_then(Value::as_str).unwrap_or_default());
    Some(json!({ "type": "chat", "at": at, "from": from, "text": text }))
}

/// Trim, drop control characters, cap at [`CHAT_MAX_CHARS`].
pub fn sanitize_chat(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() || *c == ' ').collect::<String>().trim().chars().take(CHAT_MAX_CHARS).collect::<String>().trim_end().to_string()
}

/// Trim, drop control characters, cap at [`NAME_MAX_CHARS`]; empty → "anon".
pub fn sanitize_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| !c.is_control()).take(NAME_MAX_CHARS).collect::<String>().trim().to_string();
    if n.is_empty() {
        "anon".into()
    } else {
        n
    }
}

/// One chat line per [`CHAT_INTERVAL`] per connection.
#[derive(Debug, Default)]
pub struct ChatGate {
    last: Option<Instant>,
}

impl ChatGate {
    /// May a message sent at `now` go through? (Records it if so.)
    pub fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|l| now.duration_since(l) < CHAT_INTERVAL) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// The `name = "…"` line of a scene TOML, if any.
pub fn scene_title(toml: &str) -> Option<String> {
    toml.lines().map(str::trim).find_map(|l| {
        let rest = l.strip_prefix("name")?.trim_start().strip_prefix('=')?.trim();
        let inner = rest.strip_prefix('"')?.split('"').next()?;
        Some(inner.to_string())
    })
}

pub fn unix_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn mmss(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_is_sanitized_and_capped() {
        let long = "x".repeat(400);
        let f = chat_frame(&json!({ "from": "  bob\n", "text": format!("  hi\u{7}there {long}") }), 3.5).unwrap();
        assert_eq!(f["from"], "bob");
        assert_eq!(f["at"], 3.5);
        let t = f["text"].as_str().unwrap();
        assert!(t.starts_with("hithere x"));
        assert_eq!(t.chars().count(), CHAT_MAX_CHARS);
        assert!(chat_frame(&json!({ "text": "   " }), 0.0).is_none());
        assert_eq!(chat_frame(&json!({ "text": "yo" }), 0.0).unwrap()["from"], "anon");
    }

    #[test]
    fn gate_allows_one_per_second() {
        let mut g = ChatGate::default();
        let t0 = Instant::now();
        assert!(g.allow(t0));
        assert!(!g.allow(t0 + Duration::from_millis(500)));
        assert!(g.allow(t0 + Duration::from_millis(1001)));
    }

    #[test]
    fn title_from_toml_and_room_ids() {
        assert_eq!(scene_title("# c\nname = \"Night Freeway\"\ndescription = \"x\"").as_deref(), Some("Night Freeway"));
        assert_eq!(scene_title("[[layer]]\nname = \"pad\""), Some("pad".into()));
        assert!(scene_title("x = 1").is_none());
        assert!(valid_room_id("test-1_A"));
        assert!(!valid_room_id(""));
        assert!(!valid_room_id("a/b"));
        assert!(!valid_room_id(&"a".repeat(65)));
    }
}
