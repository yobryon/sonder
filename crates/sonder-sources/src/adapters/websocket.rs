//! `websocket` — generic WebSocket source; each JSON text message becomes a reading/event.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value as Json;
use sonder_core::{SourceSpec, SourceState};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::adapters::generic::{Converter, EventsOn, GenericConfig};
use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Emitter, USER_AGENT};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

struct WsSource {
    url: String,
    subscribe: Option<Json>,
    cfg: GenericConfig,
    ping: Duration,
}

impl Source for WsSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut conv = Converter::new(self.cfg.clone());
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, "connecting").await;
                let started = std::time::Instant::now();
                let res = self.session(&mut em, &mut conv).await;
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

impl WsSource {
    async fn session(&self, em: &mut Emitter, conv: &mut Converter) -> Result<(), String> {
        let mut req = self.url.as_str().into_client_request().map_err(|e| format!("bad url: {e}"))?;
        if let Ok(v) = USER_AGENT.parse() {
            req.headers_mut().insert("User-Agent", v);
        }
        let (mut ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect failed: {e}"))?;
        if let Some(sub) = &self.subscribe {
            let txt = match sub {
                Json::String(s) => s.clone(),
                other => other.to_string(),
            };
            ws.send(Message::text(txt)).await.map_err(|e| format!("subscribe failed: {e}"))?;
        }
        em.status(SourceState::Live, "connected").await;
        let mut last_msg = std::time::Instant::now();
        let mut next_ping = std::time::Instant::now() + self.ping;
        loop {
            if em.is_shutdown() {
                let _ = ws.close(None).await;
                return Ok(());
            }
            let msg = tokio::select! {
                m = ws.next() => m,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if last_msg.elapsed() > Duration::from_secs(120) {
                        return Err("no messages for 2 min".into());
                    }
                    if std::time::Instant::now() >= next_ping {
                        next_ping = std::time::Instant::now() + self.ping;
                        ws.send(Message::Ping(Vec::new().into())).await.map_err(|e| format!("ping failed: {e}"))?;
                    }
                    continue;
                }
            };
            match msg {
                Some(Ok(Message::Text(txt))) => {
                    last_msg = std::time::Instant::now();
                    if let Ok(v) = serde_json::from_str::<Json>(txt.as_str()) {
                        em.status(SourceState::Live, "live").await;
                        for d in conv.convert(&v) {
                            em.datum(d).await;
                        }
                    }
                }
                Some(Ok(Message::Binary(b))) => {
                    last_msg = std::time::Instant::now();
                    if let Ok(v) = serde_json::from_slice::<Json>(&b) {
                        for d in conv.convert(&v) {
                            em.datum(d).await;
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Pong(_))) => last_msg = std::time::Instant::now(),
                Some(Ok(Message::Close(_))) => return Err("server closed".into()),
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(e)) => return Err(format!("ws error: {e}")),
                None => return Ok(()),
            }
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "websocket",
        summary: "Generic WebSocket source: each JSON message becomes a reading/event",
        params: vec![
            ParamInfo { name: "url", summary: "ws:// or wss:// URL", default: None },
            ParamInfo { name: "subscribe", summary: "JSON (or string) to send once connected", default: None },
            ParamInfo { name: "fields", summary: "Paths or \"auto\"", default: Some("auto") },
            ParamInfo { name: "event_name", summary: "Name of the emitted event", default: Some("message") },
            ParamInfo { name: "events_on", summary: "change | every | none", default: Some("every") },
            ParamInfo { name: "data_path", summary: "Sub-path of each message to use", default: None },
            ParamInfo { name: "filter", summary: "`path==value` — keep only matching messages", default: None },
            ParamInfo { name: "ping", summary: "Seconds between pings", default: Some("30") },
        ],
        channels: vec!["<numeric fields>"],
        events: vec!["message"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let url = crate::require_str(spec, "url")?.trim().to_string();
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        return Err(util::invalid(spec, "url", "expected a ws(s) URL"));
    }
    Ok(Box::new(WsSource {
        url,
        subscribe: spec.params.get("subscribe").filter(|v| !v.is_null()).cloned(),
        cfg: GenericConfig::from_spec(spec, EventsOn::Every, "message")?,
        ping: util::param_secs(spec, "ping", 30.0, 5.0)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn factory_parses() {
        let spec = SourceSpec::new("websocket").with("url", "wss://x/y").with("subscribe", json!({"type": "subscribe"}));
        assert!(factory(&spec).is_ok());
        assert!(factory(&SourceSpec::new("websocket").with("url", "https://x")).is_err());
    }
}
