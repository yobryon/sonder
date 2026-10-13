//! Live data source adapters.
//!
//! A [`Source`] is an async task that pushes [`Sample`]s into the engine. The
//! [`Registry`] maps a scene's `SourceSpec.kind` to a factory. Other crates
//! (e.g. `sonder-vision`) register additional kinds via [`Registry::register`].
//!
//! Builtin kinds live under [`adapters`]; shared plumbing (backoff, polling,
//! status de-dup, HTTP helpers) is in [`util`]; [`jsonpath`] and [`sse`] are the
//! small parsers the generic adapters use; [`probe`] implements `sonder probe`.
//!
//! # Testing
//!
//! Unit tests run offline against canned payloads. Tests that hit the real public
//! endpoints are `#[ignore]`d; run them with network access via
//! `cargo test -p sonder-sources -- --ignored` (or a single one, e.g.
//! `cargo test -p sonder-sources -- --ignored usgs`).

#![forbid(unsafe_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use sonder_core::{Clock, Sample, SourceId, SourceSpec};

pub mod adapters;
pub mod jsonpath;
pub mod probe;
pub mod registry;
pub mod sse;
pub mod util;

pub use registry::{Factory, ParamInfo, Registry, SourceInfo};
pub use util::{connect_loop, poll_loop, retry_delay, Emitter, Poll, Poller, USER_AGENT};

/// Everything a running source needs.
#[derive(Clone)]
pub struct SourceCtx {
    pub id: SourceId,
    pub tx: tokio::sync::mpsc::Sender<Sample>,
    pub clock: Arc<dyn Clock>,
    pub http: reqwest::Client,
    /// Becomes `true` when the engine is shutting down.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// Seed for synthetic sources (deterministic renders).
    pub seed: u64,
}

impl SourceCtx {
    /// Current engine time.
    pub fn now(&self) -> sonder_core::Seconds {
        self.clock.now()
    }
    /// Send a sample; returns false if the engine has gone away.
    pub async fn emit(&self, sample: Sample) -> bool {
        self.tx.send(sample).await.is_ok()
    }
    /// True once shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
    /// Resolves when shutdown is requested.
    pub async fn wait_shutdown(&mut self) {
        while !*self.shutdown.borrow() {
            if self.shutdown.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A boxed future with no output, `Send`.
pub type SourceFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A live data producer. `run` owns the task until shutdown.
pub trait Source: Send {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture;
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("unknown source kind `{0}`")]
    UnknownKind(String),
    #[error("source `{kind}`: missing required parameter `{param}`")]
    MissingParam { kind: String, param: &'static str },
    #[error("source `{kind}`: invalid parameter `{param}`: {message}")]
    InvalidParam { kind: String, param: &'static str, message: String },
    #[error("{0}")]
    Other(String),
}

/// Convenience for factories.
pub fn require_str<'a>(spec: &'a SourceSpec, param: &'static str) -> Result<&'a str, SourceError> {
    spec.get_str(param).ok_or_else(|| SourceError::MissingParam { kind: spec.kind.clone(), param })
}

/// A reqwest client configured the way sources like it (User-Agent, timeouts).
/// The engine may pass its own client in [`SourceCtx::http`]; adapters set the
/// User-Agent per request too, so any client works.
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// Registers all builtin kinds.
pub fn register_builtins(r: &mut Registry) {
    use adapters::*;
    macro_rules! reg {
        ($($m:ident),* $(,)?) => { $( r.register($m::info(), $m::factory); )* };
    }
    reg!(
        freeway, walk, system, astro, wikipedia, usgs, opensky, open_meteo, space_weather, coinbase, iss,
        hackernews, github, tides, http_json, sse, websocket, file_tail, stdin,
    );
    // `synthetic.random-walk` is the product-doc spelling of `synthetic.walk`.
    let mut alias = walk::info();
    alias.kind = "synthetic.random-walk";
    r.register(alias, walk::factory);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{Datum, VirtualClock};
    use std::time::Duration;

    const KINDS: &[&str] = &[
        "synthetic.freeway", "synthetic.walk", "system.metrics", "clock.astro", "wikipedia.recent-changes",
        "usgs.earthquakes", "opensky.flights", "open-meteo.weather", "noaa.space-weather", "coinbase.trades",
        "iss.position", "hackernews.firehose", "github.events", "noaa.tides", "http-json", "sse", "websocket",
        "file-tail", "stdin",
    ];

    #[test]
    fn registry_has_every_builtin() {
        let r = Registry::new();
        for k in KINDS {
            let info = r.info(k).unwrap_or_else(|| panic!("missing kind {k}"));
            assert_eq!(info.kind, *k);
            assert!(!info.summary.is_empty());
        }
        assert!(r.catalog().len() >= KINDS.len());
        // Kinds without required params construct from an empty spec.
        for k in KINDS {
            let needs_param = matches!(*k, "http-json" | "sse" | "websocket" | "file-tail");
            let res = r.create(&SourceSpec::new(*k));
            assert_eq!(res.is_ok(), !needs_param, "kind {k}: {:?}", res.err());
        }
        assert!(matches!(r.create(&SourceSpec::new("nope")), Err(SourceError::UnknownKind(_))));
    }

    fn ctx(clock: Arc<dyn Clock>) -> (SourceCtx, tokio::sync::mpsc::Receiver<Sample>, tokio::sync::watch::Sender<bool>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4096);
        let (stx, srx) = tokio::sync::watch::channel(false);
        (SourceCtx { id: SourceId::new("traffic"), tx, clock, http: default_http_client(), shutdown: srx, seed: 42 }, rx, stx)
    }

    #[tokio::test]
    async fn freeway_runs_under_virtual_clock() {
        let clock = Arc::new(VirtualClock::new());
        let (c, mut rx, stop) = ctx(clock.clone());
        let src = Registry::new().create(&SourceSpec::new("synthetic.freeway").with("flow", 0.8)).unwrap();
        let task = tokio::spawn(src.run(c));
        // Advance virtual time in 1 s jumps for 60 s of simulated traffic.
        for _ in 0..60 {
            clock.advance(1.0);
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let _ = stop.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        let (mut events, mut readings, mut viz, mut status) = (0, 0, 0, 0);
        let mut last_at = -1.0;
        while let Ok(s) = rx.try_recv() {
            assert!(s.at >= last_at - 1e-9, "samples must be time-ordered");
            last_at = s.at;
            match s.datum {
                Datum::Event { name, intensity, attrs } => {
                    assert_eq!(name, "car");
                    assert!((0.0..=1.0).contains(&intensity));
                    assert!(attrs.contains_key("lane"));
                    events += 1;
                }
                Datum::Reading { values } => {
                    assert!(values.contains_key("count"));
                    readings += 1;
                }
                Datum::Viz { payload } => {
                    assert!(payload["cars"].is_array());
                    viz += 1;
                }
                Datum::Status { .. } => status += 1,
            }
        }
        assert!(status >= 1);
        assert!(events > 10, "events {events}");
        assert!(readings >= 100, "readings {readings}");
        assert!(viz >= 500, "viz {viz}");
    }

    #[tokio::test]
    async fn walk_and_astro_emit_and_stop() {
        for spec in [
            SourceSpec::new("synthetic.walk").with("event_every", 0.2),
            SourceSpec::new("clock.astro").with("date", "2024-06-21T12:00:00").with("interval", 0.1),
        ] {
            let clock = Arc::new(sonder_core::WallClock::new());
            let (c, mut rx, stop) = ctx(clock);
            let src = Registry::new().create(&spec).unwrap();
            let task = tokio::spawn(src.run(c));
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = stop.send(true);
            tokio::time::timeout(Duration::from_secs(2), task).await.expect("source must stop on shutdown").unwrap();
            let mut readings = 0;
            while let Ok(s) = rx.try_recv() {
                if matches!(s.datum, Datum::Reading { .. }) {
                    readings += 1;
                }
            }
            assert!(readings >= 2, "{}: readings {readings}", spec.kind);
        }
    }
}
