//! `iss.position` — where the International Space Station is right now.

use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, jf, Attrs, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const URL: &str = "http://api.open-notify.org/iss-now.json";

/// Parse `{"iss_position": {"latitude": "..", "longitude": ".."}}`.
pub fn parse_position(v: &Json) -> Option<(f64, f64)> {
    let p = v.get("iss_position")?;
    Some((jf(p, "latitude")?, jf(p, "longitude")?))
}

/// Readings derived from a position.
pub fn readings(lat: f64, lon: f64) -> Vec<(&'static str, f64)> {
    vec![
        ("lat", lat),
        ("lon", lon),
        ("lat01", clamp01((lat + 90.0) / 180.0)),
        ("lon01", clamp01((lon + 180.0) / 360.0)),
        ("northbound", 0.0),
    ]
}

struct IssSource {
    interval: Duration,
}

impl Source for IssSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut prev: Option<(f64, f64)> = None;
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = async {
                    let v = util::get_json(&em.ctx, URL).await?;
                    let (lat, lon) = parse_position(&v).ok_or_else(|| "unexpected payload".to_string())?;
                    let mut vals = readings(lat, lon);
                    if let Some((plat, plon)) = prev {
                        let northbound = if lat > plat { 1.0 } else { 0.0 };
                        if let Some(v) = vals.iter_mut().find(|(k, _)| *k == "northbound") {
                            v.1 = northbound;
                        }
                        // Ground speed in deg of arc per second (≈0.07 deg/s).
                        let dlat = lat - plat;
                        let dlon = (lon - plon + 540.0).rem_euclid(360.0) - 180.0;
                        vals.push(("speed_deg_s", (dlat * dlat + dlon * dlon).sqrt() / self.interval.as_secs_f64()));
                        if (plat < 0.0) != (lat < 0.0) {
                            let attrs = Attrs::new().num("lon", lon).num("lon01", clamp01((lon + 180.0) / 360.0)).bool("northbound", lat > plat);
                            em.event("equator_cross", 1.0, attrs.into_map()).await;
                        }
                    }
                    em.reading(vals).await;
                    prev = Some((lat, lon));
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
        kind: "iss.position",
        summary: "ISS latitude/longitude from Open Notify; `equator_cross` events",
        params: vec![ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("5") }],
        channels: vec!["lat", "lon", "lat01", "lon01", "northbound", "speed_deg_s"],
        events: vec!["equator_cross"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(IssSource { interval: util::param_secs(spec, "interval", 5.0, 2.0)? }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses() {
        let v = json!({"message": "success", "timestamp": 1700000000, "iss_position": {"latitude": "-12.3456", "longitude": "100.1"}});
        let (lat, lon) = parse_position(&v).unwrap();
        assert_eq!(lat, -12.3456);
        let r: std::collections::BTreeMap<_, _> = readings(lat, lon).into_iter().collect();
        assert!((r["lon01"] - (280.1 / 360.0)).abs() < 1e-9);
        assert!(parse_position(&json!({"message": "error"})).is_none());
    }
}
