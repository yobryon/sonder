//! `sse` — generic Server-Sent Events source; each `data:` JSON becomes a reading/event.

use std::time::Duration;

use futures::StreamExt;
use serde_json::Value as Json;
use sonder_core::{SourceSpec, SourceState};

use crate::adapters::generic::{Converter, EventsOn, GenericConfig};
use crate::adapters::http_json::parse_headers;
use crate::registry::{ParamInfo, SourceInfo};
use crate::sse::SseParser;
use crate::util::{self, Emitter, USER_AGENT};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Upper bound on waiting for the response headers.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

struct SseSource {
    url: String,
    headers: Vec<(String, String)>,
    cfg: GenericConfig,
    /// Only pass SSE events with this `event:` name (None = all).
    event_filter: Option<String>,
}

impl Source for SseSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut conv = Converter::new(self.cfg.clone());
            let mut last_id: Option<String> = None;
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, "connecting").await;
                let started = std::time::Instant::now();
                let res = self.stream_once(&mut em, &mut conv, &mut last_id).await;
                if em.is_shutdown() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = res.err().unwrap_or_else(|| "stream ended".into());
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

impl SseSource {
    async fn stream_once(&self, em: &mut Emitter, conv: &mut Converter, last_id: &mut Option<String>) -> Result<(), String> {
        let mut req = em
            .ctx
            .http
            .get(&self.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(id) = last_id.as_deref() {
            req = req.header("Last-Event-ID", id);
        }
        // Waiting for response headers must be both time-bounded and
        // interruptible: a server that accepts the TCP connection and never
        // answers would otherwise hang shutdown.
        let resp = tokio::select! {
            r = tokio::time::timeout(CONNECT_TIMEOUT, req.send()) => match r {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(util::short_err(&e)),
                Err(_) => return Err("connect timeout".into()),
            },
            _ = em.ctx.wait_shutdown() => return Ok(()),
        };
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        em.status(SourceState::Live, "connected").await;
        let mut body = resp.bytes_stream();
        let mut parser = SseParser::new();
        let mut last_data = std::time::Instant::now();
        loop {
            if em.is_shutdown() {
                return Ok(());
            }
            let chunk = tokio::select! {
                c = body.next() => c,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    if last_data.elapsed() > Duration::from_secs(300) {
                        return Err("no data for 5 min".into());
                    }
                    if last_data.elapsed() > Duration::from_secs(30) {
                        em.status(SourceState::Idle, "quiet").await;
                    }
                    continue;
                }
                _ = em.ctx.wait_shutdown() => return Ok(()),
            };
            match chunk {
                Some(Ok(bytes)) => {
                    last_data = std::time::Instant::now();
                    for ev in parser.push(&bytes) {
                        if let Some(id) = ev.id {
                            *last_id = Some(id);
                        }
                        if let Some(want) = &self.event_filter {
                            if ev.event.as_deref().unwrap_or("message") != want {
                                continue;
                            }
                        }
                        let Ok(v) = serde_json::from_str::<Json>(&ev.data) else { continue };
                        em.status(SourceState::Live, "live").await;
                        for d in conv.convert(&v) {
                            em.datum(d).await;
                        }
                    }
                }
                Some(Err(e)) => return Err(format!("stream error: {}", util::short_err(&e))),
                None => return Ok(()),
            }
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "sse",
        summary: "Generic Server-Sent Events source: each JSON `data:` payload becomes a reading/event",
        params: vec![
            ParamInfo { name: "url", summary: "Stream URL", default: None },
            ParamInfo { name: "fields", summary: "Paths or \"auto\"", default: Some("auto") },
            ParamInfo { name: "event_name", summary: "Name of the emitted event", default: Some("message") },
            ParamInfo { name: "events_on", summary: "change | every | none", default: Some("every") },
            ParamInfo { name: "data_path", summary: "Sub-path of each payload to use", default: None },
            ParamInfo { name: "filter", summary: "`path==value` — keep only matching payloads", default: None },
            ParamInfo { name: "sse_event", summary: "Only SSE frames with this `event:` name", default: None },
            ParamInfo { name: "headers", summary: "Extra request headers (map)", default: None },
        ],
        channels: vec!["<numeric fields>"],
        events: vec!["message"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let url = crate::require_str(spec, "url")?.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(util::invalid(spec, "url", "expected an http(s) URL"));
    }
    Ok(Box::new(SseSource {
        url,
        headers: parse_headers(spec)?,
        cfg: GenericConfig::from_spec(spec, EventsOn::Every, "message")?,
        event_filter: spec.get_str("sse_event").map(str::to_string),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_parses() {
        let spec = SourceSpec::new("sse").with("url", "https://stream.example/x").with("filter", "type==edit").with("sse_event", "message");
        assert!(factory(&spec).is_ok());
        assert!(factory(&SourceSpec::new("sse")).is_err());
    }

    /// A server that accepts the connection but never sends headers must not
    /// keep the source alive past shutdown.
    #[tokio::test]
    async fn shutdown_interrupts_hanging_connect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let mut socks = Vec::new();
            loop {
                if let Ok((s, _)) = listener.accept().await {
                    socks.push(s);
                }
            }
        });
        let spec = SourceSpec::new("sse").with("url", format!("http://{addr}/stream"));
        let src = factory(&spec).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: sonder_core::SourceId::new("s"),
            tx,
            clock: std::sync::Arc::new(sonder_core::WallClock::new()),
            http: reqwest::Client::new(),
            shutdown: srx,
            seed: 1,
        };
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(300)).await;
        stx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("source must stop on shutdown").unwrap();
        hold.abort();
    }
}
