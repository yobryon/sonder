//! `noaa.buoys` — NOAA National Data Buoy Center latest observations, polled.
//!
//! Fetches <https://www.ndbc.noaa.gov/data/latest_obs/latest_obs.txt> (one row per
//! station, two `#` header rows, `MM` = missing) every `interval` seconds and reports
//! the chosen stations' most recent observation.
//!
//! Stations are picked by id (`stations`) or by proximity (`near = [lat, lon]` plus
//! `radius_km`, nearest first, at most 16).
//!
//! * readings per station — `wdir`, `wspd`, `gst`, `wvht`, `dpd`, `apd`, `mwd`, `pres`,
//!   `ptdy`, `atmp`, `wtmp`, `dewp` — bare when exactly one station is selected,
//!   prefixed by station id otherwise (`46026.wvht`); plus aggregates `wvht_max`,
//!   `wspd_mean`, `wtmp_mean`, `stations` (count of stations found in the file);
//! * event `update` when a station's observation timestamp advances (including the
//!   first poll): attrs `station`, `wvht`, `wspd`, `lat`, `lon`, `lat01`, `lon01`;
//!   intensity = clamp(wvht/6, 0, 1).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use chrono::{NaiveDate, TimeZone, Utc};
use sonder_core::SourceSpec;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, Poll};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const URL: &str = "https://www.ndbc.noaa.gov/data/latest_obs/latest_obs.txt";

/// Measurement columns we report, in file order.
pub const FIELDS: &[&str] = &["wdir", "wspd", "gst", "wvht", "dpd", "apd", "mwd", "pres", "ptdy", "atmp", "wtmp", "dewp"];

const MAX_NEAR: usize = 16;

/// One station's latest observation.
#[derive(Debug, Clone, PartialEq)]
pub struct Obs {
    pub station: String,
    pub lat: f64,
    pub lon: f64,
    /// Observation time, Unix seconds.
    pub time: f64,
    /// Lower-case field name → value (missing fields absent).
    pub values: BTreeMap<String, f64>,
}

impl Obs {
    pub fn get(&self, k: &str) -> Option<f64> {
        self.values.get(k).copied()
    }
    pub fn intensity(&self) -> f64 {
        clamp01(self.get("wvht").unwrap_or(0.0) / 6.0)
    }
    pub fn attrs(&self) -> Attrs {
        Attrs::new()
            .text("station", self.station.clone())
            .opt_num("wvht", self.get("wvht"))
            .opt_num("wspd", self.get("wspd"))
            .num("lat", self.lat)
            .num("lon", self.lon)
            .num("lat01", clamp01((self.lat + 90.0) / 180.0))
            .num("lon01", clamp01((self.lon + 180.0) / 360.0))
    }
}

/// Parse the whole file. Rows that don't fit the header are skipped.
pub fn parse(text: &str) -> Vec<Obs> {
    let mut lines = text.lines();
    let header: Vec<String> = match lines.next() {
        Some(h) if h.starts_with('#') => h.trim_start_matches('#').split_whitespace().map(|c| c.to_ascii_lowercase()).collect(),
        // No header → assume the documented layout.
        _ => "STN LAT LON YYYY MM DD hh mm WDIR WSPD GST WVHT DPD APD MWD PRES PTDY ATMP WTMP DEWP VIS TIDE"
            .split_whitespace()
            .map(|c| c.to_ascii_lowercase())
            .collect(),
    };
    let idx = |name: &str| header.iter().position(|h| h == name);
    let (Some(i_stn), Some(i_lat), Some(i_lon)) = (idx("stn"), idx("lat"), idx("lon")) else { return Vec::new() };
    // `MM` (month) and `mm` (minute) collide once lower-cased: the minute column follows `hh`.
    let time_idx = [idx("yyyy"), idx("mm"), idx("dd"), idx("hh"), idx("hh").map(|i| i + 1)];
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < header.len().min(8) {
            continue;
        }
        let num = |i: usize| cols.get(i).filter(|s| **s != "MM").and_then(|s| s.parse::<f64>().ok());
        let (Some(lat), Some(lon)) = (num(i_lat), num(i_lon)) else { continue };
        let t = match time_idx {
            [Some(y), Some(mo), Some(d), Some(h), Some(mi)] => {
                let f = |i: usize| cols.get(i).and_then(|s| s.parse::<u32>().ok());
                match (f(y), f(mo), f(d), f(h), f(mi)) {
                    (Some(y), Some(mo), Some(d), Some(h), Some(mi)) => NaiveDate::from_ymd_opt(y as i32, mo, d)
                        .and_then(|dt| dt.and_hms_opt(h, mi, 0))
                        .map(|ndt| Utc.from_utc_datetime(&ndt).timestamp() as f64)
                        .unwrap_or(0.0),
                    _ => 0.0,
                }
            }
            _ => 0.0,
        };
        let mut values = BTreeMap::new();
        for f in FIELDS {
            if let Some(v) = idx(f).and_then(num) {
                values.insert(f.to_string(), v);
            }
        }
        out.push(Obs { station: cols[i_stn].to_string(), lat, lon, time: t, values });
    }
    out
}

/// Great-circle distance in km.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Select {
    Ids(Vec<String>),
    Near { lat: f64, lon: f64, radius_km: f64 },
}

impl Select {
    /// Pick the selected observations (nearest first for `Near`).
    pub fn pick<'a>(&self, all: &'a [Obs]) -> Vec<&'a Obs> {
        match self {
            Select::Ids(ids) => ids.iter().filter_map(|id| all.iter().find(|o| o.station.eq_ignore_ascii_case(id))).collect(),
            Select::Near { lat, lon, radius_km } => {
                let mut v: Vec<(f64, &Obs)> = all
                    .iter()
                    .map(|o| (haversine_km(*lat, *lon, o.lat, o.lon), o))
                    .filter(|(d, _)| *d <= *radius_km)
                    .collect();
                v.sort_by(|a, b| a.0.total_cmp(&b.0));
                v.into_iter().take(MAX_NEAR).map(|(_, o)| o).collect()
            }
        }
    }
}

/// Readings for a set of observations (bare names for one station, prefixed otherwise).
pub fn readings(picked: &[&Obs]) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let single = picked.len() == 1;
    for o in picked {
        for (k, v) in &o.values {
            out.push((if single { k.clone() } else { format!("{}.{}", o.station, k) }, *v));
        }
    }
    let mean = |k: &str| {
        let xs: Vec<f64> = picked.iter().filter_map(|o| o.get(k)).collect();
        if xs.is_empty() { None } else { Some(xs.iter().sum::<f64>() / xs.len() as f64) }
    };
    if let Some(m) = picked.iter().filter_map(|o| o.get("wvht")).reduce(f64::max) {
        out.push(("wvht_max".into(), m));
    }
    if let Some(m) = mean("wspd") {
        out.push(("wspd_mean".into(), m));
    }
    if let Some(m) = mean("wtmp") {
        out.push(("wtmp_mean".into(), m));
    }
    out.push(("stations".into(), picked.len() as f64));
    out
}

struct NdbcSource {
    select: Select,
    interval: Duration,
}

impl Source for NdbcSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut last_time: HashMap<String, f64> = HashMap::new();
            let mut poller = util::Poller::new(self.interval);
            poller.start(&mut em).await;
            loop {
                let res = self.poll_once(&mut em, &mut last_time).await;
                if !poller.after(&mut em, res).await {
                    return;
                }
            }
        })
    }
}

impl NdbcSource {
    async fn poll_once(&self, em: &mut Emitter, last_time: &mut HashMap<String, f64>) -> Result<Poll, String> {
        let text = util::get_text(&em.ctx, URL).await?;
        let all = parse(&text);
        if all.is_empty() {
            return Err("unexpected payload (no observations parsed)".into());
        }
        let picked = self.select.pick(&all);
        if picked.is_empty() {
            return Err(match &self.select {
                Select::Ids(ids) => format!("none of {} found in latest_obs", ids.join(", ")),
                Select::Near { radius_km, .. } => format!("no stations within {radius_km:.0} km"),
            });
        }
        em.reading(readings(&picked)).await;
        let mut fresh = false;
        for o in &picked {
            let prev = last_time.get(&o.station).copied();
            if prev.is_none_or(|p| o.time > p) {
                last_time.insert(o.station.clone(), o.time);
                fresh = true;
                em.event("update", o.intensity(), o.attrs().into_map()).await;
            }
        }
        Ok(if fresh { Poll::Live } else { Poll::Idle })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "noaa.buoys",
        summary: "NOAA NDBC buoy observations (waves, wind, pressure, temperatures); an `update` event per new observation",
        params: vec![
            ParamInfo { name: "stations", summary: "Station ids, e.g. [\"46026\", \"46012\"]", default: Some("[\"46026\"] unless `near` is set") },
            ParamInfo { name: "near", summary: "[lat, lon]: pick the nearest stations instead", default: None },
            ParamInfo { name: "radius_km", summary: "Search radius for `near` (≤ 16 stations)", default: Some("300") },
            ParamInfo { name: "interval", summary: "Poll interval (s)", default: Some("300") },
        ],
        channels: vec![
            "wdir", "wspd", "gst", "wvht", "dpd", "apd", "mwd", "pres", "ptdy", "atmp", "wtmp", "dewp", "wvht_max", "wspd_mean", "wtmp_mean",
            "stations",
        ],
        events: vec!["update"],
        network: true,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let near = match spec.params.get("near") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let nums: Vec<f64> = match v {
                serde_json::Value::Array(a) => a.iter().filter_map(util::json_f64_opt).collect(),
                serde_json::Value::String(s) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
                _ => Vec::new(),
            };
            if nums.len() != 2 || nums[0].abs() > 90.0 || nums[1].abs() > 180.0 {
                return Err(util::invalid(spec, "near", "expected [lat, lon]"));
            }
            Some((nums[0], nums[1]))
        }
    };
    let select = match near {
        Some((lat, lon)) => Select::Near { lat, lon, radius_km: util::param_f64_in(spec, "radius_km", 300.0, 1.0, 20_000.0)? },
        None => {
            let ids: Vec<String> = util::param_str_list(spec, "stations")
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty())
                .collect();
            Select::Ids(if ids.is_empty() { vec!["46026".to_string()] } else { ids })
        }
    };
    Ok(Box::new(NdbcSource { select, interval: util::param_secs(spec, "interval", 300.0, 60.0)? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten lines captured from latest_obs.txt (2026-10-15), with `MM` gaps.
    pub const FIXTURE: &str = "\
#STN       LAT      LON  YYYY MM DD hh mm WDIR WSPD   GST WVHT  DPD APD MWD   PRES  PTDY  ATMP  WTMP  DEWP  VIS   TIDE
#text      deg      deg   yr mo day hr mn degT  m/s   m/s   m   sec sec degT   hPa   hPa  degC  degC  degC  nmi     ft
22101    37.24   126.02  2026 10 15 00 00 320   5.0    MM  0.5   0   MM  MM     MM    MM  16.2  22.1    MM   MM     MM
32ST0   -22.000  -85.000 2026 10 15 00 30 110   4.0    MM   MM  MM   MM  MM 1018.9    MM  19.3  20.5  13.2   MM     MM
41008    31.400  -80.866 2026 10 15 00 40 130   3.0   4.0   MM  MM  6.1 114 1018.4    MM  25.6    MM  22.9   MM     MM
46012    37.356 -122.881 2026 10 15 01 00 310   4.0   5.0  1.1  15   MM  MM 1019.0  -0.6  16.9  17.3  14.1   MM     MM
46013    38.235 -123.317 2026 10 15 00 40 300   5.0   6.0   MM  MM   MM  MM 1018.8    MM  14.4    MM  13.3   MM     MM
46026    37.750 -122.838 2026 10 15 00 50 310   4.0   5.0  1.2  15  7.6 251 1018.7    MM    MM  16.0    MM   MM     MM
46237    37.788 -122.634 2026 10 15 01 00  MM    MM    MM  1.4  17 10.5 248     MM    MM  15.9  15.4    MM   MM     MM
BROKEN   abc   -122.0  2026 10 15 01 00  MM    MM    MM  1.4  17 10.5 248     MM    MM  15.9  15.4    MM   MM     MM
";

    #[test]
    fn parses_fixture_with_missing_values() {
        let all = parse(FIXTURE);
        assert_eq!(all.len(), 7, "{:?}", all.iter().map(|o| &o.station).collect::<Vec<_>>());
        let o = all.iter().find(|o| o.station == "46026").unwrap();
        assert_eq!(o.get("wvht"), Some(1.2));
        assert_eq!(o.get("mwd"), Some(251.0));
        assert_eq!(o.get("ptdy"), None);
        assert_eq!(o.get("atmp"), None);
        // 2026-10-15 00:50 UTC
        assert_eq!(o.time, 1_792_025_400.0);
        let a = o.attrs().into_map();
        assert_eq!(a["station"].as_str(), Some("46026"));
        assert!((a["lon01"].as_f64().unwrap() - (57.162 / 360.0)).abs() < 1e-6);
        assert!((o.intensity() - 0.2).abs() < 1e-9);
        let n = all.iter().find(|o| o.station == "46237").unwrap();
        assert_eq!(n.get("wspd"), None);
        assert_eq!(n.get("dpd"), Some(17.0));
    }

    #[test]
    fn selection_and_readings() {
        let all = parse(FIXTURE);
        let one = Select::Ids(vec!["46026".into()]).pick(&all);
        let r: BTreeMap<_, _> = readings(&one).into_iter().collect();
        assert_eq!(r["wvht"], 1.2);
        assert_eq!(r["wvht_max"], 1.2);
        assert_eq!(r["stations"], 1.0);
        assert!(!r.contains_key("46026.wvht"));

        // San Francisco: 46026, 46237, 46012, 46013 within 100 km, nearest first.
        let near = Select::Near { lat: 37.77, lon: -122.42, radius_km: 100.0 }.pick(&all);
        let ids: Vec<&str> = near.iter().map(|o| o.station.as_str()).collect();
        assert_eq!(ids, vec!["46237", "46026", "46012", "46013"]);
        let r: BTreeMap<_, _> = readings(&near).into_iter().collect();
        assert_eq!(r["46026.wvht"], 1.2);
        assert_eq!(r["wvht_max"], 1.4);
        assert!((r["wspd_mean"] - 13.0 / 3.0).abs() < 1e-9);
        assert!((r["wtmp_mean"] - (17.3 + 16.0 + 15.4) / 3.0).abs() < 1e-9);
        assert_eq!(r["stations"], 4.0);
        assert!(Select::Ids(vec!["nope".into()]).pick(&all).is_empty());
        assert!((haversine_km(0.0, 0.0, 0.0, 1.0) - 111.19).abs() < 0.1);
    }

    #[test]
    fn factory_validates() {
        assert!(factory(&SourceSpec::new("noaa.buoys")).is_ok());
        assert!(factory(&SourceSpec::new("noaa.buoys").with("near", vec![37.7, -122.4])).is_ok());
        assert!(factory(&SourceSpec::new("noaa.buoys").with("near", "37.7")).is_err());
        assert!(factory(&SourceSpec::new("noaa.buoys").with("interval", 10)).is_err());
        assert!(factory(&SourceSpec::new("noaa.buoys").with("stations", "46026, 46012")).is_ok());
    }

    /// `cargo test -p sonder-sources -- --ignored ndbc`
    #[tokio::test]
    #[ignore]
    async fn live_latest_obs_parses() {
        let text = reqwest::Client::new()
            .get(URL)
            .header(reqwest::header::USER_AGENT, util::USER_AGENT)
            .send().await.unwrap().text().await.unwrap();
        let all = parse(&text);
        eprintln!("{} stations parsed", all.len());
        assert!(all.len() > 100);
        let near = Select::Near { lat: 37.77, lon: -122.42, radius_km: 300.0 }.pick(&all);
        eprintln!("near SF: {:?}", near.iter().map(|o| (&o.station, o.get("wvht"), o.get("wspd"))).collect::<Vec<_>>());
        assert!(!near.is_empty());
        let r = readings(&near);
        eprintln!("readings: {r:?}");
        assert!(r.iter().any(|(k, _)| k == "stations"));
    }
}
