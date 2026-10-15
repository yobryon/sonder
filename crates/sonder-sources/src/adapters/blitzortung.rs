//! `blitzortung.lightning` — live lightning strikes from the Blitzortung.org community
//! network over WebSocket.
//!
//! Connects to `wss://ws1.blitzortung.org/` (rotating to ws7/ws8 on failure), sends
//! `{"a":111}` to start the stream, and decodes each text frame with the dictionary
//! decompressor documented at <https://www.gkbrk.com/blitzortung> (an LZW variant over
//! the UTF-16 code units of the frame; [`decode`] is a faithful port).
//!
//! * event `strike` per strike (sampled to `max_rate`/s by keeping every Nth, optionally
//!   filtered to `bbox`): attrs `lat`, `lon`, `lat01`, `lon01`, `polarity`, `stations`,
//!   `delay` (ms from strike to receipt), `mds`, `mcg`; intensity = clamp(stations/40, 0.1, 1);
//! * readings every 2 s: `rate` (strikes/s over 10 s), `active` (distinct 5°×5° cells with a
//!   strike in the last 60 s);
//! * a `Viz` payload every second: `{"strikes": [{"lat01", "lon01", "age_s"}]}` for the last 5 s.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value as Json};
use sonder_core::{Seconds, SourceSpec, SourceState};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, jf, Attrs, Emitter, RateWindow};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const HOSTS: &[&str] = &["wss://ws1.blitzortung.org/", "wss://ws7.blitzortung.org/", "wss://ws8.blitzortung.org/"];
pub const HELLO: &str = r#"{"a":111}"#;

/// Decode one obfuscated frame to its JSON text.
///
/// Port of the reference Python:
/// ```text
/// e = {}; d = list(b.decode()); c = d[0]; f = c; g = [c]; h = 256; o = h
/// for i in range(1, len(d)):
///     a = ord(d[i])
///     a = d[i] if h > a else e[a] if e.get(a) else f + c
///     g.append(a); c = a[0]; e[o] = f + c; o += 1; f = a
/// return ''.join(g)
/// ```
/// Python iterates code points; the frames only use code units below U+10000, so iterating
/// UTF-16 units is identical and keeps the dictionary indices (from 256 up) aligned.
pub fn decode(frame: &str) -> Option<String> {
    let d: Vec<u16> = frame.encode_utf16().collect();
    let first = *d.first()?;
    let unit_to_string = |u: u16| -> String { char::from_u32(u as u32).map(|c| c.to_string()).unwrap_or_default() };
    let mut dict: HashMap<u32, String> = HashMap::new();
    let mut c: String = unit_to_string(first);
    let mut f: String = c.clone();
    let mut out = String::with_capacity(d.len() * 2);
    out.push_str(&c);
    for (o, &u) in (256u32..).zip(&d[1..]) {
        let a: String = if (u as u32) < 256 {
            unit_to_string(u)
        } else if let Some(s) = dict.get(&(u as u32)) {
            s.clone()
        } else {
            format!("{f}{c}")
        };
        out.push_str(&a);
        c = a.chars().next().map(|ch| ch.to_string()).unwrap_or_default();
        dict.insert(o, format!("{f}{c}"));
        f = a;
    }
    Some(out)
}

/// One strike, as emitted.
#[derive(Debug, Clone, PartialEq)]
pub struct Strike {
    pub lat: f64,
    pub lon: f64,
    /// Nanoseconds since the Unix epoch.
    pub time_ns: f64,
    pub polarity: f64,
    pub stations: usize,
    pub mds: Option<f64>,
    pub mcg: Option<f64>,
    /// Server-reported delay (s), if present.
    pub delay_s: Option<f64>,
}

impl Strike {
    pub fn intensity(&self) -> f64 {
        (self.stations as f64 / 40.0).clamp(0.1, 1.0)
    }
    pub fn lat01(&self) -> f64 {
        clamp01((self.lat + 90.0) / 180.0)
    }
    pub fn lon01(&self) -> f64 {
        clamp01((self.lon + 180.0) / 360.0)
    }
    /// Milliseconds between the strike and `now_unix` (s); falls back to the server delay.
    pub fn delay_ms(&self, now_unix: f64) -> f64 {
        let d = now_unix * 1000.0 - self.time_ns / 1e6;
        if d.is_finite() && (0.0..3_600_000.0).contains(&d) {
            d
        } else {
            self.delay_s.map(|s| s * 1000.0).unwrap_or(0.0)
        }
    }
    pub fn attrs(&self, now_unix: f64) -> Attrs {
        Attrs::new()
            .num("lat", self.lat)
            .num("lon", self.lon)
            .num("lat01", self.lat01())
            .num("lon01", self.lon01())
            .num("polarity", self.polarity)
            .num("stations", self.stations as f64)
            .num("delay", self.delay_ms(now_unix))
            .opt_num("mds", self.mds)
            .opt_num("mcg", self.mcg)
    }
    /// 5°×5° cell index.
    pub fn cell(&self) -> (i32, i32) {
        (((self.lat + 90.0) / 5.0).floor() as i32, ((self.lon + 180.0) / 5.0).floor() as i32)
    }
}

/// Parse a decoded JSON strike.
pub fn parse_strike(v: &Json) -> Option<Strike> {
    let lat = jf(v, "lat")?;
    let lon = jf(v, "lon")?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let stations = v.get("sig").and_then(Json::as_array).map(|a| a.len()).unwrap_or(0);
    Some(Strike {
        lat,
        lon,
        time_ns: jf(v, "time").unwrap_or(0.0),
        polarity: jf(v, "pol").unwrap_or(0.0),
        stations,
        mds: jf(v, "mds"),
        mcg: jf(v, "mcg"),
        delay_s: jf(v, "delay"),
    })
}

/// `[lamin, lomin, lamax, lomax]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox(pub [f64; 4]);

impl BBox {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        let [lamin, lomin, lamax, lomax] = self.0;
        lat >= lamin && lat <= lamax && lon >= lomin && lon <= lomax
    }
}

/// Rolling state for readings and viz.
#[derive(Debug, Default)]
pub struct StrikeStats {
    rate: RateWindow,
    recent: VecDeque<(Seconds, f64, f64, (i32, i32))>,
    pub total: u64,
}

impl StrikeStats {
    pub fn push(&mut self, at: Seconds, s: &Strike) {
        self.total += 1;
        self.rate.push(at);
        self.recent.push_back((at, s.lat01(), s.lon01(), s.cell()));
        if self.recent.len() > 100_000 {
            self.recent.pop_front();
        }
    }
    fn prune(&mut self, now: Seconds) {
        while let Some(&(t, ..)) = self.recent.front() {
            if now - t > 60.0 {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }
    pub fn readings(&mut self, now: Seconds) -> [(&'static str, f64); 2] {
        self.prune(now);
        let cells: std::collections::HashSet<(i32, i32)> = self.recent.iter().map(|r| r.3).collect();
        [("rate", self.rate.rate(now, 10.0)), ("active", cells.len() as f64)]
    }
    pub fn viz(&mut self, now: Seconds) -> Json {
        self.prune(now);
        let strikes: Vec<Json> = self
            .recent
            .iter()
            .rev()
            .take_while(|(t, ..)| now - *t <= 5.0)
            .map(|(t, lat01, lon01, _)| json!({"lat01": lat01, "lon01": lon01, "age_s": now - t}))
            .collect();
        json!({"strikes": strikes})
    }
    /// Keep every Nth strike so emitted events stay under `max_rate`/s.
    pub fn keep(&mut self, now: Seconds, max_rate: f64) -> bool {
        if max_rate <= 0.0 {
            return true;
        }
        let incoming = self.rate.rate(now, 2.0);
        let n = (incoming / max_rate).ceil().max(1.0) as u64;
        self.total.is_multiple_of(n)
    }
}

struct BlitzSource {
    bbox: Option<BBox>,
    max_rate: f64,
}

impl Source for BlitzSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut stats = StrikeStats::default();
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            let mut host = 0usize;
            loop {
                if em.is_shutdown() {
                    return;
                }
                let url = HOSTS[host % HOSTS.len()];
                em.status_force(SourceState::Connecting, format!("connecting to {url}")).await;
                let started = std::time::Instant::now();
                let res = session(&mut em, url, self.bbox, self.max_rate, &mut stats).await;
                if em.is_shutdown() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                } else {
                    host += 1;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = res.err().unwrap_or_else(|| "connection closed".into());
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

async fn session(em: &mut Emitter, url: &str, bbox: Option<BBox>, max_rate: f64, stats: &mut StrikeStats) -> Result<(), String> {
    let mut req = url.into_client_request().map_err(|e| format!("bad url: {e}"))?;
    if let Ok(v) = util::USER_AGENT.parse() {
        req.headers_mut().insert("User-Agent", v);
    }
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
    ws.send(Message::text(HELLO)).await.map_err(|e| format!("hello failed: {e}"))?;
    let mut next_reading = em.now() + 2.0;
    let mut next_viz = em.now() + 1.0;
    let mut last_msg = std::time::Instant::now();
    let mut bad_frames = 0u32;
    loop {
        if em.is_shutdown() {
            let _ = ws.close(None).await;
            return Ok(());
        }
        let msg = tokio::select! {
            m = ws.next() => m,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if last_msg.elapsed() > Duration::from_secs(90) {
                    return Err("no strikes for 90 s".into());
                }
                None
            }
            _ = em.ctx.wait_shutdown() => { let _ = ws.close(None).await; return Ok(()); }
        };
        match msg {
            Some(Ok(Message::Text(txt))) => {
                last_msg = std::time::Instant::now();
                let decoded = decode(txt.as_str()).unwrap_or_default();
                match serde_json::from_str::<Json>(&decoded).ok().as_ref().and_then(parse_strike) {
                    Some(s) => {
                        em.status(SourceState::Live, "live").await;
                        if bbox.is_some_and(|b| !b.contains(s.lat, s.lon)) {
                            continue;
                        }
                        let now = em.now();
                        stats.push(now, &s);
                        if stats.keep(now, max_rate) {
                            em.event("strike", s.intensity(), s.attrs(util::unix_now()).into_map()).await;
                        }
                    }
                    None => {
                        bad_frames += 1;
                        if bad_frames == 1 || bad_frames.is_multiple_of(100) {
                            tracing::warn!(source = %em.ctx.id, bad_frames, "undecodable blitzortung frame");
                        }
                        if bad_frames > 50 && stats.total == 0 {
                            return Err("frames do not decode (protocol changed?)".into());
                        }
                    }
                }
            }
            Some(Ok(Message::Ping(p))) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Some(Ok(Message::Close(_))) => return Err("server closed".into()),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("ws error: {e}")),
            None => {}
        }
        let now = em.now();
        if now >= next_reading {
            next_reading = now + 2.0;
            if stats.total > 0 {
                em.reading(stats.readings(now)).await;
            }
        }
        if now >= next_viz {
            next_viz = now + 1.0;
            if stats.total > 0 {
                em.viz(stats.viz(now)).await;
            }
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "blitzortung.lightning",
        summary: "Live lightning strikes from Blitzortung.org (WebSocket): a `strike` event per strike",
        params: vec![
            ParamInfo { name: "bbox", summary: "[lamin, lomin, lamax, lomax] filter", default: None },
            ParamInfo { name: "max_rate", summary: "Cap on emitted strikes/s (keeps every Nth); 0 = unlimited", default: Some("30") },
        ],
        channels: vec!["rate", "active"],
        events: vec!["strike"],
        network: true,
    }
}

/// Parse `bbox` from an array of 4 numbers or a comma-separated string.
pub fn parse_bbox(spec: &SourceSpec) -> Result<Option<BBox>, SourceError> {
    let v = match spec.params.get("bbox") {
        None | Some(Json::Null) => return Ok(None),
        Some(v) => v,
    };
    let nums: Vec<f64> = match v {
        Json::Array(a) => a.iter().filter_map(util::json_f64_opt).collect(),
        Json::String(s) => s.split(',').filter_map(|p| p.trim().parse().ok()).collect(),
        _ => Vec::new(),
    };
    if nums.len() != 4 {
        return Err(util::invalid(spec, "bbox", "expected [lamin, lomin, lamax, lomax]"));
    }
    let b = [nums[0], nums[1], nums[2], nums[3]];
    if b[0] > b[2] || b[1] > b[3] || b[0] < -90.0 || b[2] > 90.0 || b[1] < -180.0 || b[3] > 180.0 {
        return Err(util::invalid(spec, "bbox", "expected lamin ≤ lamax in -90..90 and lomin ≤ lomax in -180..180"));
    }
    Ok(Some(BBox(b)))
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(BlitzSource { bbox: parse_bbox(spec)?, max_rate: util::param_f64_in(spec, "max_rate", 30.0, 0.0, 1e6)? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real frame captured from wss://ws1.blitzortung.org/ (2026-10-14).
    const FRAME: &str = "{\u{22}time\u{22}:17920274548990\u{111}700,\u{22}lat\u{106}38.\u{10a}477\u{11a}lon\u{106}-\u{113}.763393\u{11a}al\u{11e}:\u{119}\u{22}pol\u{106}\u{13a}mds\u{106}59\u{110}\u{11a}mcg\u{106}186\u{11a}st\u{11d}u\u{143}\u{107}\u{11a}regi\u{129}\u{13f}\u{150}i\u{14b}:[\u{100}\u{151}a\u{14c}\u{108}\u{13a}\u{102}\u{104}\u{14c}43584\u{13a}\u{11c}\u{138}\u{120}\u{122}071\u{16d}\u{127}\u{15c}:-1\u{115}.\u{16f}568\u{135}\u{137}\u{14c}9\u{183}\u{150}\u{152}t\u{154}\u{106}2},\u{163}\u{152}\u{18e}\u{16e}\u{168}\u{103}\u{105}\u{107}9\u{117}2\u{178}\u{127}\u{11d}\u{106}41\u{12e}375\u{12f}\u{17a}\u{12a}\u{17c}\u{133}.6\u{10c}\u{170}2\u{185}\u{138}2\u{133}\u{18a}\u{153}\u{155}4\u{190}\u{192}\u{165}\u{107}1\u{16e}\u{11a}\u{169}\u{198}2\u{131}6\u{1b3}\u{1b0}\u{11b}\u{19f}:\u{174}\u{112}80\u{14d}\u{1a8}\u{12b}\u{17e}7.\u{1a6}\u{182}\u{113}\u{1b1}\u{187}\u{1cd}\u{1b5}\u{18c}\u{155}\u{18f}\u{191}\u{22}\u{164}\u{166}8\u{126}\u{101}\u{197}\u{18e}4\u{183}09\u{184}\u{1c7}\u{138}42.\u{1e7}\u{178}\u{1ee}\u{1cf}\u{1aa}\u{1ef}\u{1a4}0834\u{1d7}:285\u{1da}\u{18d}\u{1fd}\u{1b9}\u{1df}\u{193}\u{107}0\u{189}\u{1e4}\u{16a}\u{1fd}4\u{133}\u{16e}\u{1fb}\u{1ec}\u{11f}9.\u{176}\u{178}\u{179}\u{11b}\u{17b}\u{17d}\u{1f8}\u{1d3}\u{118}6\u{177}\u{1fc}1\u{170}\u{134}\u{205}\u{1b6}\u{18e}\u{204}\u{1e0}\u{20c}\u{131}\u{1bf}\u{1e5}\u{1fd}\u{1c4}66\u{114}\u{19e}\u{1ed}4.\u{17e}\u{1ff}\u{112}\u{1f4}\u{12c}3.\u{19c}\u{10c}\u{10e}\u{1fc}\u{1a4}\u{210}\u{164}\u{1db}\u{1a0}\u{227}\u{206}\u{1bd}\u{132}\u{22b}\u{20b}2\u{183}\u{1c4}\u{115}\u{232}\u{1a0}0\u{1f0}\u{183}\u{116}\u{239}\u{114}\u{1ac}\u{188}72\u{210}\u{136}\u{138}19\u{242}\u{18b}\u{202}\u{1b8}\u{1de}\u{228}\u{17e}\u{10c}\u{24a}\u{1c1}\u{19a}\u{1c2}\u{117}\u{250}:\u{1e7}\u{175}\u{19c}\u{25a}\u{239}\u{267}\u{12e}\u{16f}\u{14e}\u{14f}\u{22}\u{25d}\u{144}\u{14e}\u{201}\u{155}\u{17e}\u{246}\u{1bb}\u{167}\u{196}\u{20b}3\u{1cd}\u{20d}\u{21f}\u{211}\u{26f}\u{252}8\u{25f}8\u{1fe}\u{239}\u{112}\u{1ac}\u{19c}\u{16d}\u{223}\u{27b}\u{1fd}\u{217}\u{243}\u{263}\u{281}\u{18e}\u{286}\u{1e3}\u{1c0}\u{11f}05\u{1c2}\u{260}\u{26e}\u{10f}\u{1d3}\u{1d4}\u{24c}\u{256}\u{23b}\u{10b}91\u{21e}\u{240}0\u{223}\u{29a}\u{27f}\u{1dd}\u{1ba}\u{14c}5\u{2a2}\u{269}\u{11f}\u{178}\u{16e}6\u{200}\u{28a}4\u{234}\u{10e}\u{28d}\u{1c2}\u{256}\u{1a2}55\u{132}\u{2c9}\u{1fc}\u{1b3}\u{1eb}\u{2b4}\u{14c}\u{2b6}\u{224}\u{14c}\u{10e}\u{223}\u{2a0}\u{1c9}\u{1fe}3\u{17e}1\u{26e}\u{286}\u{23c}\u{1fe}\u{183}9\u{256}\u{1d2}\u{14e}\u{145}\u{23f}\u{27a}\u{186}\u{20c}\u{279}\u{2d0}\u{107}\u{2d2}\u{266}\u{10e}\u{1c6}\u{2d7}3\u{2d9}\u{1ee}\u{1c6}\u{172}\u{2a1}\u{2df}\u{1e2}\u{1ce}\u{218}\u{1a9}\u{12c}\u{2e4}\u{2bf}\u{10a}\u{2e2}\u{2e8}\u{1b2}\u{22a}\u{224}\u{244}\u{2ed}\u{29c}\u{1fd}\u{267}\u{2bb}\u{1c9}5\u{25f}\u{10c}\u{2f6}\u{1c8}\u{2de}\u{221}\u{145}\u{1d9}\u{2fc}\u{12b}\u{146}\u{1f0}\u{178}\u{2a2}\u{2c0}\u{297}\u{170}\u{27e}\u{2d1}\u{309}\u{17e}\u{1f3}\u{20a}\u{198}\u{1a1}\u{221}\u{117}\u{223}\u{2f7}\u{1c9}\u{23b}\u{170}\u{2c2}\u{146}\u{274}1\u{2c8}\u{259}\u{109}\u{1fc}\u{124}\u{261}\u{225}\u{203}\u{265}\u{247}\u{230}\u{2dc}\u{325}\u{1a0}\u{178}\u{1ad}\u{2b2}\u{2dd}\u{234}\u{22f}\u{2a3}\u{1a7}\u{317}\u{17c}\u{332}\u{1ef}\u{2ca}\u{1ee}\u{305}\u{297}\u{1bd}\u{10f}\u{320}\u{26f}\u{309}\u{1b3}7\u{1eb}\u{2d7}\u{1ee}\u{2af}\u{1fa}\u{171}\u{312}5\u{1f0}\u{20d}\u{22f}\u{302}\u{128}\u{2fd}\u{14e}\u{235}\u{357}\u{1c6}\u{297}\u{131}\u{2b3}\u{262}\u{2b5}\u{309}\u{2e1}\u{30c}\u{16c}\u{132}\u{1a1}\u{2c0}\u{32b}\u{174}2\u{19c}\u{25f}\u{363}\u{219}\u{16f}\u{180}\u{108}\u{1a1}\u{296}\u{2e9}\u{272}\u{353}\u{280}\u{33b}\u{1bb}\u{2da}\u{348}\u{359}\u{16d}\u{112}\u{147}\u{28a}\u{174}\u{146}\u{1ad}\u{290}\u{349}-\u{37d}\u{2bf}\u{1a5}\u{2f3}\u{2cd}\u{22f}\u{353}\u{25b}\u{355}\u{370}\u{33f}\u{26f}\u{111}\u{19a}\u{1e2}\u{2a6}\u{121}\u{2c2}\u{1af}\u{32a}\u{37c}\u{213}\u{131}\u{1fe}\u{1b4}\u{303}\u{18e}\u{348}\u{2ec}\u{264}\u{2b7}\u{1fd}\u{1b3}\u{33e}\u{359}\u{1a6}\u{115}\u{3a3}\u{2c1}\u{28c}\u{1e2}\u{1a6}\u{3a8}\u{365}\u{23b}8\u{3c2}\u{1ea}\u{31d}\u{382}\u{39a}\u{306}\u{202}1\u{2ee}\u{247}\u{37a}\u{371}\u{130}\u{25a}\u{31f}\u{2c1}\u{1a2}\u{20d}\u{108}\u{3ba}\u{364}\u{12b}\u{1f9}\u{12e}\u{10b}\u{31c}\u{220}\u{335}\u{3c8}\u{27f}0\u{322}\u{183}\u{2f1}\u{22c}\u{20d}\u{2ae}\u{2d9}\u{2a6}\u{35f}\u{1a1}\u{2f0}\u{1e3}\u{3d7}\u{17c}\u{3d9}\u{28d}\u{208}\u{2da}\u{3dd}\u{1e3}\u{2ec}\u{3ca}\u{322}\u{3d5}\u{30c}\u{2b9}\u{1a1}\u{3ac}\u{344}\u{214}\u{260}\u{183}\u{1eb}\u{3ee}\u{394}\u{23b}\u{133}\u{3f2}\u{25c}\u{2e9}3\u{3c7}\u{3f6}\u{3e1}\u{386}\u{18e}\u{14e}\u{210}\u{2d7}\u{1a6}\u{1f9}\u{14e}\u{311}\u{173}\u{35f}\u{2b2}\u{414}\u{291}\u{28c}\u{1a4}\u{1e3}\u{297}\u{378}\u{3f5}\u{36d}\u{245}\u{40e}\u{107}\u{3c2}\u{358}\u{22c}6\u{178}\u{10e}\u{3d1}\u{376}\u{35f}\u{22f}\u{10d}\u{249}\u{393}\u{109}.\u{1fa}\u{24d}\u{381}\u{25e}7\u{36c}\u{339}\u{3b2}\u{2d3}\u{1bc}\u{392}\u{2d7}6\u{1a1}\u{1cc}\u{2ca}\u{2a6}\u{1ef}\u{183}\u{221}\u{120}\u{239}\u{125}\u{434}\u{109}\u{19c}\u{302}\u{31e}\u{10d}\u{39b}\u{309}\u{1ea}\u{3e4}\u{20b}\u{439}\u{170}\u{10e}\u{35d}\u{1ed}\u{1a2}\u{1ea}\u{19a}\u{3ca}\u{44a}\u{405}\u{133}\u{124}\u{41e}\u{2e9}\u{324}\u{2ec}\u{18f}]\u{11a}de\u{11c}y\u{11f}\u{122}\u{1a8}c\u{15d}\u{1ec}\u{472}\u{139}}";

    #[test]
    fn decodes_captured_frame() {
        let txt = decode(FRAME).expect("decode");
        let v: Json = serde_json::from_str(&txt).expect("decoded frame is JSON");
        assert!(v.get("lat").is_some() && v.get("lon").is_some() && v.get("time").is_some());
        let s = parse_strike(&v).unwrap();
        assert!((s.lat - 38.92477).abs() < 1e-6);
        assert!((s.lon + 99.763393).abs() < 1e-6);
        assert_eq!(s.stations, 28);
        assert_eq!(s.polarity, 0.0);
        assert_eq!(s.mds, Some(5954.0));
        assert_eq!(s.delay_s, Some(3.9));
        assert!((s.intensity() - 0.7).abs() < 1e-9);
        // Receipt time right after the strike → delay from timestamps; far away → server delay.
        let strike_unix = s.time_ns / 1e9;
        assert!((s.delay_ms(strike_unix + 2.5) - 2500.0).abs() < 1.0);
        assert!((s.delay_ms(strike_unix + 7200.0) - 3900.0).abs() < 1e-6);
        let a = s.attrs(strike_unix + 1.0).into_map();
        assert!((a["lon01"].as_f64().unwrap() - (80.236607 / 360.0)).abs() < 1e-6);
        assert_eq!(s.cell(), (25, 16));
    }

    #[test]
    fn decode_is_identity_on_plain_text_and_tolerates_junk() {
        assert_eq!(decode("{\"a\":1}").unwrap(), "{\"a\":1}");
        assert_eq!(decode(""), None);
        // Dictionary reference before any entry exists → f + c rule.
        let s = String::from_utf16(&[b'a' as u16, 256, b'b' as u16]).unwrap();
        assert_eq!(decode(&s).unwrap(), "aaab");
        assert!(decode("\u{1234}\u{5678}").is_some());
    }

    #[test]
    fn stats_bbox_and_factory() {
        let mut st = StrikeStats::default();
        let mk = |lat: f64, lon: f64| Strike { lat, lon, time_ns: 0.0, polarity: 0.0, stations: 10, mds: None, mcg: None, delay_s: None };
        for i in 0..20 {
            st.push(i as f64 * 0.5, &mk(10.0 + i as f64, 20.0));
        }
        let r: std::collections::BTreeMap<_, _> = st.readings(10.0).into_iter().collect();
        assert!((r["rate"] - 2.0).abs() < 1e-9);
        assert_eq!(r["active"], 4.0); // lats 10..29 → cells 20..23
        let v = st.viz(10.0);
        assert_eq!(v["strikes"].as_array().unwrap().len(), 10);
        assert!(st.keep(10.0, 0.0));

        let b = BBox([30.0, -130.0, 50.0, -60.0]);
        assert!(b.contains(38.9, -99.7));
        assert!(!b.contains(10.0, -99.7));
        assert!(parse_bbox(&SourceSpec::new("x")).unwrap().is_none());
        assert_eq!(parse_bbox(&SourceSpec::new("x").with("bbox", "30,-130,50,-60")).unwrap(), Some(b));
        assert!(parse_bbox(&SourceSpec::new("x").with("bbox", vec![1, 2, 3])).is_err());
        assert!(parse_bbox(&SourceSpec::new("x").with("bbox", vec![50, 0, 30, 10])).is_err());
        assert!(factory(&SourceSpec::new("blitzortung.lightning")).is_ok());
        assert!(parse_strike(&json!({"lat": 95.0, "lon": 0.0})).is_none());
    }

    /// `cargo test -p sonder-sources -- --ignored blitzortung`
    #[tokio::test]
    #[ignore]
    async fn live_strikes_decode() {
        let (mut ws, _) = tokio_tungstenite::connect_async(HOSTS[0]).await.unwrap();
        ws.send(Message::text(HELLO)).await.unwrap();
        let mut strikes = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while strikes.len() < 5 && std::time::Instant::now() < deadline {
            let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout(Duration::from_secs(15), ws.next()).await else { break };
            let d = decode(t.as_str()).unwrap();
            let v: Json = serde_json::from_str(&d).expect("frame decodes to JSON");
            strikes.push(parse_strike(&v).expect("strike has lat/lon"));
        }
        eprintln!("got {} strikes; first: {:?}", strikes.len(), strikes.first());
        assert!(!strikes.is_empty());
        let s = &strikes[0];
        eprintln!("delay now: {:.0} ms, attrs: {:?}", s.delay_ms(util::unix_now()), s.attrs(util::unix_now()).into_map());
    }
}
