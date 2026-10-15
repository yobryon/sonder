//! `mqtt` — subscribe to an MQTT 3.1.1 broker and turn every PUBLISH into a `message` event.
//!
//! A hand-rolled minimal client (CONNECT/CONNACK, SUBSCRIBE/SUBACK, PUBLISH, PINGREQ/
//! PINGRESP, DISCONNECT; QoS 0 only) over two transports:
//!
//! * raw TCP — `mqtt://host:1883`;
//! * WebSocket — `ws://host:8080/mqtt` or `wss://host:8081/mqtt` with the `mqtt`
//!   subprotocol and binary frames (MQTT packets may span or share frames; both
//!   transports feed the same byte-stream decoder).
//!
//! `mqtts://` (TLS over raw TCP) is not supported — use `wss://` instead.
//!
//! # Public brokers
//!
//! The default is `test.mosquitto.org`. Its unauthenticated listener refuses the `#`
//! wildcard; with username `wildcard` (no password) `#` is allowed but the broker
//! **drops the connection after ~20 s**. The adapter treats that like any other
//! disconnect and reconnects with exponential backoff (a connection that lived
//! < 60 s does not reset the backoff, so a `#` firehose settles into bursts every
//! few seconds to minutes — subscribe to real topics for a steady stream).
//!
//! # Output
//!
//! * event `message` per PUBLISH (sampled to `max_rate`/s by keeping every Nth):
//!   attrs `topic`, `len` (payload bytes), `value` if the payload parses as a number,
//!   `text` (first 64 chars) if it is UTF-8, and up to 16 `data.*` leaves if it is a
//!   JSON object; intensity = clamp(len/512, 0.05, 1);
//! * readings every second: `rate` (messages/s, all messages, over 5 s), `topics`
//!   (distinct topics in the last 60 s), `bytes` (payload bytes/s over 5 s).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rand::RngExt;
use serde_json::Value as Json;
use sonder_core::{Seconds, SourceSpec, SourceState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Attrs, Emitter, RateWindow};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const DEFAULT_URL: &str = "mqtt://test.mosquitto.org:1883";

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

const CONNECT: u8 = 1;
const CONNACK: u8 = 2;
const PUBLISH: u8 = 3;
const PUBACK: u8 = 4;
const SUBSCRIBE: u8 = 8;
const SUBACK: u8 = 9;
const PINGREQ: u8 = 12;
const PINGRESP: u8 = 13;
const DISCONNECT: u8 = 14;

/// Largest packet we accept (the spec allows 256 MB; nobody needs that here).
const MAX_PACKET: usize = 1 << 20;

/// A decoded inbound control packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    ConnAck { session_present: bool, code: u8 },
    SubAck { id: u16, codes: Vec<u8> },
    Publish { topic: String, payload: Vec<u8>, qos: u8, retain: bool, id: Option<u16> },
    PubAck { id: u16 },
    PingResp,
    /// Anything else (we never send QoS>0 so nothing else should arrive).
    Other(u8),
}

fn put_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(u16::MAX as usize);
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&b[..n]);
}

fn packet(ty: u8, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push((ty << 4) | (flags & 0x0f));
    put_varint(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

/// CONNECT (protocol level 4, clean session).
pub fn encode_connect(client_id: &str, keepalive: u16, username: Option<&str>, password: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    put_str(&mut body, "MQTT");
    body.push(4);
    let mut flags = 0x02u8; // clean session
    if username.is_some() {
        flags |= 0x80;
    }
    if password.is_some() {
        flags |= 0x40;
    }
    body.push(flags);
    body.extend_from_slice(&keepalive.to_be_bytes());
    put_str(&mut body, client_id);
    if let Some(u) = username {
        put_str(&mut body, u);
    }
    if let Some(p) = password {
        put_str(&mut body, p);
    }
    packet(CONNECT, 0, &body)
}

/// SUBSCRIBE at QoS 0 for every topic filter.
pub fn encode_subscribe(id: u16, topics: &[String]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&id.to_be_bytes());
    for t in topics {
        put_str(&mut body, t);
        body.push(0);
    }
    packet(SUBSCRIBE, 0x02, &body)
}

pub fn encode_pingreq() -> Vec<u8> {
    packet(PINGREQ, 0, &[])
}

pub fn encode_disconnect() -> Vec<u8> {
    packet(DISCONNECT, 0, &[])
}

fn encode_puback(id: u16) -> Vec<u8> {
    packet(PUBACK, 0, &id.to_be_bytes())
}

/// Human-readable CONNACK return code.
pub fn connack_reason(code: u8) -> &'static str {
    match code {
        0 => "accepted",
        1 => "unacceptable protocol version",
        2 => "identifier rejected",
        3 => "server unavailable",
        4 => "bad user name or password",
        5 => "not authorized",
        _ => "unknown CONNACK code",
    }
}

/// Incremental byte-stream → packet decoder shared by both transports.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    pos: usize,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos > 64 * 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete packet, `Ok(None)` if more bytes are needed, `Err` on a malformed stream.
    pub fn next(&mut self) -> Result<Option<Packet>, String> {
        let b = &self.buf[self.pos..];
        if b.is_empty() {
            return Ok(None);
        }
        // Remaining length varint.
        let mut len = 0usize;
        let mut mult = 1usize;
        let mut i = 1usize;
        loop {
            let Some(&x) = b.get(i) else { return Ok(None) };
            len += (x & 0x7f) as usize * mult;
            i += 1;
            if x & 0x80 == 0 {
                break;
            }
            if i > 4 {
                return Err("malformed remaining length".into());
            }
            mult *= 128;
        }
        if len > MAX_PACKET {
            return Err(format!("packet too large ({len} bytes)"));
        }
        if b.len() < i + len {
            return Ok(None);
        }
        let head = b[0];
        let body = &b[i..i + len];
        let pkt = parse_body(head, body)?;
        self.pos += i + len;
        Ok(Some(pkt))
    }
}

fn read_str(b: &[u8], at: &mut usize) -> Result<String, String> {
    let n = b.get(*at..*at + 2).map(|x| u16::from_be_bytes([x[0], x[1]]) as usize).ok_or("truncated string")?;
    let s = b.get(*at + 2..*at + 2 + n).ok_or("truncated string")?;
    *at += 2 + n;
    Ok(String::from_utf8_lossy(s).into_owned())
}

fn parse_body(head: u8, body: &[u8]) -> Result<Packet, String> {
    let ty = head >> 4;
    let flags = head & 0x0f;
    Ok(match ty {
        CONNACK => {
            if body.len() < 2 {
                return Err("short CONNACK".into());
            }
            Packet::ConnAck { session_present: body[0] & 1 == 1, code: body[1] }
        }
        SUBACK => {
            if body.len() < 2 {
                return Err("short SUBACK".into());
            }
            Packet::SubAck { id: u16::from_be_bytes([body[0], body[1]]), codes: body[2..].to_vec() }
        }
        PUBLISH => {
            let qos = (flags >> 1) & 0x03;
            let retain = flags & 1 == 1;
            let mut at = 0usize;
            let topic = read_str(body, &mut at)?;
            let id = if qos > 0 {
                let x = body.get(at..at + 2).ok_or("short PUBLISH")?;
                at += 2;
                Some(u16::from_be_bytes([x[0], x[1]]))
            } else {
                None
            };
            Packet::Publish { topic, payload: body[at..].to_vec(), qos, retain, id }
        }
        PUBACK => {
            if body.len() < 2 {
                return Err("short PUBACK".into());
            }
            Packet::PubAck { id: u16::from_be_bytes([body[0], body[1]]) }
        }
        PINGRESP => Packet::PingResp,
        0 | 15 => return Err(format!("reserved packet type {ty}")),
        other => Packet::Other(other),
    })
}

// ---------------------------------------------------------------------------
// URL / transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Tcp { host: String, port: u16 },
    Ws { url: String },
}

/// `mqtt://host[:port]`, `tcp://…`, `ws://…`, `wss://…`. A bare `host[:port]` means TCP.
pub fn parse_url(url: &str) -> Result<Endpoint, String> {
    let u = url.trim();
    if u.starts_with("ws://") || u.starts_with("wss://") {
        return Ok(Endpoint::Ws { url: u.to_string() });
    }
    if u.starts_with("mqtts://") || u.starts_with("ssl://") || u.starts_with("tls://") {
        return Err("mqtts:// (TLS over TCP) is not supported; use wss://host:8081/mqtt".into());
    }
    let rest = u.strip_prefix("mqtt://").or_else(|| u.strip_prefix("tcp://")).unwrap_or(u);
    let rest = rest.split('/').next().unwrap_or("");
    if rest.is_empty() {
        return Err("empty host".into());
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') || h.starts_with('[') => {
            (h.trim_matches(['[', ']']).to_string(), p.parse::<u16>().map_err(|_| format!("bad port `{p}`"))?)
        }
        _ => (rest.trim_matches(['[', ']']).to_string(), 1883),
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    Ok(Endpoint::Tcp { host, port })
}

enum Conn {
    Tcp(tokio::net::TcpStream),
    Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>),
}

impl Conn {
    async fn connect(ep: &Endpoint) -> Result<Self, String> {
        let fut = async {
            match ep {
                Endpoint::Tcp { host, port } => tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .map(|s| {
                        let _ = s.set_nodelay(true);
                        Conn::Tcp(s)
                    })
                    .map_err(|e| format!("connect failed: {e}")),
                Endpoint::Ws { url } => {
                    let mut req = url.as_str().into_client_request().map_err(|e| format!("bad url: {e}"))?;
                    if let Ok(v) = "mqtt".parse() {
                        req.headers_mut().insert("Sec-WebSocket-Protocol", v);
                    }
                    if let Ok(v) = util::USER_AGENT.parse() {
                        req.headers_mut().insert("User-Agent", v);
                    }
                    let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| format!("connect failed: {e}"))?;
                    Ok(Conn::Ws(ws))
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| "connect timeout".to_string())?
    }

    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            Conn::Tcp(s) => s.write_all(&bytes).await.map_err(|e| format!("write failed: {e}")),
            Conn::Ws(ws) => ws.send(Message::binary(bytes)).await.map_err(|e| format!("ws send failed: {e}")),
        }
    }

    /// Read the next chunk of bytes; `Ok(None)` when the peer closed.
    async fn recv(&mut self, buf: &mut [u8]) -> Result<Option<Vec<u8>>, String> {
        match self {
            Conn::Tcp(s) => match s.read(buf).await {
                Ok(0) => Ok(None),
                Ok(n) => Ok(Some(buf[..n].to_vec())),
                Err(e) => Err(format!("read failed: {e}")),
            },
            Conn::Ws(ws) => loop {
                match ws.next().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(format!("ws error: {e}")),
                    Some(Ok(Message::Binary(b))) => return Ok(Some(b.to_vec())),
                    Some(Ok(Message::Text(t))) => return Ok(Some(t.as_bytes().to_vec())),
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(_)) => {}
                }
            },
        }
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = self.send(encode_disconnect()).await;
            if let Conn::Ws(ws) = &mut self {
                let _ = ws.close(None).await;
            }
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Message → event
// ---------------------------------------------------------------------------

/// Flatten up to `max` scalar leaves of a JSON object into `prefix.path` attrs.
pub fn flatten_leaves(v: &Json, prefix: &str, max: usize) -> Attrs {
    fn walk(v: &Json, path: &str, out: &mut Attrs, max: usize, depth: usize) {
        if out.0.len() >= max || depth > 8 {
            return;
        }
        match v {
            Json::Object(m) => {
                for (k, c) in m {
                    walk(c, &format!("{path}.{k}"), out, max, depth + 1);
                }
            }
            Json::Array(a) => {
                for (i, c) in a.iter().enumerate().take(16) {
                    walk(c, &format!("{path}[{i}]"), out, max, depth + 1);
                }
            }
            Json::Number(n) => {
                if let Some(f) = n.as_f64() {
                    *out = std::mem::take(out).num(path, f);
                }
            }
            Json::Bool(b) => *out = std::mem::take(out).bool(path, *b),
            Json::String(s) => {
                let t: String = s.chars().take(64).collect();
                *out = std::mem::take(out).text(path, t)
            }
            Json::Null => {}
        }
    }
    let mut out = Attrs::new();
    walk(v, prefix, &mut out, max, 0);
    out
}

/// Attributes and intensity for one PUBLISH.
pub fn message_attrs(topic: &str, payload: &[u8]) -> (Attrs, f64) {
    let len = payload.len();
    let mut a = Attrs::new().text("topic", topic).num("len", len as f64);
    if let Ok(text) = std::str::from_utf8(payload) {
        let t = text.trim();
        if let Ok(n) = t.parse::<f64>() {
            if n.is_finite() {
                a = a.num("value", n);
            }
        } else if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Json>(t) {
                let leaves = flatten_leaves(&v, "data", 16);
                a.0.extend(leaves.0);
            }
        }
        if !t.is_empty() {
            a = a.text("text", t.chars().take(64).collect::<String>());
        }
    }
    (a, (len as f64 / 512.0).clamp(0.05, 1.0))
}

/// Rolling statistics for the 1 Hz readings.
#[derive(Debug, Default)]
pub struct Stats {
    msgs: RateWindow,
    bytes: VecDeque<(Seconds, f64)>,
    topics: HashMap<String, Seconds>,
    recent: VecDeque<(Seconds, String)>,
    pub total: u64,
}

impl Stats {
    pub fn push(&mut self, at: Seconds, topic: &str, len: usize) {
        self.total += 1;
        self.msgs.push(at);
        self.bytes.push_back((at, len as f64));
        if self.bytes.len() > 100_000 {
            self.bytes.pop_front();
        }
        if self.topics.len() < 50_000 || self.topics.contains_key(topic) {
            self.topics.insert(topic.to_string(), at);
            self.recent.push_back((at, topic.to_string()));
        }
    }

    /// `(rate, topics, bytes)` at `now`.
    pub fn readings(&mut self, now: Seconds) -> [(&'static str, f64); 3] {
        while let Some(&(t, _)) = self.bytes.front() {
            if now - t > 5.0 {
                self.bytes.pop_front();
            } else {
                break;
            }
        }
        while let Some((t, topic)) = self.recent.front() {
            if now - *t > 60.0 {
                if self.topics.get(topic).is_some_and(|last| now - *last > 60.0) {
                    self.topics.remove(topic);
                }
                self.recent.pop_front();
            } else {
                break;
            }
        }
        let rate = self.msgs.rate(now, 5.0);
        let bytes = self.bytes.iter().map(|(_, b)| b).sum::<f64>() / 5.0;
        [("rate", rate), ("topics", self.topics.len() as f64), ("bytes", bytes)]
    }

    /// Keep every Nth message so that emitted events stay under `max_rate`/s.
    pub fn keep(&mut self, now: Seconds, max_rate: f64) -> bool {
        if max_rate <= 0.0 {
            return true;
        }
        let incoming = self.msgs.rate(now, 2.0);
        let n = (incoming / max_rate).ceil().max(1.0) as u64;
        self.total.is_multiple_of(n)
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub endpoint: Endpoint,
    pub topics: Vec<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub client_id: Option<String>,
    pub keepalive: u16,
    pub max_rate: f64,
}

struct MqttSource {
    cfg: Config,
}

impl Source for MqttSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let client_id = self.cfg.client_id.clone().unwrap_or_else(|| format!("sonder-{:04x}", rng.random_range(0u32..0xffff)));
            let mut stats = Stats::default();
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, "connecting").await;
                let started = std::time::Instant::now();
                let res = session(&mut em, &self.cfg, &client_id, &mut stats).await;
                if em.is_shutdown() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = res.err().unwrap_or_else(|| "connection closed".into());
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

/// Wait for a specific packet (skipping PUBLISHes, which are handed to `on_pub`).
async fn expect<F: FnMut(&Packet) -> bool>(conn: &mut Conn, dec: &mut Decoder, mut want: F, what: &str) -> Result<Packet, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        while let Some(p) = dec.next()? {
            if want(&p) {
                return Ok(p);
            }
        }
        let chunk = tokio::time::timeout_at(deadline, conn.recv(&mut buf)).await.map_err(|_| format!("timeout waiting for {what}"))??;
        match chunk {
            Some(b) => dec.feed(&b),
            None => return Err(format!("connection closed waiting for {what}")),
        }
    }
}

/// One connection: connect, subscribe, pump messages until the broker goes away.
async fn session(em: &mut Emitter, cfg: &Config, client_id: &str, stats: &mut Stats) -> Result<(), String> {
    let mut conn = Conn::connect(&cfg.endpoint).await?;
    let mut dec = Decoder::default();
    conn.send(encode_connect(client_id, cfg.keepalive, cfg.username.as_deref(), cfg.password.as_deref())).await?;
    match expect(&mut conn, &mut dec, |p| matches!(p, Packet::ConnAck { .. }), "CONNACK").await? {
        Packet::ConnAck { code: 0, .. } => {}
        Packet::ConnAck { code, .. } => {
            conn.close().await;
            return Err(format!("broker refused connection: {} ({code})", connack_reason(code)));
        }
        _ => return Err("unexpected packet".into()),
    }
    conn.send(encode_subscribe(1, &cfg.topics)).await?;
    // SUBACK may arrive after retained PUBLISHes; keep those.
    let mut early: Vec<Packet> = Vec::new();
    let suback = expect(&mut conn, &mut dec, |p| {
        if let Packet::Publish { .. } = p {
            early.push(p.clone());
            false
        } else {
            matches!(p, Packet::SubAck { .. })
        }
    }, "SUBACK")
    .await?;
    if let Packet::SubAck { codes, .. } = &suback {
        let refused: Vec<&String> = cfg.topics.iter().zip(codes).filter(|(_, c)| **c >= 0x80).map(|(t, _)| t).collect();
        if !refused.is_empty() {
            conn.close().await;
            return Err(format!("broker refused subscription to {}", refused.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")));
        }
    }
    em.status(SourceState::Live, format!("subscribed to {}", cfg.topics.join(", "))).await;
    for p in early {
        handle(em, cfg, stats, &mut conn, p).await?;
    }

    let keepalive = Duration::from_secs(cfg.keepalive.max(5) as u64);
    let ping_every = keepalive.mul_f64(0.5);
    let mut last_rx = std::time::Instant::now();
    let mut last_tx = std::time::Instant::now();
    let mut next_reading = em.now() + 1.0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if em.is_shutdown() {
            conn.close().await;
            return Ok(());
        }
        while let Some(p) = dec.next()? {
            handle(em, cfg, stats, &mut conn, p).await?;
        }
        let chunk = tokio::select! {
            c = conn.recv(&mut buf) => Some(c),
            _ = tokio::time::sleep(Duration::from_millis(250)) => None,
            _ = em.ctx.wait_shutdown() => { conn.close().await; return Ok(()); }
        };
        match chunk {
            Some(Ok(Some(b))) => {
                last_rx = std::time::Instant::now();
                dec.feed(&b);
            }
            Some(Ok(None)) => return Err("broker closed the connection".into()),
            Some(Err(e)) => return Err(e),
            None => {}
        }
        if last_tx.elapsed() >= ping_every {
            conn.send(encode_pingreq()).await?;
            last_tx = std::time::Instant::now();
        }
        if last_rx.elapsed() > keepalive.mul_f64(1.5) {
            return Err(format!("no response from broker for {:.0}s", last_rx.elapsed().as_secs_f64()));
        }
        let now = em.now();
        if now >= next_reading {
            next_reading = now + 1.0;
            if stats.total > 0 {
                em.reading(stats.readings(now)).await;
            }
        }
    }
}

async fn handle(em: &mut Emitter, cfg: &Config, stats: &mut Stats, conn: &mut Conn, p: Packet) -> Result<(), String> {
    match p {
        Packet::Publish { topic, payload, qos, id, .. } => {
            if qos == 1 {
                if let Some(id) = id {
                    conn.send(encode_puback(id)).await?;
                }
            }
            let now = em.now();
            stats.push(now, &topic, payload.len());
            if stats.keep(now, cfg.max_rate) {
                let (attrs, intensity) = message_attrs(&topic, &payload);
                em.event("message", intensity, attrs.into_map()).await;
            }
        }
        Packet::PingResp | Packet::SubAck { .. } | Packet::PubAck { .. } | Packet::ConnAck { .. } | Packet::Other(_) => {}
    }
    Ok(())
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "mqtt",
        summary: "MQTT 3.1.1 subscriber (TCP or WebSocket): a `message` event per PUBLISH, rate/topics/bytes readings",
        params: vec![
            ParamInfo { name: "url", summary: "mqtt://host:1883, ws://host:8080/mqtt or wss://host:8081/mqtt", default: Some(DEFAULT_URL) },
            ParamInfo { name: "topics", summary: "Topic filters (test.mosquitto.org needs username `wildcard` for `#`)", default: Some("[\"#\"]") },
            ParamInfo { name: "username", summary: "Optional username", default: None },
            ParamInfo { name: "password", summary: "Optional password", default: None },
            ParamInfo { name: "client_id", summary: "Client id", default: Some("sonder-xxxx (random)") },
            ParamInfo { name: "keepalive", summary: "Keepalive seconds", default: Some("60") },
            ParamInfo { name: "max_rate", summary: "Cap on emitted events/s (keeps every Nth); 0 = unlimited", default: Some("50") },
        ],
        channels: vec!["rate", "topics", "bytes"],
        events: vec!["message"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let url = util::param_str(spec, "url", DEFAULT_URL);
    let endpoint = parse_url(&url).map_err(|e| util::invalid(spec, "url", &e))?;
    let topics: Vec<String> = util::param_str_list(spec, "topics")
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let topics = if topics.is_empty() { vec!["#".to_string()] } else { topics };
    let keepalive = util::param_f64_in(spec, "keepalive", 60.0, 5.0, 65535.0)? as u16;
    let max_rate = util::param_f64_in(spec, "max_rate", 50.0, 0.0, 1e6)?;
    let client_id = spec.get_str("client_id").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    if client_id.as_ref().is_some_and(|c| c.len() > 65535) {
        return Err(util::invalid(spec, "client_id", "too long"));
    }
    Ok(Box::new(MqttSource {
        cfg: Config {
            endpoint,
            topics,
            username: spec.get_str("username").map(str::to_string),
            password: spec.get_str("password").map(str::to_string),
            client_id,
            keepalive,
            max_rate,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_and_connect_shape() {
        let mut v = Vec::new();
        put_varint(&mut v, 321);
        assert_eq!(v, vec![0xC1, 0x02]);
        let c = encode_connect("sonder-1", 60, Some("wildcard"), None);
        assert_eq!(c[0], 0x10);
        // "MQTT" string, level 4, flags clean+username, keepalive 60.
        assert_eq!(&c[2..8], &[0, 4, b'M', b'Q', b'T', b'T']);
        assert_eq!(c[8], 4);
        assert_eq!(c[9], 0x82);
        assert_eq!(&c[10..12], &60u16.to_be_bytes());
        let s = encode_subscribe(1, &["a/b".into(), "#".into()]);
        assert_eq!(s[0], 0x82);
        assert_eq!(s[s.len() - 1], 0);
        assert_eq!(encode_pingreq(), vec![0xC0, 0x00]);
        assert_eq!(encode_disconnect(), vec![0xE0, 0x00]);
    }

    #[test]
    fn decoder_handles_split_and_coalesced_packets() {
        // CONNACK, then a PUBLISH "t/1" with payload "42", then PINGRESP, across odd chunk boundaries.
        let mut stream = vec![0x20, 0x02, 0x00, 0x00];
        let mut pubbody = Vec::new();
        put_str(&mut pubbody, "t/1");
        pubbody.extend_from_slice(b"42");
        stream.extend(packet(PUBLISH, 0x01, &pubbody));
        stream.extend([0xD0, 0x00]);
        let mut dec = Decoder::default();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            dec.feed(chunk);
            while let Some(p) = dec.next().unwrap() {
                got.push(p);
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], Packet::ConnAck { session_present: false, code: 0 });
        assert_eq!(got[1], Packet::Publish { topic: "t/1".into(), payload: b"42".to_vec(), qos: 0, retain: true, id: None });
        assert_eq!(got[2], Packet::PingResp);
        // QoS 1 PUBLISH carries a packet id.
        let mut body = Vec::new();
        put_str(&mut body, "x");
        body.extend_from_slice(&7u16.to_be_bytes());
        body.extend_from_slice(b"p");
        let mut dec = Decoder::default();
        dec.feed(&packet(PUBLISH, 0x02, &body));
        assert!(matches!(dec.next().unwrap(), Some(Packet::Publish { id: Some(7), qos: 1, .. })));
        // Malformed: 5-byte remaining length.
        let mut dec = Decoder::default();
        dec.feed(&[0x30, 0xff, 0xff, 0xff, 0xff, 0x01]);
        assert!(dec.next().is_err());
    }

    #[test]
    fn message_attrs_number_json_and_text() {
        let (a, i) = message_attrs("sensors/t", b"21.5");
        let a = a.into_map();
        assert_eq!(a["value"].as_f64(), Some(21.5));
        assert_eq!(a["len"].as_f64(), Some(4.0));
        assert_eq!(a["topic"].as_str(), Some("sensors/t"));
        assert!((i - 0.05).abs() < 1e-9);

        let (a, _) = message_attrs("j", br#"{"temp": 20, "ok": true, "nest": {"hum": 55, "name": "x"}, "n": null}"#);
        let a = a.into_map();
        assert_eq!(a["data.temp"].as_f64(), Some(20.0));
        assert_eq!(a["data.nest.hum"].as_f64(), Some(55.0));
        assert_eq!(a["data.nest.name"].as_str(), Some("x"));
        assert!(a.get("value").is_none());

        let (a, i) = message_attrs("bin", &[0xff; 1024]);
        let a = a.into_map();
        assert!(a.get("text").is_none());
        assert_eq!(i, 1.0);
        // Leaf cap.
        let big: Json = serde_json::from_str(&format!("{{{}}}", (0..40).map(|i| format!("\"k{i}\": {i}")).collect::<Vec<_>>().join(","))).unwrap();
        assert_eq!(flatten_leaves(&big, "data", 16).0.len(), 16);
    }

    #[test]
    fn stats_rate_topics_and_sampling() {
        let mut s = Stats::default();
        for i in 0..100 {
            s.push(i as f64 * 0.01, if i % 2 == 0 { "a" } else { "b" }, 10);
        }
        let r: std::collections::BTreeMap<_, _> = s.readings(1.0).into_iter().collect();
        assert_eq!(r["topics"], 2.0);
        assert!((r["rate"] - 20.0).abs() < 1e-9);
        assert!((r["bytes"] - 200.0).abs() < 1e-9);
        // 100 msgs in 1 s ≈ 50/s over the 2 s window; cap 10/s → keep every 5th.
        let mut kept = 0;
        for i in 100..200 {
            s.push(1.0 + (i - 100) as f64 * 0.01, "a", 1);
            if s.keep(1.0 + (i - 100) as f64 * 0.01, 10.0) {
                kept += 1;
            }
        }
        assert!((5..=25).contains(&kept), "kept {kept}");
        // Topics age out.
        let r: std::collections::BTreeMap<_, _> = s.readings(100.0).into_iter().collect();
        assert_eq!(r["topics"], 0.0);
        assert_eq!(r["rate"], 0.0);
    }

    #[test]
    fn urls_and_factory() {
        assert_eq!(parse_url("mqtt://test.mosquitto.org:1883").unwrap(), Endpoint::Tcp { host: "test.mosquitto.org".into(), port: 1883 });
        assert_eq!(parse_url("broker.local").unwrap(), Endpoint::Tcp { host: "broker.local".into(), port: 1883 });
        assert_eq!(parse_url("ws://x:8080/mqtt").unwrap(), Endpoint::Ws { url: "ws://x:8080/mqtt".into() });
        assert!(parse_url("mqtts://x").is_err());
        assert!(parse_url("mqtt://x:notaport").is_err());
        assert!(factory(&SourceSpec::new("mqtt")).is_ok());
        assert!(factory(&SourceSpec::new("mqtt").with("url", "mqtts://x")).is_err());
        assert!(factory(&SourceSpec::new("mqtt").with("keepalive", 1)).is_err());
        assert!(factory(&SourceSpec::new("mqtt").with("topics", "a/#, b")).is_ok());
    }

    /// In-process broker-ish server: accepts CONNECT, SUBSCRIBE, pushes a few PUBLISHes, closes.
    async fn fake_broker(listener: tokio::net::TcpListener, publishes: usize) {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let mut buf = vec![0u8; 4096];
        let mut dec = Decoder::default();
        // CONNECT
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            dec.feed(&buf[..n]);
            // We don't decode CONNECT (client-only decoder) — just look for the header byte.
            if dec.buf.first() == Some(&0x10) && dec.buf.len() >= 12 {
                break;
            }
        }
        sock.write_all(&[0x20, 0x02, 0x00, 0x00]).await.unwrap();
        dec = Decoder::default();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            dec.feed(&buf[..n]);
            if dec.buf.first().is_some_and(|b| b >> 4 == SUBSCRIBE) && dec.buf.len() >= 7 {
                break;
            }
        }
        sock.write_all(&packet(SUBACK, 0, &[0, 1, 0])).await.unwrap();
        for i in 0..publishes {
            let mut body = Vec::new();
            put_str(&mut body, "fake/t");
            body.extend_from_slice(format!("{{\"i\": {i}}}").as_bytes());
            sock.write_all(&packet(PUBLISH, 0, &body)).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = sock.shutdown().await;
    }

    #[tokio::test]
    async fn end_to_end_against_fake_broker_and_shutdown() {
        use sonder_core::Datum;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let broker = tokio::spawn(fake_broker(listener, 5));
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: "m".into(),
            tx,
            clock: std::sync::Arc::new(sonder_core::WallClock::new()),
            http: crate::default_http_client(),
            shutdown: srx,
            seed: 3,
        };
        let src = factory(&SourceSpec::new("mqtt").with("url", format!("mqtt://{addr}")).with("topics", "fake/t")).unwrap();
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        stx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("stops on shutdown").unwrap();
        broker.abort();
        let mut events = 0;
        let mut readings = 0;
        let mut states = Vec::new();
        while let Ok(s) = rx.try_recv() {
            match s.datum {
                Datum::Event { name, attrs, .. } => {
                    assert_eq!(name, "message");
                    assert_eq!(attrs["topic"].as_str(), Some("fake/t"));
                    assert!(attrs.contains_key("data.i"));
                    events += 1;
                }
                Datum::Reading { values } => {
                    assert!(values.contains_key("rate"));
                    readings += 1;
                }
                Datum::Status { state, .. } => states.push(state),
                _ => {}
            }
        }
        assert_eq!(events, 5);
        assert!(readings >= 1, "readings {readings}");
        assert!(states.contains(&SourceState::Live));
        assert!(states.contains(&SourceState::Reconnecting), "{states:?}");
    }

    async fn live_collect(cfg: Config, secs: u64) -> (Vec<Packet>, Result<(), String>) {
        let mut conn = Conn::connect(&cfg.endpoint).await.unwrap();
        let mut dec = Decoder::default();
        conn.send(encode_connect("sonder-test", 30, cfg.username.as_deref(), cfg.password.as_deref())).await.unwrap();
        let ack = expect(&mut conn, &mut dec, |p| matches!(p, Packet::ConnAck { .. }), "CONNACK").await.unwrap();
        assert!(matches!(ack, Packet::ConnAck { code: 0, .. }), "{ack:?}");
        conn.send(encode_subscribe(1, &cfg.topics)).await.unwrap();
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        let mut buf = vec![0u8; 64 * 1024];
        let end = loop {
            while let Some(p) = dec.next().unwrap() {
                got.push(p);
            }
            match tokio::time::timeout_at(deadline, conn.recv(&mut buf)).await {
                Ok(Ok(Some(b))) => dec.feed(&b),
                Ok(Ok(None)) => break Err("closed".to_string()),
                Ok(Err(e)) => break Err(e),
                Err(_) => break Ok(()),
            }
        };
        (got, end)
    }

    /// `cargo test -p sonder-sources -- --ignored mqtt`
    #[tokio::test]
    #[ignore]
    async fn live_mosquitto_tcp_wildcard_and_ws_topic() {
        // TCP, `#` with the `wildcard` user: the broker closes after ~20 s.
        let cfg = Config {
            endpoint: parse_url(DEFAULT_URL).unwrap(),
            topics: vec!["#".into()],
            username: Some("wildcard".into()),
            password: None,
            client_id: None,
            keepalive: 30,
            max_rate: 50.0,
        };
        let (got, end) = live_collect(cfg, 25).await;
        let pubs = got.iter().filter(|p| matches!(p, Packet::Publish { .. })).count();
        let topics: std::collections::HashSet<_> =
            got.iter().filter_map(|p| if let Packet::Publish { topic, .. } = p { Some(topic.clone()) } else { None }).collect();
        eprintln!("tcp `#`: {pubs} publishes on {} topics; end: {end:?}", topics.len());
        assert!(got.iter().any(|p| matches!(p, Packet::SubAck { codes, .. } if codes[0] < 0x80)), "{:?}", got.first());
        assert!(pubs > 0);

        // WebSocket, specific topic ($SYS stats update every ~10 s).
        let cfg = Config {
            endpoint: parse_url("ws://test.mosquitto.org:8080/mqtt").unwrap(),
            topics: vec!["$SYS/broker/uptime".into(), "$SYS/broker/load/messages/received/1min".into()],
            username: None,
            password: None,
            client_id: None,
            keepalive: 30,
            max_rate: 50.0,
        };
        let (got, end) = live_collect(cfg, 15).await;
        let pubs: Vec<_> = got.iter().filter(|p| matches!(p, Packet::Publish { .. })).collect();
        eprintln!("ws $SYS: {} publishes; end: {end:?}; first: {:?}", pubs.len(), pubs.first());
        assert!(!pubs.is_empty());
        if let Some(Packet::Publish { topic, payload, .. }) = pubs.first() {
            let (a, _) = message_attrs(topic, payload);
            eprintln!("attrs: {:?}", a.into_map());
        }
    }
}
