//! Shared plumbing for adapters: parameter parsing, status de-duplication,
//! backoff, polling loops, HTTP helpers, sliding-window rates and bounded
//! de-dup sets. Adapters stay small by leaning on these.

use std::collections::{BTreeMap, HashSet, VecDeque};
#[cfg(feature = "native")]
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use rand::RngExt;
use serde_json::Value as Json;
use sonder_core::{Seconds, SourceSpec, Value};
#[cfg(feature = "native")]
use sonder_core::{Datum, Sample, SourceState};

#[cfg(feature = "native")]
use crate::SourceCtx;
use crate::SourceError;

/// User-Agent sent on every outbound request.
pub const USER_AGENT: &str = "sonder/0.1 (+https://github.com/yobryon/sonder)";

/// Default per-request timeout.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Parameter helpers
// ---------------------------------------------------------------------------

/// Numeric parameter with default; errors if present but not a number.
pub fn param_f64(spec: &SourceSpec, key: &'static str, default: f64) -> Result<f64, SourceError> {
    match spec.params.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| invalid(spec, key, "expected a number")),
    }
}

/// Numeric parameter that must lie within `lo..=hi`.
pub fn param_f64_in(spec: &SourceSpec, key: &'static str, default: f64, lo: f64, hi: f64) -> Result<f64, SourceError> {
    let v = param_f64(spec, key, default)?;
    if !v.is_finite() || v < lo || v > hi {
        return Err(invalid(spec, key, &format!("expected a number in {lo}..={hi}, got {v}")));
    }
    Ok(v)
}

/// Positive-seconds parameter (interval, period).
pub fn param_secs(spec: &SourceSpec, key: &'static str, default: f64, min: f64) -> Result<Duration, SourceError> {
    let v = param_f64(spec, key, default)?;
    if !v.is_finite() || v < min {
        return Err(invalid(spec, key, &format!("expected seconds ≥ {min}, got {v}")));
    }
    Ok(Duration::from_secs_f64(v))
}

/// Boolean parameter with default (accepts true/false, 0/1, "true"/"false").
pub fn param_bool(spec: &SourceSpec, key: &'static str, default: bool) -> Result<bool, SourceError> {
    match spec.params.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::Bool(b)) => Ok(*b),
        Some(Json::Number(n)) => Ok(n.as_f64().unwrap_or(0.0) != 0.0),
        Some(Json::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => Err(invalid(spec, key, "expected a boolean")),
        },
        Some(_) => Err(invalid(spec, key, "expected a boolean")),
    }
}

/// String parameter with default.
pub fn param_str(spec: &SourceSpec, key: &str, default: &str) -> String {
    spec.get_str(key).map(str::to_string).unwrap_or_else(|| default.to_string())
}

/// List-of-strings parameter (a single string or an array; comma-separated string also accepted).
pub fn param_str_list(spec: &SourceSpec, key: &str) -> Option<Vec<String>> {
    match spec.params.get(key)? {
        Json::String(s) if s.contains(',') => {
            Some(s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        }
        _ => spec.get_str_list(key),
    }
}

/// Optional `u64` seed parameter.
pub fn param_seed(spec: &SourceSpec) -> Result<Option<u64>, SourceError> {
    match spec.params.get("seed") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|i| i as u64))
            .or_else(|| n.as_f64().map(|f| f as u64))
            .map(Some)
            .ok_or_else(|| invalid(spec, "seed", "expected an integer")),
        Some(Json::String(s)) => Ok(Some(s.parse().unwrap_or_else(|_| hash_str(s)))),
        Some(_) => Err(invalid(spec, "seed", "expected an integer")),
    }
}

/// Build an `InvalidParam` error for `spec`.
pub fn invalid(spec: &SourceSpec, param: &'static str, message: &str) -> SourceError {
    SourceError::InvalidParam { kind: spec.kind.clone(), param, message: message.to_string() }
}

// ---------------------------------------------------------------------------
// Hashing / seeding
// ---------------------------------------------------------------------------

/// Stable 64-bit FNV-1a hash of a string (does not depend on `std`'s randomized hasher).
pub fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Seed for a source instance: engine seed mixed with the source id (and an optional
/// explicit `seed` param), so two instances of the same kind diverge.
pub fn instance_seed(ctx_seed: u64, id: &str, explicit: Option<u64>) -> u64 {
    let base = explicit.unwrap_or(ctx_seed);
    base ^ hash_str(id).rotate_left(17)
}

/// Deterministic ChaCha8 RNG from a u64 seed.
pub fn rng_from_seed(seed: u64) -> rand_chacha::ChaCha8Rng {
    rand::SeedableRng::seed_from_u64(seed)
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Exponential backoff with jitter: 1 s · 2^attempt, capped at 5 min, ±25 % jitter.
pub fn retry_delay(attempt: u32, rng: &mut impl RngExt) -> Duration {
    let base = 1.0_f64 * 2f64.powi(attempt.min(12) as i32);
    let capped = base.min(300.0);
    let jitter = rng.random_range(0.75..1.25);
    Duration::from_secs_f64((capped * jitter).clamp(0.5, 360.0))
}

// ---------------------------------------------------------------------------
// Emitter: status de-dup + convenience constructors
// ---------------------------------------------------------------------------

/// Wraps a [`SourceCtx`] and remembers the last reported [`SourceState`] so adapters
/// only emit `Status` on transitions.
#[cfg(feature = "native")]
pub struct Emitter {
    pub ctx: SourceCtx,
    last_state: Option<SourceState>,
    /// Set once the engine has gone away (channel closed).
    pub closed: bool,
}

#[cfg(feature = "native")]
impl Emitter {
    pub fn new(ctx: SourceCtx) -> Self {
        Self { ctx, last_state: None, closed: false }
    }

    pub fn now(&self) -> Seconds {
        self.ctx.now()
    }

    /// Emit a status change (no-op if the state is unchanged).
    pub async fn status(&mut self, state: SourceState, message: impl Into<String>) {
        if self.last_state == Some(state) {
            return;
        }
        self.last_state = Some(state);
        let msg = message.into();
        tracing::debug!(source = %self.ctx.id, ?state, %msg, "status");
        let s = Sample::status(&self.ctx.id, self.now(), state, msg);
        self.send(s).await;
    }

    /// Force a status line even if the state is unchanged (e.g. a changed message).
    pub async fn status_force(&mut self, state: SourceState, message: impl Into<String>) {
        self.last_state = None;
        self.status(state, message).await;
    }

    pub fn state(&self) -> Option<SourceState> {
        self.last_state
    }

    /// Emit a reading from `(name, value)` pairs (non-finite values dropped).
    pub async fn reading(&mut self, values: impl IntoIterator<Item = (impl Into<String>, f64)>) {
        let values: BTreeMap<String, f64> =
            values.into_iter().filter(|(_, v)| v.is_finite()).map(|(k, v)| (k.into(), v)).collect();
        if values.is_empty() {
            return;
        }
        let s = Sample { source: self.ctx.id.clone(), at: self.now(), datum: Datum::Reading { values } };
        self.send(s).await;
    }

    /// Emit an event with attributes.
    pub async fn event(&mut self, name: &str, intensity: f64, attrs: BTreeMap<String, Value>) {
        let s = Sample {
            source: self.ctx.id.clone(),
            at: self.now(),
            datum: Datum::Event { name: name.to_string(), intensity: sanitize(intensity), attrs },
        };
        self.send(s).await;
    }

    /// Emit a visualization payload.
    pub async fn viz(&mut self, payload: Json) {
        let s = Sample::viz(&self.ctx.id, self.now(), payload);
        self.send(s).await;
    }

    /// Emit a reading stamped with an explicit simulation time.
    pub async fn reading_at(&mut self, at: Seconds, values: impl IntoIterator<Item = (impl Into<String>, f64)>) {
        let values: BTreeMap<String, f64> =
            values.into_iter().filter(|(_, v)| v.is_finite()).map(|(k, v)| (k.into(), v)).collect();
        if values.is_empty() {
            return;
        }
        let s = Sample { source: self.ctx.id.clone(), at, datum: Datum::Reading { values } };
        self.send(s).await;
    }

    /// Emit an event stamped with an explicit simulation time.
    pub async fn event_at(&mut self, at: Seconds, name: &str, intensity: f64, attrs: BTreeMap<String, Value>) {
        let s = Sample {
            source: self.ctx.id.clone(),
            at,
            datum: Datum::Event { name: name.to_string(), intensity: sanitize(intensity), attrs },
        };
        self.send(s).await;
    }

    /// Emit a viz payload stamped with an explicit simulation time.
    pub async fn viz_at(&mut self, at: Seconds, payload: Json) {
        let s = Sample::viz(&self.ctx.id, at, payload);
        self.send(s).await;
    }

    /// Emit a raw sample.
    pub async fn send(&mut self, sample: Sample) {
        if self.closed {
            return;
        }
        if !self.ctx.emit(sample).await {
            self.closed = true;
        }
    }

    /// Emit an already-built datum.
    pub async fn datum(&mut self, datum: Datum) {
        let s = Sample { source: self.ctx.id.clone(), at: self.now(), datum };
        self.send(s).await;
    }

    pub fn is_shutdown(&self) -> bool {
        self.closed || self.ctx.is_shutdown()
    }

    /// Sleep for `d`, returning early (with `false`) if shutdown was requested.
    pub async fn sleep(&mut self, d: Duration) -> bool {
        if self.is_shutdown() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => true,
            _ = self.ctx.wait_shutdown() => false,
        }
    }
}

/// Replace NaN/inf with 0 and keep events sane.
pub fn sanitize(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

/// Clamp into 0..1 (NaN → 0).
pub fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Attribute builder
// ---------------------------------------------------------------------------

/// Small builder for event attribute maps.
#[derive(Default, Debug, Clone)]
pub struct Attrs(pub BTreeMap<String, Value>);

impl Attrs {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn num(mut self, k: &str, v: f64) -> Self {
        if v.is_finite() {
            self.0.insert(k.to_string(), Value::Num(v));
        }
        self
    }
    pub fn opt_num(self, k: &str, v: Option<f64>) -> Self {
        match v {
            Some(v) => self.num(k, v),
            None => self,
        }
    }
    pub fn text(mut self, k: &str, v: impl Into<String>) -> Self {
        self.0.insert(k.to_string(), Value::Text(v.into()));
        self
    }
    pub fn opt_text(self, k: &str, v: Option<&str>) -> Self {
        match v {
            Some(v) => self.text(k, v),
            None => self,
        }
    }
    pub fn bool(mut self, k: &str, v: bool) -> Self {
        self.0.insert(k.to_string(), Value::Bool(v));
        self
    }
    pub fn into_map(self) -> BTreeMap<String, Value> {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Sliding windows and de-dup
// ---------------------------------------------------------------------------

/// Timestamps of recent occurrences; answers "how many in the last N seconds".
#[derive(Debug, Default, Clone)]
pub struct RateWindow {
    times: VecDeque<Seconds>,
}

impl RateWindow {
    pub fn push(&mut self, at: Seconds) {
        self.times.push_back(at);
        if self.times.len() > 100_000 {
            self.times.pop_front();
        }
    }
    pub fn prune(&mut self, now: Seconds, window: f64) {
        while let Some(&t) = self.times.front() {
            if now - t > window {
                self.times.pop_front();
            } else {
                break;
            }
        }
    }
    pub fn count(&mut self, now: Seconds, window: f64) -> usize {
        self.prune(now, window);
        self.times.len()
    }
    /// Occurrences per second over `window`.
    pub fn rate(&mut self, now: Seconds, window: f64) -> f64 {
        if window <= 0.0 {
            return 0.0;
        }
        self.count(now, window) as f64 / window
    }
}

/// Remembers the last `cap` ids seen so re-polls don't re-emit old items.
#[derive(Debug, Clone)]
pub struct SeenSet<T: Hash + Eq + Clone> {
    set: HashSet<T>,
    order: VecDeque<T>,
    cap: usize,
}

impl<T: Hash + Eq + Clone> SeenSet<T> {
    pub fn new(cap: usize) -> Self {
        Self { set: HashSet::new(), order: VecDeque::new(), cap: cap.max(1) }
    }
    /// Returns `true` if the id was not seen before (and records it).
    pub fn insert(&mut self, id: T) -> bool {
        if self.set.contains(&id) {
            return false;
        }
        self.set.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
    pub fn contains(&self, id: &T) -> bool {
        self.set.contains(id)
    }
    pub fn len(&self) -> usize {
        self.set.len()
    }
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

/// Stable hash of any hashable value to `u64`.
pub fn hash_of<T: Hash>(v: &T) -> u64 {
    // FNV over the Hash stream (deterministic, unlike DefaultHasher's keys).
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 ^= u64::from(*b);
                self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    v.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// GET `url` and parse the body as JSON. Errors are strings suitable for a status line.
#[cfg(feature = "native")]
pub async fn get_json(ctx: &SourceCtx, url: &str) -> Result<Json, String> {
    get_json_with(ctx, url, &[]).await
}

/// GET with extra headers.
#[cfg(feature = "native")]
pub async fn get_json_with(ctx: &SourceCtx, url: &str, headers: &[(String, String)]) -> Result<Json, String> {
    let text = get_text_with(ctx, url, headers).await?;
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON from {url}: {e}"))
}

/// GET `url` as text.
#[cfg(feature = "native")]
pub async fn get_text(ctx: &SourceCtx, url: &str) -> Result<String, String> {
    get_text_with(ctx, url, &[]).await
}

/// GET `url` as text with extra headers.
#[cfg(feature = "native")]
pub async fn get_text_with(ctx: &SourceCtx, url: &str, headers: &[(String, String)]) -> Result<String, String> {
    let mut req = ctx
        .http
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
        .timeout(HTTP_TIMEOUT);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| format!("request failed: {}", short_err(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    resp.text().await.map_err(|e| format!("body read failed: {}", short_err(&e)))
}

/// Trim a reqwest error to a one-liner without the URL soup.
#[cfg(feature = "native")]
pub fn short_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        let s = e.to_string();
        s.split(": ").last().unwrap_or(&s).to_string()
    }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

/// Outcome of one poll iteration.
#[cfg(feature = "native")]
pub enum Poll {
    /// Fresh data arrived.
    Live,
    /// Request succeeded but nothing new (upstream quiet).
    Idle,
}

/// Runs `f` every `interval` until shutdown. Successful polls reset the backoff and
/// report `Live`/`Idle`; failures report `Reconnecting` and back off exponentially
/// (1 s → 5 min with jitter). The first call happens immediately.
#[cfg(feature = "native")]
pub async fn poll_loop<F, Fut>(em: &mut Emitter, interval: Duration, mut f: F)
where
    F: FnMut(SourceCtx) -> Fut,
    Fut: Future<Output = Result<Poll, String>>,
{
    let mut rng = rng_from_seed(instance_seed(em.ctx.seed, em.ctx.id.as_str(), None) ^ 0x9e37_79b9);
    let mut attempt: u32 = 0;
    em.status(SourceState::Connecting, "connecting").await;
    loop {
        if em.is_shutdown() {
            return;
        }
        let ctx = em.ctx.clone();
        let delay = match f(ctx).await {
            Ok(Poll::Live) => {
                attempt = 0;
                em.status(SourceState::Live, "live").await;
                interval
            }
            Ok(Poll::Idle) => {
                attempt = 0;
                em.status(SourceState::Idle, "no new data").await;
                interval
            }
            Err(msg) => {
                let d = retry_delay(attempt, &mut rng).max(interval.min(Duration::from_secs(5)));
                attempt = attempt.saturating_add(1);
                tracing::warn!(source = %em.ctx.id, attempt, %msg, "poll failed");
                em.status_force(SourceState::Reconnecting, format!("{msg}; retry in {:.0}s", d.as_secs_f64())).await;
                d
            }
        };
        if !em.sleep(delay).await {
            return;
        }
    }
}

/// Stateful counterpart of [`poll_loop`] for adapters that keep per-poll state:
///
/// ```ignore
/// let mut poller = Poller::new(interval);
/// poller.start(&mut em).await;
/// loop {
///     let res = do_poll(&mut em, &mut state).await;   // Result<Poll, String>
///     if !poller.after(&mut em, res).await { return; } // status + backoff + shutdown-aware sleep
/// }
/// ```
#[cfg(feature = "native")]
pub struct Poller {
    interval: Duration,
    attempt: u32,
    rng: rand_chacha::ChaCha8Rng,
}

#[cfg(feature = "native")]
impl Poller {
    pub fn new(interval: Duration) -> Self {
        Self { interval, attempt: 0, rng: rng_from_seed(0x51ed_270e) }
    }
    /// Emit the initial `Connecting` status.
    pub async fn start(&mut self, em: &mut Emitter) {
        self.rng = rng_from_seed(instance_seed(em.ctx.seed, em.ctx.id.as_str(), None) ^ 0x51ed_270e);
        em.status(SourceState::Connecting, "connecting").await;
    }
    /// Report the outcome of one poll and sleep until the next. Returns `false` on shutdown.
    pub async fn after(&mut self, em: &mut Emitter, res: Result<Poll, String>) -> bool {
        let delay = match res {
            Ok(Poll::Live) => {
                self.attempt = 0;
                em.status(SourceState::Live, "live").await;
                self.interval
            }
            Ok(Poll::Idle) => {
                self.attempt = 0;
                em.status(SourceState::Idle, "no new data").await;
                self.interval
            }
            Err(msg) => {
                let d = retry_delay(self.attempt, &mut self.rng).max(self.interval.min(Duration::from_secs(5)));
                self.attempt = self.attempt.saturating_add(1);
                tracing::warn!(source = %em.ctx.id, attempt = self.attempt, %msg, "poll failed");
                em.status_force(SourceState::Reconnecting, format!("{msg}; retry in {:.0}s", d.as_secs_f64())).await;
                d
            }
        };
        em.sleep(delay).await
    }
}

/// Like [`poll_loop`] but for long-lived connections (SSE, WebSocket): `f` runs until
/// the connection drops, then we reconnect with backoff. `f` returns `Ok(())` for a
/// clean end-of-stream, `Err(msg)` for a failure.
#[cfg(feature = "native")]
pub async fn connect_loop<F, Fut>(em: &mut Emitter, mut f: F)
where
    F: FnMut(SourceCtx) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut rng = rng_from_seed(instance_seed(em.ctx.seed, em.ctx.id.as_str(), None) ^ 0x7f4a_7c15);
    let mut attempt: u32 = 0;
    loop {
        if em.is_shutdown() {
            return;
        }
        em.status_force(SourceState::Connecting, "connecting").await;
        let started = std::time::Instant::now();
        let res = f(em.ctx.clone()).await;
        if em.is_shutdown() {
            return;
        }
        // A connection that lived a while counts as a success for backoff purposes.
        if started.elapsed() > Duration::from_secs(60) {
            attempt = 0;
        }
        let d = retry_delay(attempt, &mut rng);
        attempt = attempt.saturating_add(1);
        let msg = match res {
            Ok(()) => "stream ended".to_string(),
            Err(m) => m,
        };
        tracing::warn!(source = %em.ctx.id, attempt, %msg, "connection lost");
        em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
        if !em.sleep(d).await {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// JSON access helpers (tolerant)
// ---------------------------------------------------------------------------

/// Number from a JSON value (numbers, numeric strings, bools).
pub fn json_f64(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Number from a JSON value reference (numbers, numeric strings, bools; null → None).
pub fn json_f64_opt(v: &Json) -> Option<f64> {
    json_f64(Some(v))
}

/// `obj[key]` as f64.
pub fn jf(obj: &Json, key: &str) -> Option<f64> {
    json_f64(obj.get(key))
}

/// `obj[key]` as &str.
pub fn js<'a>(obj: &'a Json, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Json::as_str)
}

/// `obj[key]` as bool (accepts 0/1 and "true"/"false").
pub fn jb(obj: &Json, key: &str) -> Option<bool> {
    match obj.get(key)? {
        Json::Bool(b) => Some(*b),
        Json::Number(n) => Some(n.as_f64().unwrap_or(0.0) != 0.0),
        Json::String(s) => match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Current wall time as Unix seconds (f64).
pub fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut rng = rng_from_seed(1);
        let d0 = retry_delay(0, &mut rng);
        assert!(d0 >= Duration::from_millis(750) && d0 <= Duration::from_millis(1250));
        let d3 = retry_delay(3, &mut rng);
        assert!(d3 >= Duration::from_secs(6) && d3 <= Duration::from_secs(10));
        let big = retry_delay(40, &mut rng);
        assert!(big <= Duration::from_secs(375) && big >= Duration::from_secs(225));
    }

    #[test]
    fn seen_set_is_bounded() {
        let mut s = SeenSet::new(3);
        assert!(s.insert(1));
        assert!(!s.insert(1));
        assert!(s.insert(2));
        assert!(s.insert(3));
        assert!(s.insert(4));
        assert_eq!(s.len(), 3);
        assert!(!s.contains(&1));
        assert!(s.contains(&4));
    }

    #[test]
    fn rate_window() {
        let mut w = RateWindow::default();
        for i in 0..10 {
            w.push(i as f64);
        }
        assert_eq!(w.count(9.0, 5.0), 6);
        assert!((w.rate(9.0, 5.0) - 1.2).abs() < 1e-9);
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(hash_str("enwiki"), hash_str("enwiki"));
        assert_ne!(hash_str("a"), hash_str("b"));
        assert_eq!(hash_of(&"x"), hash_of(&"x"));
    }

    #[test]
    fn params_parse() {
        let spec = SourceSpec::new("t").with("lanes", 4).with("flow", "0.3").with("flag", "yes").with("seed", 7);
        assert_eq!(param_f64(&spec, "lanes", 1.0).unwrap(), 4.0);
        assert_eq!(param_f64(&spec, "flow", 1.0).unwrap(), 0.3);
        assert_eq!(param_f64(&spec, "missing", 2.0).unwrap(), 2.0);
        assert!(param_bool(&spec, "flag", false).unwrap());
        assert_eq!(param_seed(&spec).unwrap(), Some(7));
        assert!(param_f64_in(&spec, "lanes", 1.0, 0.0, 2.0).is_err());
    }
}
