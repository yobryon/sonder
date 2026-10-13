//! `noaa.space-weather` — NOAA SWPC solar wind (speed, density, temperature, Bz, Bt)
//! and the planetary K index.
//!
//! SWPC publishes two JSON shapes: array-of-arrays with a header row (the
//! `products/…` feeds) and array-of-objects (the `json/…` feeds). [`latest_row`]
//! reads both. The primary solar-wind source is the propagated 1-hour feed (one
//! request, all columns); the real-time `rtsw` plasma+mag pair is the fallback.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Propagated solar wind at Earth, array-of-arrays with header row.
pub const PROPAGATED_URL: &str = "https://services.swpc.noaa.gov/products/geospace/propagated-solar-wind-1-hour.json";
/// Real-time solar wind plasma (array of objects) — fallback.
pub const PLASMA_URL: &str = "https://services.swpc.noaa.gov/json/rtsw/rtsw_wind_1m.json";
/// Real-time solar wind magnetometer (array of objects) — fallback.
pub const MAG_URL: &str = "https://services.swpc.noaa.gov/json/rtsw/rtsw_mag_1m.json";
/// Planetary K index (array of objects; formerly array-of-arrays — both parse).
pub const KP_URL: &str = "https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json";

/// Return the most recent row with a non-null value for every requested column,
/// as `(column, value)` pairs. Accepts array-of-arrays (header row first, oldest
/// first) and array-of-objects (any order; the greatest `time_tag` wins).
pub fn latest_row(v: &Json, columns: &[&str]) -> Option<Vec<(String, f64)>> {
    let rows = v.as_array()?;
    let first = rows.first()?;
    if let Some(header) = first.as_array() {
        let header: Vec<&str> = header.iter().filter_map(Json::as_str).collect();
        let idx: Vec<usize> = columns.iter().map(|c| header.iter().position(|h| h == c)).collect::<Option<Vec<_>>>()?;
        for row in rows.iter().skip(1).rev() {
            let r = row.as_array()?;
            let vals: Option<Vec<f64>> = idx.iter().map(|&i| r.get(i).and_then(util::json_f64_opt)).collect();
            if let Some(vals) = vals {
                return Some(columns.iter().map(|c| c.to_string()).zip(vals).collect());
            }
        }
        return None;
    }
    if first.is_object() {
        let mut best: Option<(&str, Vec<f64>)> = None;
        for row in rows {
            let obj = row.as_object()?;
            let vals: Option<Vec<f64>> = columns.iter().map(|c| obj.get(*c).and_then(util::json_f64_opt)).collect();
            let Some(vals) = vals else { continue };
            let tag = obj.get("time_tag").and_then(Json::as_str).unwrap_or("");
            if best.as_ref().is_none_or(|(t, _)| tag >= *t) {
                best = Some((tag, vals));
            }
        }
        return best.map(|(_, vals)| columns.iter().map(|c| c.to_string()).zip(vals).collect());
    }
    None
}

/// Readings from the propagated feed: speed (km/s), density (p/cc), temperature (K), bz, bt.
pub fn parse_propagated(v: &Json) -> Option<Vec<(String, f64)>> {
    let mut out = latest_row(v, &["speed", "density", "temperature"])?;
    if let Some(mag) = latest_row(v, &["bz", "bt"]) {
        out.extend(mag);
    }
    Some(out)
}

/// Readings from the rtsw plasma feed (objects with `proton_*` columns).
pub fn parse_plasma(v: &Json) -> Option<Vec<(String, f64)>> {
    // Accept the historical header-row format too.
    if let Some(r) = latest_row(v, &["speed", "density", "temperature"]) {
        return Some(r);
    }
    latest_row(v, &["proton_speed", "proton_density", "proton_temperature"])
        .map(|r| r.into_iter().map(|(k, v)| (k.trim_start_matches("proton_").to_string(), v)).collect())
}

/// Readings from the magnetometer feed: bz (GSM), bt.
pub fn parse_mag(v: &Json) -> Option<Vec<(String, f64)>> {
    latest_row(v, &["bz_gsm", "bt"])
        .map(|r| r.into_iter().map(|(k, v)| (if k == "bz_gsm" { "bz".to_string() } else { k }, v)).collect())
}

/// Latest Kp.
pub fn parse_kp(v: &Json) -> Option<f64> {
    latest_row(v, &["Kp"]).or_else(|| latest_row(v, &["kp"])).and_then(|r| r.first().map(|(_, v)| *v))
}

struct SpaceWeatherSource {
    interval: Duration,
}

impl Source for SpaceWeatherSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut prev_kp: Option<f64> = None;
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let (wind, kp) = tokio::join!(fetch_wind(&em.ctx), util::get_json(&em.ctx, KP_URL));
                    let mut vals: Vec<(String, f64)> = Vec::new();
                    let mut errs = Vec::new();
                    match wind {
                        Ok(w) => vals.extend(w),
                        Err(e) => errs.push(e),
                    }
                    let mut kp_now = None;
                    match kp.and_then(|v| parse_kp(&v).ok_or_else(|| "kp: no rows".to_string())) {
                        Ok(k) => {
                            kp_now = Some(k);
                            vals.push(("kp".into(), k));
                            vals.push(("kp01".into(), clamp01(k / 9.0)));
                        }
                        Err(e) => errs.push(format!("kp: {e}")),
                    }
                    if vals.is_empty() {
                        return Err(errs.join("; "));
                    }
                    if let Some(speed) = vals.iter().find(|(k, _)| k == "speed").map(|(_, v)| *v) {
                        vals.push(("speed01".into(), clamp01((speed - 250.0) / 550.0)));
                    }
                    em.reading(vals).await;
                    if let (Some(p), Some(k)) = (prev_kp, kp_now) {
                        if (k - p).abs() >= 0.3 {
                            let attrs = Attrs::new().num("kp", k).num("prev", p).num("delta", k - p);
                            em.event("kp_change", clamp01(k / 9.0), attrs.into_map()).await;
                        }
                    }
                    prev_kp = kp_now.or(prev_kp);
                    if !errs.is_empty() {
                        tracing::debug!(source = %em.ctx.id, "partial space weather: {}", errs.join("; "));
                    }
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

/// Solar wind readings: propagated feed first, rtsw plasma+mag as fallback.
async fn fetch_wind(ctx: &SourceCtx) -> Result<Vec<(String, f64)>, String> {
    let primary = match util::get_json(ctx, PROPAGATED_URL).await {
        Ok(v) => parse_propagated(&v).ok_or_else(|| "propagated: no complete row".to_string()),
        Err(e) => Err(format!("propagated: {e}")),
    };
    if primary.is_ok() {
        return primary;
    }
    let (plasma, mag) = tokio::join!(util::get_json(ctx, PLASMA_URL), util::get_json(ctx, MAG_URL));
    let mut out = Vec::new();
    if let Ok(v) = &plasma {
        if let Some(r) = parse_plasma(v) {
            out.extend(r);
        }
    }
    if let Ok(v) = &mag {
        if let Some(r) = parse_mag(v) {
            out.extend(r);
        }
    }
    if out.is_empty() {
        Err(format!("{}; rtsw fallback failed too", primary.err().unwrap_or_default()))
    } else {
        Ok(out)
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "noaa.space-weather",
        summary: "Solar wind speed/density/temperature, Bz/Bt and planetary Kp from NOAA SWPC",
        params: vec![ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("120") }],
        channels: vec!["speed", "density", "temperature", "bz", "bt", "kp", "kp01", "speed01"],
        events: vec!["kp_change"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(SpaceWeatherSource { interval: util::param_secs(spec, "interval", 120.0, 60.0)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_array_of_arrays() {
        let plasma = json!([
            ["time_tag", "density", "speed", "temperature"],
            ["2024-01-01 00:00:00.000", "3.21", "412.5", "61000"],
            ["2024-01-01 00:01:00.000", null, "415.0", "62000"],
            ["2024-01-01 00:02:00.000", "3.40", "418.2", null]
        ]);
        let r: std::collections::BTreeMap<_, _> = parse_plasma(&plasma).unwrap().into_iter().collect();
        // Last complete row is the first one.
        assert_eq!(r["speed"], 412.5);
        assert_eq!(r["density"], 3.21);
        let kp = json!([["time_tag", "Kp", "a_running", "station_count"], ["2024-01-01 00:00:00.000", "2.33", 9, 8], ["2024-01-01 03:00:00.000", "3.67", 15, 8]]);
        assert_eq!(parse_kp(&kp), Some(3.67));
        assert!(parse_plasma(&json!([["time_tag", "x"]])).is_none());
        assert!(parse_plasma(&json!({"not": "array"})).is_none());
        let prop = json!([
            ["time_tag", "speed", "density", "temperature", "bx", "by", "bz", "bt", "vx", "vy", "vz", "propagated_time_tag"],
            ["2026-10-13T07:17:00Z", 468.5, 9.72, 234878.0, 7.25, -3.58, 4.44, 9.28, -468.1, -6.1, -18.4, "2026-10-13T08:05:47Z"],
            ["2026-10-13T07:18:00Z", 470.9, 9.75, 238286.0, 6.85, -2.65, 5.15, 9.11, -469.8, -8.1, -31.0, "2026-10-13T08:06:36Z"]
        ]);
        let p: std::collections::BTreeMap<_, _> = parse_propagated(&prop).unwrap().into_iter().collect();
        assert_eq!(p["speed"], 470.9);
        assert_eq!(p["bz"], 5.15);
        assert_eq!(p["bt"], 9.11);
    }

    #[test]
    fn parses_array_of_objects() {
        let wind = json!([
            {"time_tag": "2026-10-13T08:11:03", "proton_speed": 462.95, "proton_temperature": 184207, "proton_density": 8.62},
            {"time_tag": "2026-10-13T08:12:03", "proton_speed": 465.0, "proton_temperature": null, "proton_density": 8.7},
            {"time_tag": "2026-10-13T08:10:03", "proton_speed": 460.0, "proton_temperature": 180000, "proton_density": 8.5}
        ]);
        let r: std::collections::BTreeMap<_, _> = parse_plasma(&wind).unwrap().into_iter().collect();
        assert_eq!(r["speed"], 462.95, "newest complete row wins regardless of order");
        assert_eq!(r["density"], 8.62);
        let mag = json!([{"time_tag": "2026-10-13T08:13:01", "bt": 8.9, "bz_gsm": 4.95}]);
        let m: std::collections::BTreeMap<_, _> = parse_mag(&mag).unwrap().into_iter().collect();
        assert_eq!(m["bz"], 4.95);
        let kp = json!([{"time_tag": "2026-10-13T00:00:00", "Kp": 3.33, "a_running": 18}, {"time_tag": "2026-10-13T03:00:00", "Kp": 2.0, "a_running": 18}]);
        assert_eq!(parse_kp(&kp), Some(2.0));
    }

    /// `cargo test -p sonder-sources -- --ignored space_weather`
    #[tokio::test]
    #[ignore]
    async fn live_feeds_parse() {
        let c = reqwest::Client::new();
        let get = |u: &'static str| {
            let c = c.clone();
            async move { c.get(u).header(reqwest::header::USER_AGENT, util::USER_AGENT).send().await.unwrap().json::<Json>().await.unwrap() }
        };
        assert!(parse_propagated(&get(PROPAGATED_URL).await).is_some());
        assert!(parse_plasma(&get(PLASMA_URL).await).is_some());
        assert!(parse_mag(&get(MAG_URL).await).is_some());
        assert!(parse_kp(&get(KP_URL).await).is_some());
    }
}
