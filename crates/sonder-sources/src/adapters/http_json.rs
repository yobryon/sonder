//! `http-json` — poll any JSON endpoint; numeric leaves become readings.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::adapters::generic::{Converter, EventsOn, GenericConfig};
use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Parse a `headers` map param into `(name, value)` pairs.
pub fn parse_headers(spec: &SourceSpec) -> Result<Vec<(String, String)>, SourceError> {
    match spec.params.get("headers") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                let s = match v {
                    Json::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if k.trim().is_empty() {
                    Err(util::invalid(spec, "headers", "empty header name"))
                } else {
                    Ok((k.clone(), s))
                }
            })
            .collect(),
        Some(_) => Err(util::invalid(spec, "headers", "expected a map of header → value")),
    }
}

struct HttpJsonSource {
    url: String,
    interval: Duration,
    headers: Vec<(String, String)>,
    cfg: GenericConfig,
}

impl Source for HttpJsonSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut conv = Converter::new(self.cfg);
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json_with(&em.ctx, &self.url, &self.headers).await?;
                    let data = conv.convert(&v);
                    let live = data.iter().any(|d| matches!(d, sonder_core::Datum::Event { .. }));
                    if data.is_empty() {
                        return Err("no numeric fields in response (check `fields`/`data_path`)".to_string());
                    }
                    for d in data {
                        em.datum(d).await;
                    }
                    Ok(if live || conv.cfg.events_on == EventsOn::None { Poll::Live } else { Poll::Idle })
                }
                .await;
                if !poller.after(&mut em, res).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "http-json",
        summary: "Poll any JSON URL; numeric leaves become readings, changes become events",
        params: vec![
            ParamInfo { name: "url", summary: "Endpoint URL", default: None },
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("10") },
            ParamInfo { name: "fields", summary: "Paths like \".a.b[0]\" or \"auto\" for all numeric leaves", default: Some("auto") },
            ParamInfo { name: "events_on", summary: "change | every | none", default: Some("change") },
            ParamInfo { name: "event_name", summary: "Name of the emitted event", default: Some("update") },
            ParamInfo { name: "data_path", summary: "Sub-path of the response to treat as the object", default: None },
            ParamInfo { name: "headers", summary: "Extra request headers (map)", default: None },
        ],
        channels: vec!["<numeric fields>"],
        events: vec!["update"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let url = crate::require_str(spec, "url")?.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(util::invalid(spec, "url", "expected an http(s) URL"));
    }
    Ok(Box::new(HttpJsonSource {
        url,
        interval: util::param_secs(spec, "interval", 10.0, 1.0)?,
        headers: parse_headers(spec)?,
        cfg: GenericConfig::from_spec(spec, EventsOn::Change, "update")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn factory_requires_url_and_parses_headers() {
        assert!(factory(&SourceSpec::new("http-json")).is_err());
        assert!(factory(&SourceSpec::new("http-json").with("url", "ftp://x")).is_err());
        let spec = SourceSpec::new("http-json").with("url", "https://x/y").with("headers", json!({"X-Token": "abc", "N": 1}));
        let h = parse_headers(&spec).unwrap();
        assert_eq!(h.len(), 2);
        assert!(factory(&spec).is_ok());
    }
}
