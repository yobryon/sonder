//! `noaa.tides` — latest water level at a NOAA CO-OPS station.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::{Seconds, SourceSpec};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, jf, js, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub fn url(station: &str) -> String {
    format!(
        "https://api.tidesandcurrents.noaa.gov/api/prod/datagetter?product=water_level&date=latest&station={station}&datum=MLLW&units=metric&time_zone=gmt&format=json"
    )
}

/// Parse `{"data":[{"t":"2024-01-01 00:00","v":"1.234",...}]}` → (water level m, timestamp string).
pub fn parse_level(v: &Json) -> Result<(f64, String), String> {
    if let Some(err) = v.get("error").and_then(|e| js(e, "message")) {
        return Err(format!("NOAA: {err}"));
    }
    let data = v.get("data").and_then(Json::as_array).ok_or_else(|| "no data array".to_string())?;
    let last = data.last().ok_or_else(|| "empty data".to_string())?;
    let level = jf(last, "v").ok_or_else(|| "no `v` field".to_string())?;
    Ok((level, js(last, "t").unwrap_or("").to_string()))
}

/// Trend (m/hour) between two readings `dt` seconds apart.
pub fn trend(prev: f64, cur: f64, dt: Seconds) -> f64 {
    if dt <= 0.0 {
        0.0
    } else {
        (cur - prev) / dt * 3600.0
    }
}

struct TidesSource {
    station: String,
    interval: Duration,
}

impl Source for TidesSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let url = url(&self.station);
            let mut em = Emitter::new(ctx);
            let mut prev: Option<(f64, String, Seconds)> = None;
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json(&em.ctx, &url).await?;
                    let (level, stamp) = parse_level(&v)?;
                    let now = em.now();
                    let mut vals = vec![("water_level", level), ("water_level01", util::clamp01((level + 1.0) / 4.0))];
                    let mut fresh = true;
                    if let Some((plevel, pstamp, pt)) = &prev {
                        if *pstamp == stamp {
                            fresh = false;
                        } else {
                            vals.push(("trend", trend(*plevel, level, now - pt)));
                        }
                    }
                    em.reading(vals).await;
                    if fresh {
                        prev = Some((level, stamp, now));
                    }
                    Ok(if fresh { Poll::Live } else { Poll::Idle })
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
        kind: "noaa.tides",
        summary: "Water level at a NOAA CO-OPS tide station (slow, 6-minute data)",
        params: vec![
            ParamInfo { name: "station", summary: "Station id (9414290 = San Francisco)", default: Some("9414290") },
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("120") },
        ],
        channels: vec!["water_level", "water_level01", "trend"],
        events: vec![],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let station = match spec.params.get("station") {
        None | Some(Json::Null) => "9414290".to_string(),
        Some(Json::String(s)) => s.trim().to_string(),
        Some(Json::Number(n)) => n.to_string(),
        Some(_) => return Err(util::invalid(spec, "station", "expected a station id")),
    };
    if station.is_empty() || !station.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(util::invalid(spec, "station", "expected an alphanumeric station id"));
    }
    Ok(Box::new(TidesSource { station, interval: util::param_secs(spec, "interval", 120.0, 60.0)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_level() {
        let v = json!({"metadata": {"id": "9414290", "name": "San Francisco"}, "data": [{"t": "2024-01-01 00:00", "v": "1.234", "s": "0.010", "f": "0,0,0,0", "q": "p"}]});
        let (lvl, t) = parse_level(&v).unwrap();
        assert_eq!(lvl, 1.234);
        assert_eq!(t, "2024-01-01 00:00");
        assert!(parse_level(&json!({"error": {"message": "No data was found"}})).is_err());
        assert!((trend(1.0, 1.1, 360.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn factory_accepts_numeric_station() {
        assert!(factory(&SourceSpec::new("noaa.tides").with("station", 8518750)).is_ok());
        assert!(factory(&SourceSpec::new("noaa.tides").with("station", "../x")).is_err());
    }
}
