//! `hackernews.firehose` — every new HN item (stories and comments) as events.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, jf, js, Attrs, Emitter, Poll, RateWindow};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const MAXITEM_URL: &str = "https://hacker-news.firebaseio.com/v0/maxitem.json";

pub fn item_url(id: u64) -> String {
    format!("https://hacker-news.firebaseio.com/v0/item/{id}.json")
}

/// A parsed HN item.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: u64,
    pub ty: String,
    pub by: String,
    pub score: f64,
    pub title: String,
    pub text_len: f64,
    pub descendants: f64,
}

pub fn parse_item(v: &Json) -> Option<Item> {
    let id = v.get("id")?.as_u64()?;
    let ty = js(v, "type").unwrap_or("unknown").to_string();
    if v.get("deleted").and_then(Json::as_bool) == Some(true) || v.get("dead").and_then(Json::as_bool) == Some(true) {
        return None;
    }
    Some(Item {
        id,
        ty,
        by: js(v, "by").unwrap_or("").to_string(),
        score: jf(v, "score").unwrap_or(0.0),
        title: js(v, "title").unwrap_or("").chars().take(200).collect(),
        text_len: js(v, "text").map(|t| t.len() as f64).unwrap_or(0.0),
        descendants: jf(v, "descendants").unwrap_or(0.0),
    })
}

impl Item {
    pub fn intensity(&self) -> f64 {
        match self.ty.as_str() {
            "story" => 0.8,
            "job" => 0.6,
            "poll" => 0.7,
            "comment" => 0.3,
            _ => 0.4,
        }
    }
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .num("id", self.id as f64)
            .text("type", self.ty.clone())
            .num("score", self.score)
            .text("by", self.by.clone())
            .text("title", self.title.clone())
            .num("text_len", self.text_len)
            .num("descendants", self.descendants)
    }
}

struct HnSource {
    interval: Duration,
    cap: usize,
}

impl Source for HnSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut last_max: Option<u64> = None;
            let mut window = RateWindow::default();
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json(&em.ctx, MAXITEM_URL).await?;
                    let max = v.as_u64().ok_or_else(|| "maxitem is not an integer".to_string())?;
                    let start = match last_max {
                        None => max.saturating_sub(3), // a few recent items so we aren't silent at start
                        Some(m) => m + 1,
                    };
                    let mut ids: Vec<u64> = (start..=max).collect();
                    let skipped = ids.len().saturating_sub(self.cap);
                    if skipped > 0 {
                        ids = ids.split_off(skipped); // keep the newest `cap`
                    }
                    let mut fetched = 0;
                    for id in &ids {
                        if em.is_shutdown() {
                            break;
                        }
                        match util::get_json(&em.ctx, &item_url(*id)).await {
                            Ok(iv) => {
                                if let Some(item) = parse_item(&iv) {
                                    fetched += 1;
                                    let now = em.now();
                                    window.push(now);
                                    em.event("item", item.intensity(), item.attrs().into_map()).await;
                                }
                            }
                            Err(e) => tracing::debug!(source = %em.ctx.id, id, "item fetch failed: {e}"),
                        }
                    }
                    last_max = Some(max);
                    let now = em.now();
                    let n = ids.len() as f64;
                    em.reading([("rate", window.rate(now, 60.0)), ("new_items", n), ("skipped", skipped as f64), ("max_id", max as f64)]).await;
                    Ok(if fetched > 0 { Poll::Live } else { Poll::Idle })
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
        kind: "hackernews.firehose",
        summary: "Every new Hacker News item (story/comment) as an `item` event",
        params: vec![
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("20") },
            ParamInfo { name: "max_per_poll", summary: "Max items fetched per poll", default: Some("30") },
        ],
        channels: vec!["rate", "new_items", "skipped", "max_id"],
        events: vec!["item"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(HnSource {
        interval: util::param_secs(spec, "interval", 20.0, 5.0)?,
        cap: util::param_f64_in(spec, "max_per_poll", 30.0, 1.0, 200.0)? as usize,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_items() {
        let story = json!({"by": "pg", "descendants": 71, "id": 8863, "kids": [8952], "score": 111, "time": 1175714200, "title": "My YC app: Dropbox", "type": "story", "url": "http://x"});
        let s = parse_item(&story).unwrap();
        assert_eq!(s.intensity(), 0.8);
        assert_eq!(s.attrs().into_map()["title"].as_str(), Some("My YC app: Dropbox"));
        let comment = json!({"by": "norvig", "id": 2921983, "parent": 2921506, "text": "Aw shucks, guys ... <p>Hello", "time": 1314211127, "type": "comment"});
        let c = parse_item(&comment).unwrap();
        assert_eq!(c.intensity(), 0.3);
        assert!(c.text_len > 10.0);
        assert!(parse_item(&json!({"id": 1, "deleted": true, "type": "comment"})).is_none());
        assert!(parse_item(&Json::Null).is_none());
    }
}
