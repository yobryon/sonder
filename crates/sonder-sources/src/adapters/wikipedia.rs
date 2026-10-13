//! `wikipedia.recent-changes` — the Wikimedia EventStreams SSE firehose.

use std::collections::HashSet;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value as Json;
use sonder_core::{SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::sse::SseParser;
use crate::util::{self, clamp01, jb, jf, js, Attrs, Emitter, RateWindow, USER_AGENT};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const URL: &str = "https://stream.wikimedia.org/v2/stream/recentchange";

#[derive(Debug, Clone)]
pub struct WikiConfig {
    /// Empty = all wikis.
    pub wikis: HashSet<String>,
    pub include_bots: bool,
    pub types: HashSet<String>,
}

impl WikiConfig {
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, SourceError> {
        let wikis = util::param_str_list(spec, "wikis").unwrap_or_default().into_iter().collect();
        let include_bots = util::param_bool(spec, "include_bots", false)?;
        let types: HashSet<String> = util::param_str_list(spec, "types")
            .unwrap_or_else(|| vec!["edit".into(), "new".into()])
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        for t in &types {
            if !["edit", "new", "log", "categorize"].contains(&t.as_str()) {
                return Err(util::invalid(spec, "types", &format!("unknown type `{t}` (edit|new|log|categorize)")));
            }
        }
        Ok(Self { wikis, include_bots, types })
    }

    /// Does this recentchange record pass the filters?
    pub fn accepts(&self, rc: &Json) -> bool {
        let ty = js(rc, "type").unwrap_or("");
        if !self.types.contains(ty) {
            return false;
        }
        if !self.include_bots && jb(rc, "bot").unwrap_or(false) {
            return false;
        }
        if !self.wikis.is_empty() {
            match js(rc, "wiki") {
                Some(w) if self.wikis.contains(w) => {}
                _ => return false,
            }
        }
        true
    }
}

/// A parsed edit, ready to become an event.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub wiki: String,
    pub bytes: f64,
    pub bot: bool,
    pub user: String,
    pub title: String,
    pub ty: String,
    pub minor: bool,
    pub namespace: f64,
    pub intensity: f64,
}

/// Parse one recentchange JSON record.
pub fn parse_edit(rc: &Json) -> Option<Edit> {
    let ty = js(rc, "type")?.to_string();
    let old = rc.get("length").and_then(|l| jf(l, "old")).unwrap_or(0.0);
    let new = rc.get("length").and_then(|l| jf(l, "new")).unwrap_or(0.0);
    let bytes = new - old;
    Some(Edit {
        wiki: js(rc, "wiki").unwrap_or("").to_string(),
        bytes,
        bot: jb(rc, "bot").unwrap_or(false),
        user: js(rc, "user").unwrap_or("").to_string(),
        title: js(rc, "title").unwrap_or("").chars().take(200).collect(),
        ty,
        minor: jb(rc, "minor").unwrap_or(false),
        namespace: jf(rc, "namespace").unwrap_or(0.0),
        intensity: clamp01(bytes.abs() / 2000.0),
    })
}

impl Edit {
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .text("wiki", self.wiki.clone())
            .num("bytes", self.bytes)
            .bool("bot", self.bot)
            .text("user", self.user.clone())
            .text("title", self.title.clone())
            .text("type", self.ty.clone())
            .bool("minor", self.minor)
            .num("namespace", self.namespace)
    }
}

struct WikiSource {
    cfg: WikiConfig,
}

impl Source for WikiSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let cfg = self.cfg;
            let mut em = Emitter::new(ctx);
            // Shared across reconnects: rate window + net bytes.
            let mut window = RateWindow::default();
            let mut net_bytes = 0.0_f64;
            let mut last_id: Option<String> = None;
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, "connecting").await;
                let started = std::time::Instant::now();
                let res = stream_once(&mut em, &cfg, &mut window, &mut net_bytes, &mut last_id).await;
                if em.is_shutdown() {
                    return;
                }
                let msg = match res {
                    Ok(()) => "stream ended".to_string(),
                    Err(m) => m,
                };
                // A connection that lived a while counts as a success for backoff.
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

async fn stream_once(
    em: &mut Emitter,
    cfg: &WikiConfig,
    window: &mut RateWindow,
    net_bytes: &mut f64,
    last_id: &mut Option<String>,
) -> Result<(), String> {
    let mut req = em
        .ctx
        .http
        .get(URL)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(id) = last_id.as_deref() {
        req = req.header("Last-Event-ID", id);
    }
    // Header wait is time-bounded and interruptible by shutdown.
    let resp = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(20), req.send()) => match r {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => return Err(util::short_err(&e)),
            Err(_) => return Err("connect timeout".into()),
        },
        _ = em.ctx.wait_shutdown() => return Ok(()),
    };
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut body = resp.bytes_stream();
    let mut parser = SseParser::new();
    let mut next_reading = em.now() + 2.0;
    let mut last_data = std::time::Instant::now();
    loop {
        if em.is_shutdown() {
            return Ok(());
        }
        let mut timed_out = false;
        let chunk = tokio::select! {
            c = body.next() => c,
            _ = tokio::time::sleep(Duration::from_secs(2)) => { timed_out = true; None }
            _ = em.ctx.wait_shutdown() => return Ok(()),
        };
        match chunk {
            Some(Ok(bytes)) => {
                last_data = std::time::Instant::now();
                for ev in parser.push(&bytes) {
                    if let Some(id) = ev.id {
                        *last_id = Some(id);
                    }
                    let Ok(rc) = serde_json::from_str::<Json>(&ev.data) else { continue };
                    if !cfg.accepts(&rc) {
                        continue;
                    }
                    if let Some(edit) = parse_edit(&rc) {
                        let now = em.now();
                        window.push(now);
                        *net_bytes += edit.bytes;
                        em.status(SourceState::Live, "live").await;
                        em.event("edit", edit.intensity, edit.attrs().into_map()).await;
                    }
                }
            }
            Some(Err(e)) => return Err(format!("stream error: {}", util::short_err(&e))),
            None if !timed_out => return Ok(()),
            None => {
                if last_data.elapsed() > Duration::from_secs(90) {
                    return Err("no data for 90 s".into());
                }
            }
        }
        let now = em.now();
        if now >= next_reading {
            next_reading = now + 2.0;
            let rate = window.rate(now, 10.0);
            em.reading([("rate", rate), ("net_bytes", *net_bytes)]).await;
            if rate == 0.0 && last_data.elapsed() > Duration::from_secs(20) {
                em.status(SourceState::Idle, "quiet").await;
            }
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "wikipedia.recent-changes",
        summary: "Wikimedia recent-changes firehose (SSE): every edit on every wiki",
        params: vec![
            ParamInfo { name: "wikis", summary: "Wiki ids to keep, e.g. [\"enwiki\"] (default: all)", default: None },
            ParamInfo { name: "include_bots", summary: "Keep bot edits", default: Some("false") },
            ParamInfo { name: "types", summary: "Change types: edit|new|log|categorize", default: Some("[\"edit\", \"new\"]") },
        ],
        channels: vec!["rate", "net_bytes"],
        events: vec!["edit"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(WikiSource { cfg: WikiConfig::from_spec(spec)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rc() -> Json {
        json!({
            "$schema": "/mediawiki/recentchange/1.0.0", "type": "edit", "namespace": 0,
            "title": "Rust (programming language)", "user": "Ferris", "bot": false, "minor": true,
            "length": {"old": 1000, "new": 1500}, "wiki": "enwiki", "server_name": "en.wikipedia.org"
        })
    }

    #[test]
    fn parses_and_filters() {
        let e = parse_edit(&rc()).unwrap();
        assert_eq!(e.bytes, 500.0);
        assert!((e.intensity - 0.25).abs() < 1e-9);
        assert_eq!(e.wiki, "enwiki");
        assert!(e.minor);
        let a = e.attrs().into_map();
        assert_eq!(a["user"].as_str(), Some("Ferris"));
        assert_eq!(a["bytes"].as_f64(), Some(500.0));

        let spec = SourceSpec::new("wikipedia.recent-changes").with("wikis", json!(["dewiki"]));
        let cfg = WikiConfig::from_spec(&spec).unwrap();
        assert!(!cfg.accepts(&rc()));
        let spec = SourceSpec::new("wikipedia.recent-changes").with("wikis", "enwiki");
        let cfg = WikiConfig::from_spec(&spec).unwrap();
        assert!(cfg.accepts(&rc()));
        let mut bot = rc();
        bot["bot"] = json!(true);
        assert!(!cfg.accepts(&bot));
        let mut log = rc();
        log["type"] = json!("log");
        assert!(!cfg.accepts(&log));
        assert!(WikiConfig::from_spec(&SourceSpec::new("w").with("types", "bogus")).is_err());
    }

    #[test]
    fn sse_frame_to_edit() {
        let frame = format!("event: message\nid: [{{\"topic\":\"x\"}}]\ndata: {}\n\n", rc());
        let mut p = SseParser::new();
        let evs = p.push(frame.as_bytes());
        let v: Json = serde_json::from_str(&evs[0].data).unwrap();
        assert!(parse_edit(&v).is_some());
    }

    /// Run with `cargo test -p sonder-sources -- --ignored wikipedia` (needs network).
    #[tokio::test]
    #[ignore]
    async fn live_stream_parses() {
        let client = reqwest::Client::new();
        let resp = client.get(URL).header(reqwest::header::USER_AGENT, USER_AGENT).send().await.unwrap();
        assert!(resp.status().is_success());
        let mut body = resp.bytes_stream();
        let mut p = SseParser::new();
        let mut edits = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline && edits < 5 {
            let Some(Ok(b)) = body.next().await else { break };
            for ev in p.push(&b) {
                if let Ok(v) = serde_json::from_str::<Json>(&ev.data) {
                    if parse_edit(&v).is_some() {
                        edits += 1;
                    }
                }
            }
        }
        assert!(edits >= 5, "got {edits} edits");
    }
}
