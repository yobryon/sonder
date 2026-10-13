//! `github.events` — the public GitHub events timeline (unauthenticated: 60 req/h).

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, js, Attrs, Emitter, Poll, RateWindow, SeenSet};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const URL: &str = "https://api.github.com/events?per_page=100";

#[derive(Debug, Clone, PartialEq)]
pub struct GhEvent {
    pub id: String,
    pub ty: String,
    pub repo: String,
    pub actor: String,
    pub action: String,
}

pub fn intensity_for(ty: &str) -> f64 {
    match ty {
        "PushEvent" => 0.4,
        "PullRequestEvent" => 0.7,
        "PullRequestReviewEvent" | "PullRequestReviewCommentEvent" => 0.5,
        "CreateEvent" => 0.5,
        "DeleteEvent" => 0.45,
        "WatchEvent" => 0.3,
        "ForkEvent" => 0.45,
        "IssuesEvent" => 0.6,
        "IssueCommentEvent" => 0.35,
        "ReleaseEvent" => 0.9,
        "PublicEvent" => 0.8,
        "MemberEvent" => 0.5,
        "GollumEvent" => 0.3,
        "CommitCommentEvent" => 0.35,
        _ => 0.4,
    }
}

impl GhEvent {
    pub fn intensity(&self) -> f64 {
        intensity_for(&self.ty)
    }
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .text("type", self.ty.clone())
            .text("repo", self.repo.clone())
            .text("actor", self.actor.clone())
            .text("action", self.action.clone())
            .text("id", self.id.clone())
    }
}

/// Parse the events array (newest first).
pub fn parse_events(v: &Json) -> Vec<GhEvent> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    arr.iter()
        .filter_map(|e| {
            Some(GhEvent {
                id: js(e, "id")?.to_string(),
                ty: js(e, "type").unwrap_or("").to_string(),
                repo: e.get("repo").and_then(|r| js(r, "name")).unwrap_or("").to_string(),
                actor: e.get("actor").and_then(|a| js(a, "login")).unwrap_or("").to_string(),
                action: e.get("payload").and_then(|p| js(p, "action")).unwrap_or("").to_string(),
            })
        })
        .collect()
}

struct GithubSource {
    interval: Duration,
    initial: usize,
}

impl Source for GithubSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut seen: SeenSet<String> = SeenSet::new(10_000);
            let mut first = true;
            let mut window = RateWindow::default();
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let headers = [("Accept".to_string(), "application/vnd.github+json".to_string())];
                    let v = util::get_json_with(&em.ctx, URL, &headers).await?;
                    if let Some(msg) = v.get("message").and_then(Json::as_str) {
                        return Err(format!("GitHub: {msg}"));
                    }
                    let mut events = parse_events(&v);
                    events.reverse(); // oldest first
                    let mut fresh: Vec<GhEvent> = events.into_iter().filter(|e| seen.insert(e.id.clone())).collect();
                    if first {
                        let keep = self.initial.min(fresh.len());
                        fresh = fresh.split_off(fresh.len() - keep);
                        first = false;
                    }
                    let now = em.now();
                    let n = fresh.len();
                    for e in &fresh {
                        window.push(now);
                        em.event("event", e.intensity(), e.attrs().into_map()).await;
                    }
                    em.reading([("rate", window.rate(now, 300.0)), ("new_events", n as f64)]).await;
                    Ok(if n > 0 { Poll::Live } else { Poll::Idle })
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
        kind: "github.events",
        summary: "Public GitHub events timeline (push, PR, star…) as `event` events",
        params: vec![
            ParamInfo { name: "interval", summary: "Poll interval (s); unauthenticated limit is 60/h", default: Some("60") },
            ParamInfo { name: "initial", summary: "How many events to emit from the first poll", default: Some("10") },
        ],
        channels: vec!["rate", "new_events"],
        events: vec!["event"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(GithubSource {
        interval: util::param_secs(spec, "interval", 60.0, 60.0)?,
        initial: util::param_f64_in(spec, "initial", 10.0, 0.0, 100.0)? as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_events() {
        let v = json!([
            {"id": "3", "type": "PushEvent", "actor": {"login": "octocat"}, "repo": {"name": "octo/hello"}, "payload": {"size": 1}},
            {"id": "2", "type": "PullRequestEvent", "actor": {"login": "a"}, "repo": {"name": "x/y"}, "payload": {"action": "opened"}},
            {"type": "NoId"}
        ]);
        let e = parse_events(&v);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].intensity(), 0.4);
        assert_eq!(e[1].action, "opened");
        assert_eq!(e[1].intensity(), 0.7);
        assert_eq!(e[0].attrs().into_map()["repo"].as_str(), Some("octo/hello"));
    }
}
