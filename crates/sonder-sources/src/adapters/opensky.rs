//! `opensky.flights` — aircraft state vectors within a bounding box (anonymous API).

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll, SeenSet};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Default bbox: London area [lamin, lomin, lamax, lomax].
pub const DEFAULT_BBOX: [f64; 4] = [50.5, -1.5, 52.0, 1.0];

pub fn url(bbox: [f64; 4]) -> String {
    format!(
        "https://opensky-network.org/api/states/all?lamin={}&lomin={}&lamax={}&lomax={}",
        bbox[0], bbox[1], bbox[2], bbox[3]
    )
}

/// One aircraft state vector (subset).
#[derive(Debug, Clone, PartialEq)]
pub struct Aircraft {
    pub icao24: String,
    pub callsign: String,
    pub lon: Option<f64>,
    pub lat: Option<f64>,
    pub alt: Option<f64>,
    pub on_ground: bool,
    pub speed: Option<f64>,
    pub heading: Option<f64>,
    pub vertical_rate: Option<f64>,
}

/// Parse the `states` array-of-arrays.
pub fn parse_states(v: &Json) -> Vec<Aircraft> {
    let Some(states) = v.get("states").and_then(Json::as_array) else { return Vec::new() };
    states
        .iter()
        .filter_map(|s| {
            let a = s.as_array()?;
            let f = |i: usize| a.get(i).and_then(Json::as_f64);
            Some(Aircraft {
                icao24: a.first()?.as_str()?.to_string(),
                callsign: a.get(1).and_then(Json::as_str).unwrap_or("").trim().to_string(),
                lon: f(5),
                lat: f(6),
                alt: f(7).or_else(|| f(13)),
                on_ground: a.get(8).and_then(Json::as_bool).unwrap_or(false),
                speed: f(9),
                heading: f(10),
                vertical_rate: f(11),
            })
        })
        .collect()
}

/// Aggregate readings for a set of aircraft.
pub fn summarize(list: &[Aircraft]) -> Vec<(&'static str, f64)> {
    let n = list.len() as f64;
    let airborne: Vec<&Aircraft> = list.iter().filter(|a| !a.on_ground).collect();
    let mean = |f: &dyn Fn(&Aircraft) -> Option<f64>| {
        let vals: Vec<f64> = airborne.iter().filter_map(|a| f(a)).collect();
        if vals.is_empty() {
            0.0
        } else {
            vals.iter().sum::<f64>() / vals.len() as f64
        }
    };
    let climbing = if airborne.is_empty() {
        0.0
    } else {
        airborne.iter().filter(|a| a.vertical_rate.unwrap_or(0.0) > 0.5).count() as f64 / airborne.len() as f64
    };
    vec![
        ("count", n),
        ("airborne", airborne.len() as f64),
        ("mean_alt", mean(&|a| a.alt)),
        ("mean_speed", mean(&|a| a.speed)),
        ("climbing", climbing),
    ]
}

impl Aircraft {
    pub fn intensity(&self) -> f64 {
        // Higher and faster → bigger; a cruising jet ≈ 0.8, a light aircraft ≈ 0.2.
        let alt = clamp01(self.alt.unwrap_or(0.0) / 12000.0);
        let spd = clamp01(self.speed.unwrap_or(0.0) / 280.0);
        clamp01(0.1 + 0.5 * alt + 0.4 * spd)
    }
    pub fn attrs(&self, bbox: [f64; 4]) -> Attrs {
        let lat01 = self.lat.map(|l| clamp01((l - bbox[0]) / (bbox[2] - bbox[0]).max(1e-6)));
        let lon01 = self.lon.map(|l| clamp01((l - bbox[1]) / (bbox[3] - bbox[1]).max(1e-6)));
        Attrs::new()
            .text("icao24", self.icao24.clone())
            .text("callsign", self.callsign.clone())
            .opt_num("alt", self.alt)
            .opt_num("speed", self.speed)
            .opt_num("heading", self.heading)
            .opt_num("vertical_rate", self.vertical_rate)
            .opt_num("lat", self.lat)
            .opt_num("lon", self.lon)
            .opt_num("lat01", lat01)
            .opt_num("lon01", lon01)
            .bool("on_ground", self.on_ground)
    }
}

struct OpenSkySource {
    bbox: [f64; 4],
    interval: Duration,
}

impl Source for OpenSkySource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let url = url(self.bbox);
            let mut em = Emitter::new(ctx);
            let mut seen: SeenSet<String> = SeenSet::new(20_000);
            let mut first = true;
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json(&em.ctx, &url).await?;
                    if v.get("states").is_none() && v.get("time").is_none() {
                        return Err("unexpected payload".to_string());
                    }
                    let list = parse_states(&v);
                    em.reading(summarize(&list)).await;
                    let mut fresh = 0;
                    for a in &list {
                        if seen.insert(a.icao24.clone()) && !first {
                            fresh += 1;
                            em.event("aircraft", a.intensity(), a.attrs(self.bbox).into_map()).await;
                        }
                    }
                    first = false;
                    Ok(if fresh > 0 || !list.is_empty() { Poll::Live } else { Poll::Idle })
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
        kind: "opensky.flights",
        summary: "OpenSky Network aircraft within a bounding box; `aircraft` event when a new one appears",
        params: vec![
            ParamInfo { name: "bbox", summary: "[lamin, lomin, lamax, lomax]", default: Some("[50.5, -1.5, 52.0, 1.0] (London)") },
            ParamInfo { name: "interval", summary: "Poll interval (s); anonymous limit ≈ 10 s", default: Some("15") },
        ],
        channels: vec!["count", "airborne", "mean_alt", "mean_speed", "climbing"],
        events: vec!["aircraft"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let bbox = match spec.params.get("bbox") {
        None | Some(Json::Null) => DEFAULT_BBOX,
        Some(Json::Array(a)) if a.len() == 4 => {
            let mut b = [0.0; 4];
            for (i, v) in a.iter().enumerate() {
                b[i] = v.as_f64().ok_or_else(|| util::invalid(spec, "bbox", "expected 4 numbers"))?;
            }
            if b[0] >= b[2] || b[1] >= b[3] {
                return Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax] with min < max"));
            }
            b
        }
        Some(_) => return Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax]")),
    };
    Ok(Box::new(OpenSkySource { bbox, interval: util::param_secs(spec, "interval", 15.0, 10.0)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Json {
        json!({"time": 1700000000, "states": [
            ["4ca1fa", "RYR1234 ", "Ireland", 1700000000, 1700000000, -0.45, 51.47, 10972.8, false, 231.5, 270.1, 0.0, null, 11277.6, "4521", false, 0],
            ["406b2c", "BAW12   ", "United Kingdom", 1700000000, 1700000000, -0.2, 51.5, null, true, 5.1, 90.0, null, null, null, null, false, 0],
            ["abc123", null, "x", 1, 1, 0.1, 51.0, 3000.0, false, 120.0, 10.0, 5.2, null, 3100.0, null, false, 0]
        ]})
    }

    #[test]
    fn parses_states_and_summarizes() {
        let list = parse_states(&fixture());
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].callsign, "RYR1234");
        assert_eq!(list[1].alt, None);
        assert!(list[1].on_ground);
        let s: std::collections::BTreeMap<_, _> = summarize(&list).into_iter().collect();
        assert_eq!(s["count"], 3.0);
        assert_eq!(s["airborne"], 2.0);
        assert!((s["mean_alt"] - (10972.8 + 3000.0) / 2.0).abs() < 1e-6);
        assert_eq!(s["climbing"], 0.5);
        let a = list[0].attrs(DEFAULT_BBOX).into_map();
        assert!((a["lat01"].as_f64().unwrap() - (51.47 - 50.5) / 1.5).abs() < 1e-9);
        assert!(list[0].intensity() > list[2].intensity());
    }

    #[test]
    fn factory_bbox() {
        assert!(factory(&SourceSpec::new("opensky.flights").with("bbox", json!([1, 2, 3]))).is_err());
        assert!(factory(&SourceSpec::new("opensky.flights").with("bbox", json!([3, 2, 1, 4]))).is_err());
        assert!(factory(&SourceSpec::new("opensky.flights").with("bbox", json!([30, -10, 40, 10]))).is_ok());
    }

    /// `cargo test -p sonder-sources -- --ignored opensky`
    #[tokio::test]
    #[ignore]
    async fn live_states_parse() {
        let v: Json = reqwest::Client::new()
            .get(url(DEFAULT_BBOX))
            .header(reqwest::header::USER_AGENT, util::USER_AGENT)
            .send().await.unwrap().json().await.unwrap();
        let list = parse_states(&v);
        assert!(v.get("time").is_some());
        let _ = summarize(&list);
    }
}
