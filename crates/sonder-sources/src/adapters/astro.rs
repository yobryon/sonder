//! `clock.astro` — sun and moon position for a lat/lon using NOAA's solar
//! position algorithm and a truncated Meeus lunar theory. Offline, deterministic
//! when given a fixed `date`.

use std::f64::consts::PI;
use std::time::Duration;

use sonder_core::{SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, invalid, Attrs, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Altitude (deg) below which the sun counts as set (refraction + semi-diameter).
pub const HORIZON: f64 = -0.833;

fn d2r(d: f64) -> f64 {
    d * PI / 180.0
}
fn r2d(r: f64) -> f64 {
    r * 180.0 / PI
}
fn norm360(d: f64) -> f64 {
    d.rem_euclid(360.0)
}

/// Julian day from Unix seconds.
pub fn julian_day(unix: f64) -> f64 {
    unix / 86400.0 + 2_440_587.5
}

/// Sun position in the sky for an observer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPos {
    pub altitude: f64,
    pub azimuth: f64,
    pub declination: f64,
    /// Equation of time, minutes.
    pub eq_time: f64,
    /// Apparent ecliptic longitude, degrees.
    pub longitude: f64,
}

/// NOAA solar position (as in the NOAA solar calculator spreadsheet).
pub fn sun_position(unix: f64, lat: f64, lon: f64) -> SunPos {
    let jd = julian_day(unix);
    let jc = (jd - 2_451_545.0) / 36525.0;
    let l0 = norm360(280.46646 + jc * (36000.76983 + jc * 0.0003032));
    let m = 357.52911 + jc * (35999.05029 - 0.0001537 * jc);
    let ecc = 0.016708634 - jc * (0.000042037 + 0.0000001267 * jc);
    let mr = d2r(m);
    let eq_ctr = mr.sin() * (1.914602 - jc * (0.004817 + 0.000014 * jc))
        + (2.0 * mr).sin() * (0.019993 - 0.000101 * jc)
        + (3.0 * mr).sin() * 0.000289;
    let true_long = l0 + eq_ctr;
    let omega = 125.04 - 1934.136 * jc;
    let app_long = true_long - 0.00569 - 0.00478 * d2r(omega).sin();
    let mean_obliq = 23.0 + (26.0 + (21.448 - jc * (46.815 + jc * (0.00059 - jc * 0.001813))) / 60.0) / 60.0;
    let obliq = mean_obliq + 0.00256 * d2r(omega).cos();
    let decl = (d2r(obliq).sin() * d2r(app_long).sin()).asin();
    let y = (d2r(obliq / 2.0)).tan().powi(2);
    let l0r = d2r(l0);
    let eq_time = 4.0
        * r2d(
            y * (2.0 * l0r).sin() - 2.0 * ecc * mr.sin() + 4.0 * ecc * y * mr.sin() * (2.0 * l0r).cos()
                - 0.5 * y * y * (4.0 * l0r).sin()
                - 1.25 * ecc * ecc * (2.0 * mr).sin(),
        );
    let minutes_utc = (unix.rem_euclid(86400.0)) / 60.0;
    let tst = (minutes_utc + eq_time + 4.0 * lon).rem_euclid(1440.0);
    let ha = if tst / 4.0 < 0.0 { tst / 4.0 + 180.0 } else { tst / 4.0 - 180.0 };
    let latr = d2r(lat);
    let cos_zen = (latr.sin() * decl.sin() + latr.cos() * decl.cos() * d2r(ha).cos()).clamp(-1.0, 1.0);
    let zen = cos_zen.acos();
    let altitude = 90.0 - r2d(zen);
    let az_arg = ((latr.sin() * zen.cos() - decl.sin()) / (latr.cos() * zen.sin())).clamp(-1.0, 1.0);
    let az = r2d(az_arg.acos());
    let azimuth = if az.is_nan() {
        0.0
    } else if ha > 0.0 {
        norm360(az + 180.0)
    } else {
        norm360(540.0 - az)
    };
    SunPos { altitude, azimuth, declination: r2d(decl), eq_time, longitude: norm360(app_long) }
}

/// Moon position and phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoonPos {
    pub altitude: f64,
    pub azimuth: f64,
    /// 0 = new, 0.5 = full, →1 = new again.
    pub phase: f64,
    /// Illuminated fraction 0..1.
    pub illumination: f64,
}

/// Truncated Meeus lunar position (≈0.3° in longitude) and phase from elongation.
pub fn moon_position(unix: f64, lat: f64, lon: f64, sun_longitude: f64) -> MoonPos {
    let jd = julian_day(unix);
    let t = (jd - 2_451_545.0) / 36525.0;
    let lp = norm360(218.3164477 + 481267.88123421 * t);
    let d = d2r(norm360(297.8501921 + 445267.1114034 * t));
    let m = d2r(norm360(357.5291092 + 35999.0502909 * t));
    let mp = d2r(norm360(134.9633964 + 477198.8675055 * t));
    let f = d2r(norm360(93.2720950 + 483202.0175233 * t));
    let lambda = lp
        + 6.289 * mp.sin()
        - 1.274 * (2.0 * d - mp).sin()
        + 0.658 * (2.0 * d).sin()
        - 0.214 * (2.0 * mp).sin()
        - 0.186 * m.sin()
        - 0.114 * (2.0 * f).sin()
        + 0.059 * (2.0 * d - 2.0 * mp).sin()
        + 0.057 * (2.0 * d - m - mp).sin()
        + 0.053 * (2.0 * d + mp).sin()
        + 0.046 * (2.0 * d - m).sin()
        - 0.041 * (m - mp).sin()
        - 0.035 * d.sin()
        - 0.031 * (m + mp).sin();
    let beta = 5.128 * f.sin() + 0.281 * (mp + f).sin() - 0.278 * (f - mp).sin() - 0.173 * (2.0 * d - f).sin()
        + 0.055 * (2.0 * d - mp + f).sin()
        + 0.046 * (2.0 * d - mp - f).sin()
        + 0.033 * (2.0 * d + f).sin()
        + 0.017 * (2.0 * mp + f).sin();
    let eps = d2r(23.4393 - 0.0130 * t);
    let (lr, br) = (d2r(norm360(lambda)), d2r(beta));
    let ra = (lr.sin() * eps.cos() - br.tan() * eps.sin()).atan2(lr.cos());
    let dec = (br.sin() * eps.cos() + br.cos() * eps.sin() * lr.sin()).asin();
    // Greenwich mean sidereal time (deg).
    let d_ut = jd - 2_451_545.0;
    let gmst = norm360(280.46061837 + 360.98564736629 * d_ut);
    let lst = d2r(norm360(gmst + lon));
    let ha = lst - ra;
    let latr = d2r(lat);
    let sin_alt = (latr.sin() * dec.sin() + latr.cos() * dec.cos() * ha.cos()).clamp(-1.0, 1.0);
    let alt = sin_alt.asin();
    let az = (-ha.sin()).atan2(dec.tan() * latr.cos() - latr.sin() * ha.cos());
    // Parallax lowers the apparent moon by ~0.95° × cos(alt).
    let altitude = r2d(alt) - 0.95 * alt.cos();
    let elong = norm360(lambda - sun_longitude);
    let phase = elong / 360.0;
    let illumination = (1.0 - d2r(elong).cos()) / 2.0;
    MoonPos { altitude, azimuth: norm360(r2d(az)), phase, illumination }
}

/// Everything the source reports at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct AstroReading {
    pub sun: SunPos,
    pub moon: MoonPos,
    pub is_day: bool,
    pub twilight: f64,
    pub hour: f64,
}

/// Compute the full reading at `unix` for `lat`,`lon` with a UTC offset in hours.
pub fn compute(unix: f64, lat: f64, lon: f64, utc_offset_h: f64) -> AstroReading {
    let sun = sun_position(unix, lat, lon);
    let moon = moon_position(unix, lat, lon, sun.longitude);
    let is_day = sun.altitude > HORIZON;
    // 0 at astronomical night (−18°), 1 at the horizon and above.
    let twilight = clamp01((sun.altitude + 18.0) / 18.0);
    let hour = ((unix + utc_offset_h * 3600.0).rem_euclid(86400.0)) / 3600.0;
    AstroReading { sun, moon, is_day, twilight, hour }
}

impl AstroReading {
    pub fn values(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("sun_altitude", self.sun.altitude),
            ("sun_azimuth", self.sun.azimuth),
            ("sun_elevation01", clamp01((self.sun.altitude + 90.0) / 180.0)),
            ("moon_phase", self.moon.phase),
            ("moon_illumination", self.moon.illumination),
            ("moon_altitude", self.moon.altitude),
            ("moon_azimuth", self.moon.azimuth),
            ("is_day", if self.is_day { 1.0 } else { 0.0 }),
            ("twilight", self.twilight),
            ("hour", self.hour),
            ("hour01", self.hour / 24.0),
        ]
    }
}

/// Parse a `date` param: unix seconds or RFC 3339 / `YYYY-MM-DD[THH:MM[:SS]]` (UTC).
pub fn parse_date(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Ok(n) = t.parse::<f64>() {
        return Some(n);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(dt.timestamp() as f64);
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S") {
        return Some(d.and_utc().timestamp() as f64);
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M") {
        return Some(d.and_utc().timestamp() as f64);
    }
    if let Ok(d) = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M") {
        return Some(d.and_utc().timestamp() as f64);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return d.and_hms_opt(12, 0, 0).map(|dt| dt.and_utc().timestamp() as f64);
    }
    None
}

struct AstroSource {
    lat: f64,
    lon: f64,
    utc_offset: f64,
    /// Unix time at engine epoch (None = wall clock at start).
    epoch: Option<f64>,
    /// Simulated seconds per engine second (for demos / renders).
    rate: f64,
    interval: Duration,
}

impl Source for AstroSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let start_engine = em.now();
            let epoch = self.epoch.unwrap_or_else(util::unix_now) - start_engine * self.rate;
            em.status(SourceState::Live, format!("sun/moon for {:.3},{:.3}", self.lat, self.lon)).await;
            let mut prev_day: Option<bool> = None;
            loop {
                let now = em.now();
                let unix = epoch + now * self.rate;
                let r = compute(unix, self.lat, self.lon, self.utc_offset);
                em.reading(r.values()).await;
                if let Some(p) = prev_day {
                    if p != r.is_day {
                        let name = if r.is_day { "sunrise" } else { "sunset" };
                        let attrs = Attrs::new().num("hour", r.hour).num("sun_azimuth", r.sun.azimuth).num("moon_phase", r.moon.phase);
                        em.event(name, 1.0, attrs.into_map()).await;
                    }
                }
                prev_day = Some(r.is_day);
                if !em.sleep(self.interval).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "clock.astro",
        summary: "Sun altitude/azimuth, moon phase/altitude, day/twilight and local hour for a lat/lon",
        params: vec![
            ParamInfo { name: "lat", summary: "Latitude in degrees", default: Some("51.5") },
            ParamInfo { name: "lon", summary: "Longitude in degrees (east positive)", default: Some("-0.12") },
            ParamInfo { name: "utc_offset", summary: "Hours offset for the `hour` channel", default: Some("0") },
            ParamInfo { name: "date", summary: "Fixed start time (unix or ISO 8601) for deterministic renders", default: None },
            ParamInfo { name: "rate", summary: "Simulated seconds per real second (e.g. 3600 = a day in 24 s)", default: Some("1") },
            ParamInfo { name: "interval", summary: "Seconds between readings", default: Some("1") },
        ],
        channels: vec![
            "sun_altitude", "sun_azimuth", "sun_elevation01", "moon_phase", "moon_illumination", "moon_altitude",
            "moon_azimuth", "is_day", "twilight", "hour", "hour01",
        ],
        events: vec!["sunrise", "sunset"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let lat = util::param_f64_in(spec, "lat", 51.5, -90.0, 90.0)?;
    let lon = util::param_f64_in(spec, "lon", -0.12, -180.0, 180.0)?;
    let utc_offset = util::param_f64_in(spec, "utc_offset", 0.0, -14.0, 14.0)?;
    let epoch = match spec.params.get("date") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some(parse_date(&s).ok_or_else(|| invalid(spec, "date", "expected unix seconds or ISO 8601"))?)
        }
    };
    let rate = util::param_f64_in(spec, "rate", 1.0, 0.0, 1.0e6)?;
    let interval = util::param_secs(spec, "interval", 1.0, 0.05)?;
    Ok(Box::new(AstroSource { lat, lon, utc_offset, epoch, rate, interval }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summer_noon_in_london() {
        // 2024-06-21 12:00 UTC, Greenwich: solar noon ≈ 11:58 UTC, altitude ≈ 61.9°.
        let unix = parse_date("2024-06-21T12:00:00").unwrap();
        let s = sun_position(unix, 51.48, 0.0);
        assert!((s.altitude - 61.9).abs() < 1.0, "altitude {}", s.altitude);
        assert!((s.azimuth - 180.0).abs() < 3.0, "azimuth {}", s.azimuth);
        assert!((s.declination - 23.43).abs() < 0.1);
    }

    #[test]
    fn midnight_is_dark_and_sunrise_detected() {
        let midnight = parse_date("2024-06-21T00:00:00").unwrap();
        let r = compute(midnight, 51.48, 0.0, 1.0);
        assert!(!r.is_day);
        assert!(r.sun.altitude < -10.0);
        assert!((r.hour - 1.0).abs() < 1e-6);
        // Sunrise in London on 2024-06-21 ≈ 03:43 UTC.
        let before = compute(parse_date("2024-06-21T03:35:00").unwrap(), 51.48, 0.0, 0.0);
        let after = compute(parse_date("2024-06-21T03:52:00").unwrap(), 51.48, 0.0, 0.0);
        assert!(!before.is_day && after.is_day, "{} / {}", before.sun.altitude, after.sun.altitude);
    }

    #[test]
    fn full_moon_phase() {
        // Full moon 2024-01-25 17:54 UTC.
        let unix = parse_date("2024-01-25T17:54:00").unwrap();
        let r = compute(unix, 40.0, -74.0, -5.0);
        assert!((r.moon.phase - 0.5).abs() < 0.01, "phase {}", r.moon.phase);
        assert!(r.moon.illumination > 0.99);
        // New moon 2024-01-11 11:57 UTC.
        let nm = compute(parse_date("2024-01-11T11:57:00").unwrap(), 40.0, -74.0, 0.0);
        assert!(nm.moon.phase < 0.01 || nm.moon.phase > 0.99, "phase {}", nm.moon.phase);
    }

    #[test]
    fn moon_altitude_plausible() {
        // Full moon is opposite the sun: near local midnight it should be high.
        let unix = parse_date("2024-01-26T00:00:00").unwrap();
        let r = compute(unix, 51.48, 0.0, 0.0);
        assert!(r.moon.altitude > 30.0, "moon alt {}", r.moon.altitude);
        assert!(!r.is_day);
    }

    #[test]
    fn date_parsing() {
        assert_eq!(parse_date("0"), Some(0.0));
        assert_eq!(parse_date("1970-01-02"), Some(86400.0 + 12.0 * 3600.0));
        assert_eq!(parse_date("1970-01-01T01:00:00Z"), Some(3600.0));
        assert!(parse_date("nope").is_none());
    }
}
