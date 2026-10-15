//! The `camera.jpeg` source adapter: a still-image URL polled every few seconds.
//!
//! Thousands of public traffic cameras publish a JPEG that refreshes every 30 s or so
//! rather than a video stream — e.g. Caltrans
//! `https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv102i580westofsr24/tv102i580westofsr24.jpg`
//! (the district list `https://cwwp2.dot.ca.gov/data/d4/cctv/cctvStatusD04.json` has the
//! rest under `data[].cctv.imageData.static.currentImageURL`).
//!
//! Each poll fetches the image, skips it if the bytes are unchanged (hash), decodes it
//! with [`crate::decode_gray`] and runs the [`Tracker`] — in `motion` mode by default,
//! differencing against a slowly adapting background. With tens of seconds between frames
//! every vehicle is "new", so the useful outputs are the `motion` energy, the blob count
//! and one `blob` event per detected vehicle (tracks that *entered* this frame; there are
//! no meaningful exits or crossings). Attrs, readings and viz match `camera.mjpeg`:
//!
//! * event `blob` per entered blob: `x, y, size, area, brightness, speed, dir, lane, duration, id`
//!   (via [`crate::events::blob_sample`]);
//! * readings per new frame: `motion`, `blobs`, `mean_speed`, `mean_brightness`, `fps`
//!   (frames per second actually analysed — fractions of a frame per second here);
//! * a `Viz` payload per new frame: `{"blobs":[{x,y,w,h,b,id}], "line", "w", "h", "motion"}`.

use sonder_core::{Sample, Seconds, SourceSpec, SourceState};
use sonder_sources::util::{self, hash_of};
use sonder_sources::{Emitter, Factory, ParamInfo, Poll, Poller, Registry, Source, SourceCtx, SourceError, SourceFuture, SourceInfo};
use std::time::Duration;

use crate::decode::Decoder;
use crate::events::blob_sample;
use crate::tracker::{FrameAnalysis, Mode, Tracker, TrackerConfig};

/// Source kind name.
pub const KIND: &str = "camera.jpeg";

/// HTTP timeout for one image fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest image we will download.
const MAX_BYTES: usize = 16 << 20;

/// Describe the source for the catalog.
pub fn info() -> SourceInfo {
    SourceInfo {
        kind: KIND,
        summary: "Still-image camera (JPEG URL polled): motion energy, blob count, a `blob` event per vehicle in each new frame",
        params: vec![
            ParamInfo { name: "url", summary: "http(s)://… JPEG still (e.g. a Caltrans cctv image)", default: None },
            ParamInfo { name: "interval", summary: "poll interval (s); unchanged images are skipped", default: Some("10") },
            ParamInfo { name: "mode", summary: "\"motion\" (frame difference) or \"bright\" (headlights at night)", default: Some("motion") },
            ParamInfo { name: "threshold", summary: "0..1 frame-difference (motion) or brightness (bright) threshold", default: Some("0.25 motion / 0.6 bright") },
            ParamInfo { name: "lanes", summary: "x is quantized into this many lanes for the `lane` attr", default: Some("4") },
            ParamInfo { name: "downscale", summary: "integer decode downscale factor (1..=16)", default: Some("4") },
            ParamInfo { name: "min_blob", summary: "minimum blob area in (downscaled) pixels", default: Some("4") },
            ParamInfo { name: "background_alpha", summary: "motion mode: background learning rate per frame", default: Some("0.1") },
        ],
        channels: vec!["motion", "blobs", "mean_speed", "mean_brightness", "fps"],
        events: vec!["blob"],
        network: true,
    }
}

/// `(info, factory)` for registering with a [`Registry`].
pub fn factory() -> (SourceInfo, Factory) {
    (info(), create)
}

/// Register `camera.jpeg` in a registry.
pub fn register(r: &mut Registry) {
    let (i, f) = factory();
    r.register(i, f);
}

fn create(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(JpegPollSource { params: JpegPollParams::from_spec(spec)? }))
}

/// Parsed parameters of a `camera.jpeg` source.
#[derive(Debug, Clone, PartialEq)]
pub struct JpegPollParams {
    pub url: String,
    pub interval: Duration,
    pub tracker: TrackerConfig,
    pub lanes: u32,
}

impl JpegPollParams {
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, SourceError> {
        let url = sonder_sources::require_str(spec, "url")?.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(util::invalid(spec, "url", "expected http(s)://…"));
        }
        let mode = match spec.get_str("mode") {
            None => Mode::Motion,
            Some(s) => Mode::parse(s).ok_or_else(|| util::invalid(spec, "mode", "expected \"motion\" or \"bright\""))?,
        };
        let default_thr = match mode {
            Mode::Bright => 0.6,
            Mode::Motion => 0.25,
        };
        let tracker = TrackerConfig {
            mode,
            threshold: util::param_f64_in(spec, "threshold", default_thr, 0.0, 1.0)? as f32,
            min_blob_px: util::param_f64_in(spec, "min_blob", 4.0, 1.0, 100_000.0)? as u32,
            downscale: util::param_f64_in(spec, "downscale", 4.0, 1.0, 16.0)? as u32,
            background_alpha: util::param_f64_in(spec, "background_alpha", 0.1, 0.0, 1.0)? as f32,
            // Stills tens of seconds apart: no crossing line, report blobs on their first frame.
            line: None,
            confirm_frames: 1,
            max_missed: 0,
            ..Default::default()
        };
        Ok(Self {
            url,
            interval: util::param_secs(spec, "interval", 10.0, 1.0)?,
            tracker,
            lanes: util::param_f64_in(spec, "lanes", 4.0, 1.0, 64.0)? as u32,
        })
    }
}

/// The `camera.jpeg` source.
#[derive(Debug)]
pub struct JpegPollSource {
    params: JpegPollParams,
}

impl JpegPollSource {
    pub fn new(params: JpegPollParams) -> Self {
        Self { params }
    }
}

impl Source for JpegPollSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(run(self.params, ctx))
    }
}

/// Per-source analysis state, kept across polls.
pub struct FrameState {
    decoder: Decoder,
    gray: Vec<u8>,
    tracker: Tracker,
    /// Result of the most recent analysed frame.
    pub analysis: FrameAnalysis,
    last_hash: Option<u64>,
    last_frame_at: Option<Seconds>,
    /// Smoothed analysed-frames-per-second (fractions of a frame per second for stills).
    pub fps: f64,
}

impl FrameState {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            decoder: Decoder::new(),
            gray: Vec::new(),
            tracker: Tracker::new(cfg),
            analysis: FrameAnalysis::default(),
            last_hash: None,
            last_frame_at: None,
            fps: 0.0,
        }
    }

    /// Analyse one fetched image into `self.analysis`. `Ok(false)` when the bytes are
    /// unchanged since last time (nothing analysed).
    pub fn ingest(&mut self, jpeg: &[u8], now: Seconds) -> Result<bool, String> {
        let h = hash_of(&jpeg);
        if self.last_hash == Some(h) {
            return Ok(false);
        }
        let downscale = self.tracker.config().downscale.max(1);
        let (w, hgt) = self.decoder.decode_into(jpeg, downscale, &mut self.gray).map_err(|e| format!("jpeg decode failed: {e}"))?;
        self.last_hash = Some(h);
        self.tracker.process_into(&self.gray, w, hgt, now, &mut self.analysis);
        if let Some(prev) = self.last_frame_at {
            let dt = now - prev;
            if dt > 0.0 {
                let inst = 1.0 / dt;
                self.fps = if self.fps == 0.0 { inst } else { self.fps + (inst - self.fps) * 0.3 };
            }
        }
        self.last_frame_at = Some(now);
        Ok(true)
    }
}

/// Samples for one analysed frame: a `blob` event per entered blob, a reading and a viz payload.
pub fn frame_samples(ctx: &SourceCtx, now: Seconds, a: &FrameAnalysis, fps: f64, lanes: u32) -> Vec<Sample> {
    let mut out = Vec::with_capacity(a.entered.len() + 2);
    for b in &a.entered {
        out.push(blob_sample(&ctx.id, now, "blob", b, lanes));
    }
    let mean_speed = if a.blobs.is_empty() { 0.0 } else { a.blobs.iter().map(|b| b.speed as f64).sum::<f64>() / a.blobs.len() as f64 };
    out.push(Sample::reading(
        &ctx.id,
        now,
        [
            ("motion", a.motion_energy as f64),
            ("blobs", a.blobs.len() as f64),
            ("mean_speed", mean_speed),
            ("mean_brightness", a.mean_brightness as f64),
            ("fps", fps),
        ],
    ));
    let blobs: Vec<serde_json::Value> =
        a.blobs.iter().map(|b| serde_json::json!({ "x": b.x, "y": b.y, "w": b.w, "h": b.h, "b": b.brightness, "id": b.id })).collect();
    out.push(Sample::viz(
        &ctx.id,
        now,
        serde_json::json!({ "blobs": blobs, "line": serde_json::Value::Null, "w": a.width, "h": a.height, "motion": a.motion_energy }),
    ));
    out
}

/// GET the image bytes.
pub async fn fetch(ctx: &SourceCtx, url: &str) -> Result<Vec<u8>, String> {
    let resp = ctx
        .http
        .get(url)
        .header(reqwest::header::USER_AGENT, util::USER_AGENT)
        .header(reqwest::header::ACCEPT, "image/jpeg, image/*, */*")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", util::short_err(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        return Err("image too large".into());
    }
    let bytes = resp.bytes().await.map_err(|e| format!("body read failed: {}", util::short_err(&e)))?;
    if bytes.len() > MAX_BYTES {
        return Err("image too large".into());
    }
    if bytes.is_empty() {
        return Err("empty body".into());
    }
    Ok(bytes.to_vec())
}

async fn run(p: JpegPollParams, ctx: SourceCtx) {
    let mut em = Emitter::new(ctx);
    let mut state = FrameState::new(p.tracker.clone());
    let mut poller = Poller::new(p.interval);
    poller.start(&mut em).await;
    loop {
        let res = poll_once(&mut em, &p, &mut state).await;
        if !poller.after(&mut em, res).await {
            return;
        }
    }
}

async fn poll_once(em: &mut Emitter, p: &JpegPollParams, state: &mut FrameState) -> Result<Poll, String> {
    let mut shutdown = em.ctx.shutdown.clone();
    let jpeg = tokio::select! {
        r = fetch(&em.ctx, &p.url) => r?,
        _ = async { while !*shutdown.borrow() { if shutdown.changed().await.is_err() { break } } } => return Ok(Poll::Idle),
    };
    let now = em.now();
    if !state.ingest(&jpeg, now)? {
        return Ok(Poll::Idle);
    }
    let samples = frame_samples(&em.ctx, now, &state.analysis, state.fps, p.lanes);
    for s in samples {
        em.send(s).await;
    }
    if em.closed {
        return Ok(Poll::Idle);
    }
    em.status(SourceState::Live, "live").await;
    Ok(Poll::Live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;
    use sonder_core::Datum;
    use std::sync::Arc;

    fn ctx(clock: Arc<dyn sonder_core::Clock>) -> (SourceCtx, tokio::sync::mpsc::Receiver<Sample>, tokio::sync::watch::Sender<bool>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4096);
        let (stx, srx) = tokio::sync::watch::channel(false);
        (SourceCtx { id: "cam".into(), tx, clock, http: reqwest::Client::new(), shutdown: srx, seed: 1 }, rx, stx)
    }

    #[test]
    fn params_parse_and_registry() {
        let p = JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "https://x/y.jpg")).unwrap();
        assert_eq!(p.tracker.mode, Mode::Motion);
        assert_eq!(p.tracker.threshold, 0.25);
        assert_eq!(p.tracker.line, None);
        assert_eq!(p.tracker.confirm_frames, 1);
        assert_eq!(p.interval, Duration::from_secs(10));
        let p = JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "http://x/y.jpg").with("mode", "bright").with("interval", 30).with("lanes", 3)).unwrap();
        assert_eq!(p.tracker.mode, Mode::Bright);
        assert_eq!(p.tracker.threshold, 0.6);
        assert_eq!(p.lanes, 3);
        assert!(matches!(JpegPollParams::from_spec(&SourceSpec::new(KIND)), Err(SourceError::MissingParam { .. })));
        assert!(JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "ftp://x")).is_err());
        assert!(JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "http://x").with("interval", 0.1)).is_err());
        assert!(JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "http://x").with("mode", "x")).is_err());

        let mut r = Registry::empty();
        register(&mut r);
        assert!(r.info(KIND).is_some());
        assert!(r.create(&SourceSpec::new(KIND).with("url", "http://x/y.jpg")).is_ok());
        assert!(r.create(&SourceSpec::new(KIND)).is_err());
    }

    /// Frame 1 primes the background; frame 2 (same) is skipped by hash; frame 3 with a
    /// vehicle yields motion, one blob and one `blob` event.
    #[test]
    fn ingest_dedups_and_detects_new_vehicle() {
        let (w, h) = (160u32, 120u32);
        let empty = testgen::encode_jpeg(&testgen::render(w, h, 90, 0, &[], 1), w, h, 90).unwrap();
        let car = testgen::encode_jpeg(&testgen::render(w, h, 90, 0, &[testgen::Disc { x: 40.0, y: 60.0, r: 8.0, brightness: 240 }], 1), w, h, 90).unwrap();
        let p = JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", "http://x/y.jpg").with("downscale", 1)).unwrap();
        let mut st = FrameState::new(p.tracker.clone());
        assert!(st.ingest(&empty, 0.0).unwrap());
        assert!(!st.ingest(&empty, 10.0).unwrap(), "identical bytes must be skipped");
        assert!(st.ingest(&car, 20.0).unwrap());
        let a = st.analysis.clone();
        assert!(a.motion_energy > 0.0);
        assert_eq!(a.blobs.len(), 1, "{a:?}");
        assert_eq!(a.entered.len(), 1);
        let (c, _, _) = ctx(Arc::new(sonder_core::VirtualClock::new()));
        let samples = frame_samples(&c, 20.0, &a, st.fps, 4);
        assert_eq!(samples.len(), 3);
        match &samples[0].datum {
            Datum::Event { name, attrs, .. } => {
                assert_eq!(name, "blob");
                assert_eq!(attrs["lane"].as_f64(), Some(1.0));
            }
            d => panic!("{d:?}"),
        }
        assert!(matches!(&samples[1].datum, Datum::Reading { values } if values["blobs"] == 1.0 && values["fps"] > 0.0));
        assert!(matches!(&samples[2].datum, Datum::Viz { payload } if payload["blobs"].as_array().unwrap().len() == 1));
        assert!(st.ingest(b"not a jpeg", 30.0).is_err());
    }

    /// End to end against an in-process HTTP server that serves a changing still.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn polls_server_and_stops_promptly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (w, h) = (96u32, 64u32);
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut n = 0u32;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                n += 1;
                let discs = if n.is_multiple_of(2) { vec![testgen::Disc { x: 20.0 + n as f32 * 5.0, y: 32.0, r: 6.0, brightness: 240 }] } else { vec![] };
                let jpeg = testgen::encode_jpeg(&testgen::render(w, h, 90, 0, &discs, n as u64), w, h, 90).unwrap();
                tokio::spawn(async move {
                    let mut tmp = [0u8; 4096];
                    let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut tmp)).await;
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", jpeg.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&jpeg).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        let (c, mut rx, stop) = ctx(Arc::new(sonder_core::WallClock::new()));
        let spec = SourceSpec::new(KIND).with("url", format!("http://{addr}/still.jpg")).with("interval", 1).with("downscale", 1);
        let src = Box::new(JpegPollSource::new(JpegPollParams::from_spec(&spec).unwrap()));
        let task = tokio::spawn(src.run(c));
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let t0 = std::time::Instant::now();
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("stops on shutdown").unwrap();
        assert!(t0.elapsed() < Duration::from_millis(1500));
        server.abort();
        let (mut readings, mut viz, mut blobs, mut live) = (0, 0, 0, false);
        while let Ok(s) = rx.try_recv() {
            match s.datum {
                Datum::Reading { values } => {
                    assert!(values.contains_key("motion"));
                    readings += 1;
                }
                Datum::Viz { .. } => viz += 1,
                Datum::Event { name, .. } => {
                    assert_eq!(name, "blob");
                    blobs += 1;
                }
                Datum::Status { state, .. } => live |= state == SourceState::Live,
                Datum::Audio { .. } => {}
            }
        }
        assert!(live);
        assert!(readings >= 3, "readings {readings}");
        assert_eq!(viz, readings);
        assert!(blobs >= 1, "blobs {blobs}");
    }

    /// `cargo test -p sonder-vision -- --ignored jpeg_poll`
    #[tokio::test]
    #[ignore]
    async fn live_caltrans_still() {
        let url = "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv102i580westofsr24/tv102i580westofsr24.jpg";
        let (c, _rx, _stop) = ctx(Arc::new(sonder_core::WallClock::new()));
        let jpeg = fetch(&c, url).await.unwrap();
        let p = JpegPollParams::from_spec(&SourceSpec::new(KIND).with("url", url)).unwrap();
        let mut st = FrameState::new(p.tracker.clone());
        assert!(st.ingest(&jpeg, 0.0).unwrap());
        let a = st.analysis.clone();
        eprintln!("{} bytes → {}x{} (downscaled), mean brightness {:.3}", jpeg.len(), a.width, a.height, a.mean_brightness);
        assert!(a.width > 0 && a.height > 0);
        assert!(!st.ingest(&jpeg, 1.0).unwrap());
        // Second fetch ~0 s later is normally identical; a changed image exercises the tracker.
        let again = fetch(&c, url).await.unwrap();
        if st.ingest(&again, 2.0).unwrap() {
            let a = &st.analysis;
            eprintln!("second frame: motion {:.4}, {} blobs, {} entered", a.motion_energy, a.blobs.len(), a.entered.len());
        } else {
            eprintln!("second fetch unchanged (expected within the 30 s refresh)");
        }
    }
}
