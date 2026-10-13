//! `usgs.earthquakes` — USGS GeoJSON summary feeds, polled.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, jf, js, Attrs, Emitter, Poll, SeenSet};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const FEEDS: &[&str] = &[
    "all_hour", "all_day", "all_week", "1.0_hour", "1.0_day", "1.0_week", "2.5_hour", "2.5_day", "2.5_week",
    "4.5_hour", "4.5_day", "4.5_week", "significant_hour", "significant_day", "significant_week",
];

pub fn feed_url(feed: &str) -> String {
    format!("https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/{feed}.geojson")
}

/// One earthquake from the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Quake {
    pub id: String,
    pub mag: f64,
    pub depth_km: f64,
    pub lat: f64,
    pub lon: f64,
    pub place: String,
    /// Milliseconds since epoch.
    pub time_ms: f64,
}

impl Quake {
    pub fn intensity(&self) -> f64 {
        clamp01((self.mag - 1.0) / 7.0)
    }
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .num("mag", self.mag)
            .num("depth_km", self.depth_km)
            .num("lat", self.lat)
            .num("lon", self.lon)
            .text("place", self.place.clone())
            .num("lon01", clamp01((self.lon + 180.0) / 360.0))
            .num("lat01", clamp01((self.lat + 90.0) / 180.0))
            .text("id", self.id.clone())
    }
}

/// Parse a GeoJSON FeatureCollection into quakes, newest first (as the feed orders them).
pub fn parse_feed(v: &Json) -> Vec<Quake> {
    let Some(features) = v.get("features").and_then(Json::as_array) else { return Vec::new() };
    features
        .iter()
        .filter_map(|f| {
            let id = js(f, "id")?.to_string();
            let props = f.get("properties")?;
            let coords = f.get("geometry")?.get("coordinates")?.as_array()?;
            let lon = coords.first().and_then(Json::as_f64)?;
            let lat = coords.get(1).and_then(Json::as_f64)?;
            let depth_km = coords.get(2).and_then(Json::as_f64).unwrap_or(0.0);
            Some(Quake {
                id,
                mag: jf(props, "mag").unwrap_or(0.0),
                depth_km,
                lat,
                lon,
                place: js(props, "place").unwrap_or("").to_string(),
                time_ms: jf(props, "time").unwrap_or(0.0),
            })
        })
        .collect()
}

struct UsgsSource {
    feed: String,
    min_mag: f64,
    interval: Duration,
}

impl Source for UsgsSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let url = feed_url(&self.feed);
            let mut em = Emitter::new(ctx);
            let mut seen: SeenSet<String> = SeenSet::new(5000);
            let mut first = true;
            let min_mag = self.min_mag;
            // Shared mutable state must be moved into the closure via a RefCell-free
            // pattern: we collect output per poll and emit from the loop body.
            let tx = em.ctx.tx.clone();
            let id = em.ctx.id.clone();
            let clock = em.ctx.clock.clone();
            let seen_cell = std::rc::Rc::new(std::cell::RefCell::new((seen, first)));
            let _ = (&mut seen, &mut first);
            util::poll_loop(&mut em, self.interval, |ctx| {
                let url = url.clone();
                let tx = tx.clone();
                let id = id.clone();
                let clock = clock.clone();
                let seen_cell = seen_cell.clone();
                async move {
                    let v = util::get_json(&ctx, &url).await?;
                    let quakes = parse_feed(&v);
                    if quakes.is_empty() && v.get("features").is_none() {
                        return Err("unexpected payload (no features)".into());
                    }
                    let mut state = seen_cell.borrow_mut();
                    let (seen, first) = &mut *state;
                    let hour_ago = util::unix_now() * 1000.0 - 3_600_000.0;
                    let count_hour = quakes.iter().filter(|q| q.time_ms >= hour_ago).count() as f64;
                    let max_mag = quakes.iter().map(|q| q.mag).fold(0.0, f64::max);
                    let mut fresh: Vec<&Quake> = Vec::new();
                    for q in &quakes {
                        let is_new = seen.insert(q.id.clone());
                        if is_new && q.mag >= min_mag {
                            fresh.push(q);
                        }
                    }
                    if *first {
                        // Feed is newest-first; keep only the 3 most recent to seed the scene.
                        fresh.truncate(3);
                        fresh.reverse();
                        *first = false;
                    } else {
                        fresh.reverse(); // oldest first
                    }
                    let now = clock.now();
                    let _ = tx
                        .send(sonder_core::Sample::reading(&id, now, [("count_hour", count_hour), ("max_mag", max_mag), ("count_feed", quakes.len() as f64)]))
                        .await;
                    let had_fresh = !fresh.is_empty();
                    for q in fresh {
                        let s = sonder_core::Sample {
                            source: id.clone(),
                            at: now,
                            datum: sonder_core::Datum::Event { name: "quake".into(), intensity: q.intensity(), attrs: q.attrs().into_map() },
                        };
                        let _ = tx.send(s).await;
                    }
                    Ok(if had_fresh { Poll::Live } else { Poll::Idle })
                }
            })
            .await;
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "usgs.earthquakes",
        summary: "USGS earthquake GeoJSON feed; a `quake` event per new earthquake",
        params: vec![
            ParamInfo { name: "feed", summary: "Feed name: all_hour|all_day|2.5_day|4.5_week|significant_day|…", default: Some("all_hour") },
            ParamInfo { name: "min_magnitude", summary: "Drop quakes below this magnitude", default: Some("0") },
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("60") },
        ],
        channels: vec!["count_hour", "max_mag", "count_feed"],
        events: vec!["quake"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let feed = util::param_str(spec, "feed", "all_hour");
    if !FEEDS.contains(&feed.as_str()) {
        return Err(util::invalid(spec, "feed", &format!("unknown feed `{feed}`; one of {}", FEEDS.join(", "))));
    }
    Ok(Box::new(UsgsSource {
        feed,
        min_mag: util::param_f64(spec, "min_magnitude", 0.0)?,
        interval: util::param_secs(spec, "interval", 60.0, 15.0)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn fixture() -> Json {
        json!({
            "type": "FeatureCollection",
            "metadata": {"generated": 1700000000000u64, "count": 2},
            "features": [
                {"type": "Feature", "id": "nc73000001",
                 "properties": {"mag": 2.7, "place": "10km NE of Somewhere, CA", "time": 1700000000000u64, "type": "earthquake"},
                 "geometry": {"type": "Point", "coordinates": [-122.1, 37.9, 8.2]}},
                {"type": "Feature", "id": "us7000abcd",
                 "properties": {"mag": 5.1, "place": "Kermadec Islands", "time": 1699999000000u64},
                 "geometry": {"type": "Point", "coordinates": [-177.5, -30.2, 35.0]}},
                {"type": "Feature", "id": "broken", "properties": {}, "geometry": null}
            ]
        })
    }

    #[test]
    fn parses_geojson() {
        let q = parse_feed(&fixture());
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].id, "nc73000001");
        assert_eq!(q[0].depth_km, 8.2);
        assert!((q[1].intensity() - (4.1 / 7.0)).abs() < 1e-9);
        let a = q[1].attrs().into_map();
        assert!((a["lon01"].as_f64().unwrap() - (2.5 / 360.0)).abs() < 1e-9);
        assert_eq!(a["place"].as_str(), Some("Kermadec Islands"));
    }

    #[test]
    fn factory_validates_feed() {
        assert!(factory(&SourceSpec::new("usgs.earthquakes").with("feed", "nope")).is_err());
        assert!(factory(&SourceSpec::new("usgs.earthquakes").with("feed", "2.5_day")).is_ok());
    }

    /// `cargo test -p sonder-sources -- --ignored usgs`
    #[tokio::test]
    #[ignore]
    async fn live_feed_parses() {
        let v: Json = reqwest::Client::new()
            .get(feed_url("all_day"))
            .header(reqwest::header::USER_AGENT, util::USER_AGENT)
            .send().await.unwrap().json().await.unwrap();
        let q = parse_feed(&v);
        assert!(!q.is_empty());
        assert!(q.iter().all(|q| q.lat.abs() <= 90.0));
    }
}
