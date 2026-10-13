//! `open-meteo.weather` — current weather for a lat/lon from Open-Meteo (no key).

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const FIELDS: &[&str] = &[
    "temperature_2m", "relative_humidity_2m", "precipitation", "cloud_cover", "wind_speed_10m",
    "wind_direction_10m", "wind_gusts_10m", "pressure_msl", "is_day", "weather_code",
];

pub fn url(lat: f64, lon: f64) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current={}&wind_speed_unit=ms&timezone=UTC",
        FIELDS.join(",")
    )
}

/// Extract the `current` block as readings (snake_case field names, as Open-Meteo names them).
pub fn parse_current(v: &Json) -> Vec<(String, f64)> {
    let Some(cur) = v.get("current").and_then(Json::as_object) else { return Vec::new() };
    let mut out: Vec<(String, f64)> = cur
        .iter()
        .filter(|(k, _)| k.as_str() != "time" && k.as_str() != "interval")
        .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
        .collect();
    // Convenience normalized channels.
    let get = |out: &[(String, f64)], k: &str| out.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let extra = [
        ("cloud01", get(&out, "cloud_cover").map(|c| clamp01(c / 100.0))),
        ("humidity01", get(&out, "relative_humidity_2m").map(|h| clamp01(h / 100.0))),
        ("wind01", get(&out, "wind_speed_10m").map(|w| clamp01(w / 25.0))),
        ("temp01", get(&out, "temperature_2m").map(|t| clamp01((t + 10.0) / 50.0))),
    ];
    for (k, v) in extra {
        if let Some(v) = v {
            out.push((k.to_string(), v));
        }
    }
    out
}

struct MeteoSource {
    lat: f64,
    lon: f64,
    interval: Duration,
}

impl Source for MeteoSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let url = url(self.lat, self.lon);
            let mut em = Emitter::new(ctx);
            let mut prev_gust: Option<f64> = None;
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json(&em.ctx, &url).await?;
                    let vals = parse_current(&v);
                    if vals.is_empty() {
                        return Err(format!("no `current` block: {}", v.get("reason").and_then(Json::as_str).unwrap_or("?")));
                    }
                    let gust = vals.iter().find(|(k, _)| k == "wind_gusts_10m").map(|(_, v)| *v);
                    em.reading(vals.clone()).await;
                    if let (Some(p), Some(g)) = (prev_gust, gust) {
                        if g - p > 3.0 {
                            let attrs = Attrs::new().num("gust", g).num("prev", p);
                            em.event("gust", clamp01((g - p) / 15.0), attrs.into_map()).await;
                        }
                    }
                    prev_gust = gust.or(prev_gust);
                    Ok(Poll::Live)
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
        kind: "open-meteo.weather",
        summary: "Current weather (temperature, wind, clouds, rain, pressure) from Open-Meteo",
        params: vec![
            ParamInfo { name: "lat", summary: "Latitude", default: Some("51.5") },
            ParamInfo { name: "lon", summary: "Longitude", default: Some("-0.12") },
            ParamInfo { name: "interval", summary: "Poll interval (s); the data updates every 15 min", default: Some("300") },
        ],
        channels: vec![
            "temperature_2m", "relative_humidity_2m", "precipitation", "cloud_cover", "wind_speed_10m",
            "wind_direction_10m", "wind_gusts_10m", "pressure_msl", "is_day", "weather_code", "cloud01", "humidity01",
            "wind01", "temp01",
        ],
        events: vec!["gust"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(MeteoSource {
        lat: util::param_f64_in(spec, "lat", 51.5, -90.0, 90.0)?,
        lon: util::param_f64_in(spec, "lon", -0.12, -180.0, 180.0)?,
        interval: util::param_secs(spec, "interval", 300.0, 60.0)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_current() {
        let v = json!({"latitude": 51.5, "current_units": {"temperature_2m": "°C"},
            "current": {"time": "2024-01-01T12:00", "interval": 900, "temperature_2m": 7.3, "relative_humidity_2m": 81,
                        "precipitation": 0.0, "cloud_cover": 100, "wind_speed_10m": 5.2, "wind_direction_10m": 210,
                        "wind_gusts_10m": 9.8, "pressure_msl": 1003.4, "is_day": 1, "weather_code": 3}});
        let m: std::collections::BTreeMap<_, _> = parse_current(&v).into_iter().collect();
        assert_eq!(m["temperature_2m"], 7.3);
        assert_eq!(m["cloud01"], 1.0);
        assert!(!m.contains_key("time"));
        assert!(!m.contains_key("interval"));
        assert_eq!(m["is_day"], 1.0);
        assert!(parse_current(&json!({"error": true})).is_empty());
    }

    /// `cargo test -p sonder-sources -- --ignored open_meteo`
    #[tokio::test]
    #[ignore]
    async fn live_current_parses() {
        let v: Json = reqwest::Client::new().get(url(51.5, -0.12))
            .header(reqwest::header::USER_AGENT, util::USER_AGENT)
            .send().await.unwrap().json().await.unwrap();
        let m = parse_current(&v);
        assert!(m.iter().any(|(k, _)| k == "temperature_2m"), "{v}");
    }
}
