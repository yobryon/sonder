//! `noaa.space-weather` — NOAA SWPC solar wind plasma + planetary K index.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const PLASMA_URL: &str = "https://services.swpc.noaa.gov/products/solar-wind/plasma-7-day.json";
pub const MAG_URL: &str = "https://services.swpc.noaa.gov/products/solar-wind/mag-7-day.json";
pub const KP_URL: &str = "https://services.swpc.noaa.gov/products/noaa-planetary-k-index.json";

/// Parse an SWPC array-of-arrays (header row first) and return the last row with
/// a non-null value for every requested column, as `(column, value)` pairs.
pub fn latest_row(v: &Json, columns: &[&str]) -> Option<Vec<(String, f64)>> {
    let rows = v.as_array()?;
    let header: Vec<&str> = rows.first()?.as_array()?.iter().filter_map(Json::as_str).collect();
    let idx: Vec<usize> = columns.iter().map(|c| header.iter().position(|h| h == c)).collect::<Option<Vec<_>>>()?;
    for row in rows.iter().skip(1).rev() {
        let r = row.as_array()?;
        let vals: Option<Vec<f64>> = idx.iter().map(|&i| r.get(i).and_then(util::json_f64_opt)).collect();
        if let Some(vals) = vals {
            return Some(columns.iter().map(|c| c.to_string()).zip(vals).collect());
        }
    }
    None
}

/// Readings from the plasma feed: speed (km/s), density (p/cc), temperature (K).
pub fn parse_plasma(v: &Json) -> Option<Vec<(String, f64)>> {
    latest_row(v, &["speed", "density", "temperature"])
}

/// Readings from the magnetometer feed: bz_gsm, bt.
pub fn parse_mag(v: &Json) -> Option<Vec<(String, f64)>> {
    latest_row(v, &["bz_gsm", "bt"]).map(|r| r.into_iter().map(|(k, v)| (if k == "bz_gsm" { "bz".to_string() } else { k }, v)).collect())
}

/// Latest Kp (the feed is `[time_tag, Kp, a_running, station_count]`).
pub fn parse_kp(v: &Json) -> Option<f64> {
    latest_row(v, &["Kp"]).and_then(|r| r.first().map(|(_, v)| *v))
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
                    let (plasma, mag, kp) = tokio::join!(
                        util::get_json(&em.ctx, PLASMA_URL),
                        util::get_json(&em.ctx, MAG_URL),
                        util::get_json(&em.ctx, KP_URL)
                    );
                    let mut vals: Vec<(String, f64)> = Vec::new();
                    let mut errs = Vec::new();
                    match plasma {
                        Ok(v) => match parse_plasma(&v) {
                            Some(r) => vals.extend(r),
                            None => errs.push("plasma: no complete row".to_string()),
                        },
                        Err(e) => errs.push(format!("plasma: {e}")),
                    }
                    match mag {
                        Ok(v) => {
                            if let Some(r) = parse_mag(&v) {
                                vals.extend(r);
                            }
                        }
                        Err(e) => errs.push(format!("mag: {e}")),
                    }
                    let mut kp_now = None;
                    match kp {
                        Ok(v) => match parse_kp(&v) {
                            Some(k) => {
                                kp_now = Some(k);
                                vals.push(("kp".into(), k));
                                vals.push(("kp01".into(), clamp01(k / 9.0)));
                            }
                            None => errs.push("kp: no rows".to_string()),
                        },
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

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "noaa.space-weather",
        summary: "Solar wind speed/density/temperature, Bz and planetary Kp from NOAA SWPC",
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
    }

    /// `cargo test -p sonder-sources -- --ignored space_weather`
    #[tokio::test]
    #[ignore]
    async fn live_feeds_parse() {
        let c = reqwest::Client::new();
        let p: Json = c.get(PLASMA_URL).header(reqwest::header::USER_AGENT, util::USER_AGENT).send().await.unwrap().json().await.unwrap();
        assert!(parse_plasma(&p).is_some());
        let k: Json = c.get(KP_URL).header(reqwest::header::USER_AGENT, util::USER_AGENT).send().await.unwrap().json().await.unwrap();
        assert!(parse_kp(&k).is_some());
    }
}
