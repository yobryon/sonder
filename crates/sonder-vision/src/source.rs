//! The `camera.mjpeg` source adapter.
//!
//! Connects to an MJPEG URL (or reads stdin), frames JPEGs, decodes and tracks
//! them on a dedicated worker thread, and emits:
//!
//! * `blob` events when a track ends (a vehicle has left the frame) and
//!   `crossing` events when a track crosses the configured line, with attrs
//!   `x, y, size, area, brightness, speed, dir, lane, duration, id`;
//! * readings at 4 Hz: `motion`, `blobs`, `mean_speed`, `mean_brightness`, `fps`;
//! * a `Viz` payload at ≤10 Hz: `{"blobs":[{x,y,w,h,b,id}], "line", "w", "h"}`;
//! * `Status` transitions, with exponential backoff reconnects.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::StreamExt;
use sonder_core::{Clock, Sample, Seconds, SourceSpec, SourceState};
use sonder_sources::{Factory, ParamInfo, Registry, Source, SourceCtx, SourceError, SourceFuture, SourceInfo};

use crate::decode::Decoder;
use crate::mjpeg::FrameStream;
use crate::tracker::{FrameAnalysis, Mode, Tracker, TrackerConfig};

/// Source kind name.
pub const KIND: &str = "camera.mjpeg";

/// Per-chunk read timeout: no bytes for this long means the camera is gone.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
/// HTTP connect / header timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF: f64 = 60.0;

/// Describe the source for the catalog.
pub fn info() -> SourceInfo {
    SourceInfo {
        kind: KIND,
        summary: "MJPEG camera over HTTP or stdin: motion energy, bright/moving blob tracking, crossing events",
        params: vec![
            ParamInfo {
                name: "url",
                summary: "http(s)://… MJPEG stream, or \"-\" to read stdin (e.g. ffmpeg -f mjpeg -)",
                default: None,
            },
            ParamInfo {
                name: "mode",
                summary: "\"bright\" (headlights at night) or \"motion\" (frame difference)",
                default: Some("bright"),
            },
            ParamInfo {
                name: "threshold",
                summary: "0..1 brightness (bright) or frame-difference (motion) threshold",
                default: Some("0.6 bright / 0.25 motion"),
            },
            ParamInfo {
                name: "line",
                summary: "normalized y of the crossing line; false/null disables",
                default: Some("0.5"),
            },
            ParamInfo {
                name: "min_blob",
                summary: "minimum blob area in (downscaled) pixels",
                default: Some("4"),
            },
            ParamInfo {
                name: "downscale",
                summary: "integer decode downscale factor (1..=16)",
                default: Some("4"),
            },
            ParamInfo {
                name: "fps_cap",
                summary: "maximum frames per second to analyse",
                default: Some("15"),
            },
            ParamInfo {
                name: "reconnect",
                summary: "reconnect with backoff when the stream drops",
                default: Some("true"),
            },
            ParamInfo {
                name: "lanes",
                summary: "x is quantized into this many lanes for the `lane` attr",
                default: Some("4"),
            },
            ParamInfo {
                name: "merge_dist",
                summary: "merge blobs closer than this many pixels (headlight pairs)",
                default: Some("6"),
            },
            ParamInfo {
                name: "max_blobs",
                summary: "keep at most this many blobs per frame",
                default: Some("64"),
            },
            ParamInfo {
                name: "background_alpha",
                summary: "motion mode: background learning rate per frame",
                default: Some("0.02"),
            },
        ],
        channels: vec!["motion", "blobs", "mean_speed", "mean_brightness", "fps"],
        events: vec!["blob", "crossing"],
        network: true,
    }
}

/// `(info, factory)` for registering with a [`Registry`].
pub fn factory() -> (SourceInfo, Factory) {
    (info(), create)
}

/// Register `camera.mjpeg` in a registry.
pub fn register(r: &mut Registry) {
    let (i, f) = factory();
    r.register(i, f);
}

fn create(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let params = CameraParams::from_spec(spec)?;
    Ok(Box::new(CameraSource { params }))
}

/// Parsed parameters of a `camera.mjpeg` source.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraParams {
    pub url: String,
    pub tracker: TrackerConfig,
    pub fps_cap: f64,
    pub reconnect: bool,
    pub lanes: u32,
    pub readings_hz: f64,
    pub viz_hz: f64,
}

fn invalid(spec: &SourceSpec, param: &'static str, message: impl Into<String>) -> SourceError {
    SourceError::InvalidParam {
        kind: spec.kind.clone(),
        param,
        message: message.into(),
    }
}

fn f64_param(spec: &SourceSpec, name: &'static str, range: std::ops::RangeInclusive<f64>, default: f64) -> Result<f64, SourceError> {
    match spec.params.get(name) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .ok_or_else(|| invalid(spec, name, "expected a number"))?;
            if !n.is_finite() || !range.contains(&n) {
                return Err(invalid(spec, name, format!("{n} out of range {}..={}", range.start(), range.end())));
            }
            Ok(n)
        }
    }
}

fn bool_param(spec: &SourceSpec, name: &'static str, default: bool) -> Result<bool, SourceError> {
    match spec.params.get(name) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(serde_json::Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Ok(true),
            "false" | "no" | "0" | "off" => Ok(false),
            _ => Err(invalid(spec, name, "expected true/false")),
        },
        Some(serde_json::Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0) != 0.0),
        Some(_) => Err(invalid(spec, name, "expected true/false")),
    }
}

impl CameraParams {
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, SourceError> {
        let url = sonder_sources::require_str(spec, "url")?.trim().to_string();
        if url.is_empty() {
            return Err(invalid(spec, "url", "empty"));
        }
        if url != "-" && !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(invalid(spec, "url", "expected http(s)://… or \"-\" for stdin"));
        }
        let mode = match spec.get_str("mode") {
            None => Mode::Bright,
            Some(s) => Mode::parse(s).ok_or_else(|| invalid(spec, "mode", "expected \"bright\" or \"motion\""))?,
        };
        let default_thr = match mode {
            Mode::Bright => 0.6,
            Mode::Motion => 0.25,
        };
        let threshold = f64_param(spec, "threshold", 0.0..=1.0, default_thr)? as f32;
        let line = match spec.params.get("line") {
            None => Some(0.5),
            Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => None,
            Some(serde_json::Value::Bool(true)) => Some(0.5),
            Some(serde_json::Value::String(s)) if matches!(s.trim().to_ascii_lowercase().as_str(), "none" | "off" | "" | "false") => None,
            Some(_) => Some(f64_param(spec, "line", 0.0..=1.0, 0.5)? as f32),
        };
        let min_blob_px = f64_param(spec, "min_blob", 1.0..=100_000.0, 4.0)? as u32;
        let downscale = f64_param(spec, "downscale", 1.0..=16.0, 4.0)? as u32;
        let fps_cap = f64_param(spec, "fps_cap", 0.1..=240.0, 15.0)?;
        let reconnect = bool_param(spec, "reconnect", true)?;
        let lanes = f64_param(spec, "lanes", 1.0..=64.0, 4.0)? as u32;
        let merge_dist = f64_param(spec, "merge_dist", 0.0..=1000.0, 6.0)? as f32;
        let max_blobs = f64_param(spec, "max_blobs", 1.0..=1024.0, 64.0)? as usize;
        let background_alpha = f64_param(spec, "background_alpha", 0.0..=1.0, 0.02)? as f32;
        let tracker = TrackerConfig {
            threshold,
            min_blob_px,
            max_blobs,
            downscale,
            mode,
            line,
            background_alpha,
            merge_dist,
            ..Default::default()
        };
        Ok(Self {
            url,
            tracker,
            fps_cap,
            reconnect,
            lanes,
            readings_hz: 4.0,
            viz_hz: 10.0,
        })
    }
}

/// The `camera.mjpeg` source.
#[derive(Debug)]
pub struct CameraSource {
    params: CameraParams,
}

impl CameraSource {
    pub fn new(params: CameraParams) -> Self {
        Self { params }
    }
}

impl Source for CameraSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(run(self.params, ctx))
    }
}

/// Messages from the worker thread to the emitter task.
enum WorkerMsg {
    Frame(FrameAnalysis),
    DecodeError(String),
}

/// Spawn the decode + track thread. Frames arrive on a bounded channel; the
/// network side drops frames when the worker is behind.
fn spawn_worker(
    frames: std::sync::mpsc::Receiver<Bytes>,
    results: tokio::sync::mpsc::Sender<WorkerMsg>,
    cfg: TrackerConfig,
    clock: Arc<dyn Clock>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let downscale = cfg.downscale.max(1);
    std::thread::Builder::new().name("sonder-vision".into()).spawn(move || {
        let mut decoder = Decoder::new();
        let mut gray = Vec::new();
        let mut tracker = Tracker::new(cfg);
        let mut out = FrameAnalysis::default();
        for jpeg in frames {
            match decoder.decode_into(&jpeg, downscale, &mut gray) {
                Ok((w, h)) => {
                    let t = clock.now();
                    tracker.process_into(&gray, w, h, t, &mut out);
                    if results.blocking_send(WorkerMsg::Frame(std::mem::take(&mut out))).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // Don't let a flood of bad frames block; drop if the emitter is busy.
                    let _ = results.try_send(WorkerMsg::DecodeError(e.to_string()));
                }
            }
        }
    })
}

/// Perceptual size of a blob in 0..1 from its frame-area fraction.
use crate::events::blob_sample;

fn blob_event(ctx: &SourceCtx, at: Seconds, name: &str, b: &crate::tracker::Blob, lanes: u32) -> Sample {
    blob_sample(&ctx.id, at, name, b, lanes)
}

/// Consume analyses from the worker and turn them into samples.
async fn emit_loop(ctx: SourceCtx, mut rx: tokio::sync::mpsc::Receiver<WorkerMsg>, p: CameraParams) {
    let reading_period = 1.0 / p.readings_hz.max(0.1);
    let viz_period = 1.0 / p.viz_hz.max(0.1);
    let mut last_reading = f64::NEG_INFINITY;
    let mut last_viz = f64::NEG_INFINITY;
    let mut frames_since_reading = 0u32;
    let mut fps = 0.0f64;
    let mut decode_errors = 0u64;
    let mut latest: Option<FrameAnalysis> = None;
    while let Some(msg) = rx.recv().await {
        let a = match msg {
            WorkerMsg::Frame(a) => a,
            WorkerMsg::DecodeError(e) => {
                decode_errors += 1;
                if decode_errors == 1 || decode_errors.is_multiple_of(100) {
                    tracing::warn!(source = %ctx.id, errors = decode_errors, "jpeg decode failed: {e}");
                }
                continue;
            }
        };
        let now = ctx.now();
        frames_since_reading += 1;
        for b in &a.exited {
            if !ctx.emit(blob_event(&ctx, now, "blob", b, p.lanes)).await {
                return;
            }
        }
        for b in &a.crossings {
            if !ctx.emit(blob_event(&ctx, now, "crossing", b, p.lanes)).await {
                return;
            }
        }
        if now - last_reading >= reading_period {
            if last_reading.is_finite() {
                let dt = now - last_reading;
                if dt > 0.0 {
                    let inst = frames_since_reading as f64 / dt;
                    fps = if fps == 0.0 { inst } else { fps + (inst - fps) * 0.3 };
                }
            }
            frames_since_reading = 0;
            last_reading = now;
            let mean_speed = if a.blobs.is_empty() {
                0.0
            } else {
                a.blobs.iter().map(|b| b.speed as f64).sum::<f64>() / a.blobs.len() as f64
            };
            let values: Vec<(&str, f64)> = vec![
                ("motion", a.motion_energy as f64),
                ("blobs", a.blobs.len() as f64),
                ("mean_speed", mean_speed),
                ("mean_brightness", a.mean_brightness as f64),
                ("fps", fps),
            ];
            if !ctx.emit(Sample::reading(&ctx.id, now, values)).await {
                return;
            }
        }
        if now - last_viz >= viz_period {
            last_viz = now;
            let blobs: Vec<serde_json::Value> = a
                .blobs
                .iter()
                .map(|b| serde_json::json!({ "x": b.x, "y": b.y, "w": b.w, "h": b.h, "b": b.brightness, "id": b.id }))
                .collect();
            let payload = serde_json::json!({
                "blobs": blobs,
                "line": p.tracker.line,
                "w": a.width,
                "h": a.height,
                "motion": a.motion_energy,
            });
            if !ctx.emit(Sample::viz(&ctx.id, now, payload)).await {
                return;
            }
        }
        latest = Some(a);
    }
    drop(latest);
}

/// Why a connection ended.
enum End {
    /// Shutdown requested.
    Shutdown,
    /// Upstream closed cleanly.
    Eof,
    /// Engine channel closed.
    EngineGone,
}

/// Tiny deterministic jitter source for backoff.
struct Jitter(u64);
impl Jitter {
    fn next(&mut self) -> f64 {
        let mut x = self.0 | 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

async fn status(ctx: &SourceCtx, state: SourceState, msg: impl Into<String>) -> bool {
    ctx.emit(Sample::status(&ctx.id, ctx.now(), state, msg)).await
}

async fn run(p: CameraParams, ctx: SourceCtx) {
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Bytes>(2);
    let (res_tx, res_rx) = tokio::sync::mpsc::channel::<WorkerMsg>(32);
    let worker = match spawn_worker(frame_rx, res_tx, p.tracker.clone(), ctx.clock.clone()) {
        Ok(h) => h,
        Err(e) => {
            status(&ctx, SourceState::Failed, format!("cannot start vision worker thread: {e}")).await;
            return;
        }
    };
    let emitter = tokio::spawn(emit_loop(ctx.clone(), res_rx, p.clone()));

    let mut jitter = Jitter(ctx.seed ^ 0xA5A5_5A5A_1234_5678);
    let mut attempt: u32 = 0;
    let mut shutdown = ctx.shutdown.clone();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if !status(&ctx, SourceState::Connecting, format!("connecting to {}", p.url)).await {
            break;
        }
        let outcome = run_connection(&p, &ctx, &frame_tx, &mut shutdown, &mut attempt).await;
        match outcome {
            Ok(End::Shutdown) | Ok(End::EngineGone) => break,
            Ok(End::Eof) => {
                if p.url == "-" {
                    status(&ctx, SourceState::Idle, "stdin closed").await;
                    break;
                }
                if !p.reconnect {
                    status(&ctx, SourceState::Failed, "stream ended").await;
                    break;
                }
            }
            Err(msg) => {
                if p.url == "-" || !p.reconnect {
                    status(&ctx, SourceState::Failed, msg).await;
                    break;
                }
                tracing::warn!(source = %ctx.id, "camera.mjpeg: {msg}");
            }
        }
        attempt = attempt.saturating_add(1);
        let base = (2f64.powi(attempt.min(10) as i32 - 1)).min(MAX_BACKOFF);
        let delay = base * (0.75 + 0.5 * jitter.next());
        if !status(
            &ctx,
            SourceState::Reconnecting,
            format!("reconnecting in {delay:.0}s (attempt {attempt})"),
        )
        .await
        {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(delay)) => {}
            _ = wait_shutdown(&mut shutdown) => break,
        }
    }
    drop(frame_tx);
    let _ = emitter.await;
    // The worker ends on its own once the frame channel closes. It is a plain
    // `std::thread`, deliberately not joined: joining through `spawn_blocking`
    // would make `Runtime::drop` wait on a decode that may still be running.
    drop(worker);
}

/// Read `r` on a detached `std::thread`, forwarding chunks as a `Stream`.
///
/// `tokio::io::stdin()` reads on the runtime's blocking pool, and a read that
/// never returns (nobody closes our stdin) then blocks `Runtime::drop`, i.e.
/// process exit. A detached thread blocked in `read` simply dies with the
/// process; dropping the returned stream is enough to abandon it.
fn detached_reader_stream<R: std::io::Read + Send + 'static>(
    mut r: R,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let spawned = std::thread::Builder::new().name("sonder-vision-stdin".into()).spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("cannot start stdin reader thread: {e}");
    }
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

async fn wait_shutdown(rx: &mut tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// One connection: open the stream, pump frames to the worker until it ends.
/// `attempt` is reset once frames flow. `Err` carries a human-readable reason.
async fn run_connection(
    p: &CameraParams,
    ctx: &SourceCtx,
    frame_tx: &std::sync::mpsc::SyncSender<Bytes>,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
    attempt: &mut u32,
) -> Result<End, String> {
    let mut stream = if p.url == "-" {
        FrameStream::from_stream(detached_reader_stream(std::io::stdin()), None)
    } else {
        let req = ctx
            .http
            .get(&p.url)
            .header(reqwest::header::ACCEPT, "multipart/x-mixed-replace, image/jpeg, */*")
            .send();
        let resp = tokio::select! {
            r = tokio::time::timeout(CONNECT_TIMEOUT, req) => match r {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => return Err(format!("connect failed: {e}")),
                Err(_) => return Err("connect timed out".into()),
            },
            _ = wait_shutdown(shutdown) => return Ok(End::Shutdown),
        };
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        tracing::debug!(source = %ctx.id, content_type = ?ct, "camera.mjpeg connected");
        FrameStream::from_stream(resp.bytes_stream(), ct.as_deref())
    };

    let min_gap = Duration::from_secs_f64(1.0 / p.fps_cap.max(0.1));
    let mut last_accept: Option<Instant> = None;
    let mut received = 0u64;
    let mut dropped = 0u64;
    loop {
        let item = tokio::select! {
            r = tokio::time::timeout(CHUNK_TIMEOUT, stream.next()) => r,
            _ = wait_shutdown(shutdown) => return Ok(End::Shutdown),
        };
        match item {
            Err(_) => return Err(format!("no data for {}s", CHUNK_TIMEOUT.as_secs())),
            Ok(None) => return Ok(End::Eof),
            Ok(Some(Err(e))) => return Err(format!("stream error: {e}")),
            Ok(Some(Ok(jpeg))) => {
                received += 1;
                if received == 1 {
                    *attempt = 0;
                    let st = stream.parser().stats();
                    let how = if st.fast_path > 0 {
                        "multipart (content-length)"
                    } else {
                        "scanning"
                    };
                    if !status(ctx, SourceState::Live, format!("receiving frames ({how})")).await {
                        return Ok(End::EngineGone);
                    }
                }
                let now = Instant::now();
                if let Some(la) = last_accept {
                    if now.duration_since(la) < min_gap {
                        continue;
                    }
                }
                match frame_tx.try_send(jpeg) {
                    Ok(()) => last_accept = Some(now),
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        dropped += 1;
                        if dropped.is_multiple_of(100) {
                            tracing::debug!(source = %ctx.id, dropped, "vision worker behind; dropping frames");
                        }
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Ok(End::EngineGone),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;
    use sonder_core::Datum;

    #[test]
    fn params_parse_and_validate() {
        let spec = SourceSpec::new(KIND).with("url", "-");
        let p = CameraParams::from_spec(&spec).unwrap();
        assert_eq!(p.tracker.mode, Mode::Bright);
        assert_eq!(p.tracker.threshold, 0.6);
        assert_eq!(p.tracker.line, Some(0.5));
        assert_eq!(p.fps_cap, 15.0);
        assert!(p.reconnect);

        let spec = SourceSpec::new(KIND)
            .with("url", "http://cam/stream")
            .with("mode", "motion")
            .with("line", serde_json::Value::Null)
            .with("downscale", 2)
            .with("reconnect", false)
            .with("lanes", 6);
        let p = CameraParams::from_spec(&spec).unwrap();
        assert_eq!(p.tracker.mode, Mode::Motion);
        assert_eq!(p.tracker.threshold, 0.25);
        assert_eq!(p.tracker.line, None);
        assert_eq!(p.tracker.downscale, 2);
        assert!(!p.reconnect);
        assert_eq!(p.lanes, 6);

        assert!(matches!(
            CameraParams::from_spec(&SourceSpec::new(KIND)),
            Err(SourceError::MissingParam { .. })
        ));
        assert!(CameraParams::from_spec(&SourceSpec::new(KIND).with("url", "rtsp://x")).is_err());
        assert!(CameraParams::from_spec(&SourceSpec::new(KIND).with("url", "-").with("threshold", 3.0)).is_err());
        assert!(CameraParams::from_spec(&SourceSpec::new(KIND).with("url", "-").with("mode", "psychic")).is_err());
        assert!(CameraParams::from_spec(&SourceSpec::new(KIND).with("url", "-").with("downscale", 0)).is_err());
    }

    #[test]
    fn registry_creates_the_source() {
        let mut r = Registry::empty();
        register(&mut r);
        assert!(r.info(KIND).is_some());
        assert!(r.create(&SourceSpec::new(KIND).with("url", "-")).is_ok());
        assert!(r.create(&SourceSpec::new(KIND)).is_err());
        let i = info();
        assert!(i.events.contains(&"blob"));
        assert!(i.channels.contains(&"motion"));
    }

    #[test]
    fn blob_event_attrs() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (_stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: "cam".into(),
            tx,
            clock: Arc::new(sonder_core::VirtualClock::new()),
            http: reqwest::Client::new(),
            shutdown: srx,
            seed: 1,
        };
        let b = crate::tracker::Blob {
            id: 7,
            x: 0.8,
            y: 0.9,
            area: 0.005,
            brightness: 0.9,
            speed: 0.3,
            dir: 1.0,
            age_s: 2.5,
            ..Default::default()
        };
        let s = blob_event(&ctx, 1.0, "blob", &b, 4);
        match s.datum {
            Datum::Event { name, intensity, attrs } => {
                assert_eq!(name, "blob");
                assert!(intensity > 0.0 && intensity <= 1.0);
                assert_eq!(attrs["lane"].as_f64(), Some(3.0));
                assert_eq!(attrs["dir"].as_f64(), Some(1.0));
                assert_eq!(attrs["duration"].as_f64(), Some(2.5));
                assert_eq!(attrs["id"].as_f64(), Some(7.0));
            }
            _ => panic!("expected event"),
        }
    }

    /// A reader whose `read` never returns, like an idle stdin.
    struct Forever;
    impl std::io::Read for Forever {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    /// Abandoning the stdin stream must not wait for the blocked read.
    #[tokio::test]
    async fn detached_reader_stream_can_be_dropped_while_blocked() {
        let started = Instant::now();
        let s = detached_reader_stream(Forever);
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s);
        assert!(started.elapsed() < Duration::from_secs(1));
        // And it does forward data from a reader that produces some.
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let s = detached_reader_stream(std::io::Cursor::new(data.clone()));
        let got: Vec<u8> = s.map(|r| r.unwrap().to_vec()).concat().await;
        assert_eq!(got, data);
    }

    /// The full source against a never-ending HTTP MJPEG stream must stop within
    /// about a second of shutdown being signalled, and `run` must return.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stops_promptly_on_shutdown_with_endless_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let frames = testgen::jpeg_sequence(4, 48, 32);
        let server = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let frames = frames.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    // Drain the request line/headers (best effort) then stream forever.
                    let _ = tokio::time::timeout(Duration::from_millis(200), async {
                        let mut tmp = [0u8; 4096];
                        let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut tmp).await;
                    })
                    .await;
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=f\r\nConnection: close\r\n\r\n";
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let mut i = 0usize;
                    loop {
                        buf.clear();
                        buf.extend_from_slice(&testgen::multipart(&frames[i % frames.len()..i % frames.len() + 1], "f", true));
                        buf.truncate(buf.len() - "--f--\r\n".len());
                        if sock.write_all(&buf).await.is_err() {
                            return;
                        }
                        i += 1;
                        tokio::time::sleep(Duration::from_millis(40)).await;
                    }
                });
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: "cam".into(),
            tx,
            clock: Arc::new(sonder_core::WallClock::new()),
            http: reqwest::Client::new(),
            shutdown: srx,
            seed: 1,
        };
        let spec = SourceSpec::new(KIND).with("url", format!("http://{addr}/stream")).with("downscale", 1);
        let src = Box::new(CameraSource::new(CameraParams::from_spec(&spec).unwrap()));
        let task = tokio::spawn(src.run(ctx));
        // Wait until frames are flowing.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut live = false;
        while Instant::now() < deadline && !live {
            if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                live = matches!(s.datum, Datum::Reading { .. } | Datum::Viz { .. });
            }
        }
        assert!(live, "no frames received from the test stream");
        let t0 = Instant::now();
        stx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("source must stop on shutdown").unwrap();
        assert!(t0.elapsed() < Duration::from_millis(1500), "took {:?}", t0.elapsed());
        server.abort();
    }

    /// End-to-end: a multipart body served from an in-process stream (via the
    /// `-` stdin path replaced by a reader) produces events and readings.
    #[tokio::test]
    async fn worker_and_emitter_produce_samples() {
        let (w, h) = (160u32, 120u32);
        // 40 frames: one disc moving down and out of the frame, then empties.
        let mut jpegs = Vec::new();
        for i in 0..40 {
            let y = -6.0 + i as f32 * 4.0;
            let discs = if y < 130.0 {
                vec![testgen::Disc {
                    x: 100.0,
                    y,
                    r: 5.0,
                    brightness: 235,
                }]
            } else {
                vec![]
            };
            let frame = testgen::render(w, h, 18, 8, &discs, i as u64 + 1);
            jpegs.push(testgen::encode_jpeg(&frame, w, h, 85).unwrap());
        }
        let clock = Arc::new(sonder_core::VirtualClock::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
        let (_stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: "cam".into(),
            tx,
            clock: clock.clone(),
            http: reqwest::Client::new(),
            shutdown: srx,
            seed: 1,
        };
        let spec = SourceSpec::new(KIND).with("url", "-").with("downscale", 1).with("line", 0.5);
        let p = CameraParams::from_spec(&spec).unwrap();

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<Bytes>(64);
        let (res_tx, res_rx) = tokio::sync::mpsc::channel::<WorkerMsg>(64);
        let worker = spawn_worker(frame_rx, res_tx, p.tracker.clone(), clock.clone()).unwrap();
        let emitter = tokio::spawn(emit_loop(ctx.clone(), res_rx, p.clone()));
        for (i, j) in jpegs.iter().enumerate() {
            clock.set(i as f64 / 10.0);
            frame_tx.send(Bytes::from(j.clone())).unwrap();
            // let the worker keep up so timestamps are meaningful
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        drop(frame_tx);
        worker.join().unwrap();
        emitter.await.unwrap();
        drop(ctx);

        let mut events = Vec::new();
        let mut readings = 0;
        let mut viz = 0;
        while let Some(s) = rx.recv().await {
            match s.datum {
                Datum::Event { name, attrs, .. } => events.push((name, attrs)),
                Datum::Reading { values } => {
                    readings += 1;
                    assert!(values.contains_key("motion") && values.contains_key("fps"));
                }
                Datum::Viz { payload } => {
                    viz += 1;
                    assert!(payload.get("blobs").is_some());
                }
                Datum::Status { .. } => {}
            }
        }
        let blobs: Vec<_> = events.iter().filter(|(n, _)| n == "blob").collect();
        let crossings: Vec<_> = events.iter().filter(|(n, _)| n == "crossing").collect();
        assert_eq!(blobs.len(), 1, "{events:?}");
        assert_eq!(crossings.len(), 1, "{events:?}");
        assert_eq!(blobs[0].1["dir"].as_f64(), Some(1.0));
        assert_eq!(blobs[0].1["lane"].as_f64(), Some(2.0));
        assert!(readings >= 10, "readings {readings}");
        assert!(viz >= 20, "viz {viz}");
    }
}
