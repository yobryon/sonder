//! `audio.features` — spectral features of raw PCM audio from stdin, a file or a FIFO.
//!
//! Feed it mono PCM, typically from ffmpeg:
//!
//! ```text
//! ffmpeg -i <hls url or mic> -ac 1 -ar 48000 -f s16le - | sonder play scene.toml
//! ```
//!
//! # Orcasound (live hydrophones, Salish Sea)
//!
//! The feed list is <https://live.orcasound.net/api/json/feeds>; each entry's
//! `attributes.node_name` (e.g. `rpi_orcasound_lab`, `rpi_bush_point`, `rpi_port_townsend`,
//! `rpi_sunset_bay`) and `attributes.bucket` (`audio-orcasound-net`) build the HLS URL. The
//! player first reads `https://{bucket}.s3.amazonaws.com/{node_name}/latest.txt` (a Unix
//! timestamp naming the current stream segment directory) and then plays
//! `https://{bucket}.s3.amazonaws.com/{node_name}/hls/{timestamp}/live.m3u8`:
//!
//! ```text
//! NODE=rpi_orcasound_lab; B=https://audio-orcasound-net.s3.amazonaws.com/$NODE
//! ffmpeg -loglevel warning -i "$B/hls/$(curl -s $B/latest.txt)/live.m3u8" \
//!        -ac 1 -ar 48000 -f s16le - | sonder play scene.toml
//! ```
//!
//! # Features
//!
//! Per 50 ms hop over a Hann window of 2048 samples — or the next power of two above the hop
//! when that is longer (4096 at 48 kHz), so consecutive frames overlap rather than leave
//! gaps: `rms` (dBFS mapped −60..0 → 0..1), `peak`,
//! `centroid` (Hz), `flux` (half-wave-rectified spectral flux), `zcr` (zero-crossing rate
//! 0..1), `low`/`mid`/`high` (energy fractions below 250 Hz, 250–2000 Hz, above 2 kHz).
//! Readings at 10 Hz. Events: `onset` when flux exceeds median + k·MAD of the last 2 s
//! (refractory 100 ms; attrs `centroid`, `centroid01`, `flux`, `rms`), `loud` when `rms`
//! rises through 0.6. The reader runs on a detached thread so a blocked `read` on stdin
//! never holds up runtime shutdown.

use std::collections::VecDeque;

use sonder_core::{SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Smallest analysis window; grown to cover the hop (see [`Analyzer::window`]).
pub const MIN_WINDOW: usize = 2048;
pub const HOP_S: f64 = 0.05;
/// MAD multiplier for the adaptive onset threshold.
pub const ONSET_K: f32 = 3.0;
/// Frames of flux history (2 s at 20 frames/s).
pub const HISTORY: usize = 40;
const ONSET_MIN_HISTORY: usize = 10;
const FLUX_FLOOR: f32 = 1e-3;
const ONSET_RMS_GATE: f32 = 0.05;
const LOUD_LEVEL: f32 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    S16le,
    F32le,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s.trim().to_ascii_lowercase().as_str() {
            "s16le" | "s16" | "pcm_s16le" => Some(Format::S16le),
            "f32le" | "f32" | "pcm_f32le" | "float" => Some(Format::F32le),
            _ => None,
        }
    }
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Format::S16le => 2,
            Format::F32le => 4,
        }
    }
}

/// Converts interleaved PCM bytes into mono `f32` samples, carrying partial frames across calls.
#[derive(Debug)]
pub struct PcmDecoder {
    format: Format,
    channels: usize,
    rest: Vec<u8>,
}

impl PcmDecoder {
    pub fn new(format: Format, channels: usize) -> Self {
        Self { format, channels: channels.max(1), rest: Vec::new() }
    }
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
        self.rest.extend_from_slice(bytes);
        let frame = self.format.bytes_per_sample() * self.channels;
        let whole = self.rest.len() / frame * frame;
        let inv = 1.0 / self.channels as f32;
        for f in self.rest[..whole].chunks_exact(frame) {
            let mut acc = 0.0f32;
            for s in f.chunks_exact(self.format.bytes_per_sample()) {
                acc += match self.format {
                    Format::S16le => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                    Format::F32le => {
                        let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                        if v.is_finite() { v.clamp(-4.0, 4.0) } else { 0.0 }
                    }
                };
            }
            out.push(acc * inv);
        }
        self.rest.drain(..whole);
    }
}

/// Features of one analysis frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Frame {
    pub index: u64,
    pub rms: f32,
    pub rms_db: f32,
    pub peak: f32,
    pub centroid: f32,
    pub flux: f32,
    pub zcr: f32,
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
    Frame(Frame),
    Onset { intensity: f32, frame: Frame },
    Loud { frame: Frame },
}

/// In-place iterative radix-2 FFT (`n` a power of two).
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

/// Streaming feature extractor; pure and deterministic so it can be unit-tested offline.
#[derive(Debug)]
pub struct Analyzer {
    rate: f32,
    hop: usize,
    win: usize,
    ring: Vec<f32>,
    filled: usize,
    since_hop: usize,
    hann: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    mag: Vec<f32>,
    prev_mag: Vec<f32>,
    flux_hist: VecDeque<f32>,
    frames: u64,
    last_onset: Option<u64>,
    prev_rms: f32,
}

impl Analyzer {
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(1000) as f32;
        let hop = ((rate as f64 * HOP_S).round() as usize).max(1);
        let win = hop.max(MIN_WINDOW).next_power_of_two();
        let hann: Vec<f32> = (0..win).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / win as f32).cos()).collect();
        Self {
            rate,
            hop,
            win,
            ring: vec![0.0; win],
            filled: 0,
            since_hop: 0,
            hann,
            re: vec![0.0; win],
            im: vec![0.0; win],
            mag: vec![0.0; win / 2 + 1],
            prev_mag: vec![0.0; win / 2 + 1],
            flux_hist: VecDeque::with_capacity(HISTORY + 1),
            frames: 0,
            last_onset: None,
            prev_rms: 0.0,
        }
    }

    /// Samples per hop.
    pub fn hop(&self) -> usize {
        self.hop
    }
    /// Analysis window length in samples.
    pub fn window(&self) -> usize {
        self.win
    }

    /// Feed samples; returns the frames (and events) completed.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<Out>) {
        for &s in samples {
            // `ring` is a shift register indexed from the oldest sample at `filled % win`.
            let pos = self.filled % self.win;
            self.ring[pos] = if s.is_finite() { s } else { 0.0 };
            self.filled += 1;
            self.since_hop += 1;
            if self.since_hop >= self.hop {
                self.since_hop = 0;
                self.analyze(out);
            }
        }
    }

    fn analyze(&mut self, out: &mut Vec<Out>) {
        let win = self.win;
        let start = self.filled % win;
        let mut sum_sq = 0.0f32;
        let mut peak = 0.0f32;
        let mut crossings = 0u32;
        let mut prev = 0.0f32;
        for i in 0..win {
            let x = self.ring[(start + i) % win];
            sum_sq += x * x;
            peak = peak.max(x.abs());
            if i > 0 && (x > 0.0) != (prev > 0.0) && (x != 0.0 || prev != 0.0) {
                crossings += 1;
            }
            prev = x;
            self.re[i] = x * self.hann[i];
            self.im[i] = 0.0;
        }
        fft(&mut self.re, &mut self.im);
        let bins = win / 2 + 1;
        let hz_per_bin = self.rate / win as f32;
        let (mut num, mut den) = (0.0f32, 0.0f32);
        let (mut low, mut mid, mut high) = (0.0f32, 0.0f32, 0.0f32);
        let mut flux = 0.0f32;
        for k in 0..bins {
            let m = (self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt();
            self.mag[k] = m;
            let f = k as f32 * hz_per_bin;
            num += f * m;
            den += m;
            let e = m * m;
            if f < 250.0 {
                low += e;
            } else if f < 2000.0 {
                mid += e;
            } else {
                high += e;
            }
            flux += (m - self.prev_mag[k]).max(0.0);
        }
        std::mem::swap(&mut self.mag, &mut self.prev_mag);
        flux /= win as f32;
        let total = low + mid + high;
        let frac = |e: f32| if total > 1e-12 { e / total } else { 0.0 };
        let rms_lin = (sum_sq / win as f32).sqrt();
        let rms_db = if rms_lin > 1e-9 { 20.0 * rms_lin.log10() } else { -120.0 };
        let frame = Frame {
            index: self.frames,
            rms: ((rms_db + 60.0) / 60.0).clamp(0.0, 1.0),
            rms_db,
            peak: peak.min(1.0),
            centroid: if den > 1e-9 { num / den } else { 0.0 },
            flux,
            zcr: crossings as f32 / (win - 1) as f32,
            low: frac(low),
            mid: frac(mid),
            high: frac(high),
        };
        out.push(Out::Frame(frame));

        // Adaptive onset threshold: median + k·MAD of recent flux.
        if self.flux_hist.len() >= ONSET_MIN_HISTORY {
            let mut v: Vec<f32> = self.flux_hist.iter().copied().collect();
            v.sort_by(f32::total_cmp);
            let median = v[v.len() / 2];
            let mut dev: Vec<f32> = v.iter().map(|x| (x - median).abs()).collect();
            dev.sort_by(f32::total_cmp);
            let mad = dev[dev.len() / 2];
            let thr = (median + ONSET_K * mad).max(FLUX_FLOOR);
            let refractory_frames = (0.1 / HOP_S).round() as u64;
            let free = self.last_onset.is_none_or(|l| self.frames - l >= refractory_frames);
            if flux > thr && frame.rms > ONSET_RMS_GATE && free {
                self.last_onset = Some(self.frames);
                let intensity = (flux / (thr * 3.0)).clamp(0.1, 1.0);
                out.push(Out::Onset { intensity, frame });
            }
        }
        self.flux_hist.push_back(flux);
        while self.flux_hist.len() > HISTORY {
            self.flux_hist.pop_front();
        }
        if self.prev_rms < LOUD_LEVEL && frame.rms >= LOUD_LEVEL {
            out.push(Out::Loud { frame });
        }
        self.prev_rms = frame.rms;
        self.frames += 1;
    }

    /// Log-frequency position of `hz` between 50 Hz and Nyquist, 0..1.
    pub fn centroid01(&self, hz: f32) -> f64 {
        let lo = 50.0f64;
        let hi = (self.rate as f64 / 2.0).max(lo * 2.0);
        clamp01(((hz as f64).max(lo) / lo).log2() / (hi / lo).log2())
    }
}

impl Frame {
    pub fn readings(&self) -> [(&'static str, f64); 8] {
        [
            ("rms", self.rms as f64),
            ("peak", self.peak as f64),
            ("centroid", self.centroid as f64),
            ("flux", self.flux as f64),
            ("zcr", self.zcr as f64),
            ("low", self.low as f64),
            ("mid", self.mid as f64),
            ("high", self.high as f64),
        ]
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub path: String,
    pub format: Format,
    pub rate: u32,
    pub channels: usize,
}

enum Chunk {
    Data(Vec<f32>),
    End,
    Err(String),
}

/// Read `path` (or stdin for `-`) on a detached thread, decoding to mono f32.
fn spawn_reader(cfg: &Config) -> tokio::sync::mpsc::Receiver<Chunk> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Chunk>(64);
    let cfg = cfg.clone();
    let spawned = std::thread::Builder::new().name("sonder-audio".into()).spawn(move || {
        let mut reader: Box<dyn std::io::Read> = if cfg.path == "-" {
            Box::new(std::io::stdin())
        } else {
            match std::fs::File::open(&cfg.path) {
                Ok(f) => Box::new(f),
                Err(e) => {
                    let _ = tx.blocking_send(Chunk::Err(format!("cannot open {}: {e}", cfg.path)));
                    return;
                }
            }
        };
        let mut dec = PcmDecoder::new(cfg.format, cfg.channels);
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.blocking_send(Chunk::End);
                    return;
                }
                Ok(n) => {
                    let mut samples = Vec::with_capacity(n / 2);
                    dec.push(&buf[..n], &mut samples);
                    if !samples.is_empty() && tx.blocking_send(Chunk::Data(samples)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = tx.blocking_send(Chunk::Err(format!("read error: {e}")));
                    return;
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("cannot start audio reader thread: {e}");
    }
    rx
}

#[cfg(unix)]
fn is_fifo(path: &str) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).map(|m| m.file_type().is_fifo()).unwrap_or(false)
}
#[cfg(not(unix))]
fn is_fifo(_path: &str) -> bool {
    false
}

struct AudioSource {
    cfg: Config,
}

impl Source for AudioSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut an = Analyzer::new(self.cfg.rate);
            let reading_every = ((0.1 / HOP_S).round() as u64).max(1);
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            let reopen = self.cfg.path != "-" && is_fifo(&self.cfg.path);
            loop {
                let what = if self.cfg.path == "-" { "stdin".to_string() } else { self.cfg.path.clone() };
                em.status_force(SourceState::Connecting, format!("waiting for audio on {what}")).await;
                let mut rx = spawn_reader(&self.cfg);
                let mut out = Vec::new();
                let end: Result<(), String> = loop {
                    let chunk = tokio::select! {
                        c = rx.recv() => c,
                        _ = em.ctx.wait_shutdown() => return,
                    };
                    match chunk {
                        Some(Chunk::Data(samples)) => {
                            attempt = 0;
                            em.status(SourceState::Live, "live").await;
                            out.clear();
                            an.push(&samples, &mut out);
                            for o in &out {
                                match *o {
                                    Out::Frame(f) => {
                                        if f.index.is_multiple_of(reading_every) {
                                            em.reading(f.readings()).await;
                                        }
                                    }
                                    Out::Onset { intensity, frame } => {
                                        let attrs = Attrs::new()
                                            .num("centroid", frame.centroid as f64)
                                            .num("centroid01", an.centroid01(frame.centroid))
                                            .num("flux", frame.flux as f64)
                                            .num("rms", frame.rms as f64);
                                        em.event("onset", intensity as f64, attrs.into_map()).await;
                                    }
                                    Out::Loud { frame } => {
                                        let attrs = Attrs::new().num("rms", frame.rms as f64).num("peak", frame.peak as f64);
                                        em.event("loud", frame.rms as f64, attrs.into_map()).await;
                                    }
                                }
                            }
                            if em.closed {
                                return;
                            }
                        }
                        Some(Chunk::End) | None => break Ok(()),
                        Some(Chunk::Err(e)) => break Err(e),
                    }
                };
                drop(rx);
                if !reopen {
                    match end {
                        Ok(()) => em.status(SourceState::Idle, "audio stream ended").await,
                        Err(e) => em.status(SourceState::Failed, e).await,
                    }
                    em.ctx.wait_shutdown().await;
                    return;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = end.err().unwrap_or_else(|| "writer closed the pipe".into());
                em.status_force(SourceState::Reconnecting, format!("{msg}; reopening in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "audio.features",
        summary: "Spectral features of raw PCM on stdin/file/FIFO (pipe ffmpeg in): rms, centroid, flux, bands; `onset`/`loud` events",
        params: vec![
            ParamInfo { name: "path", summary: "\"-\" for stdin, or a file / FIFO path (FIFOs are reopened when the writer leaves)", default: Some("-") },
            ParamInfo { name: "format", summary: "s16le | f32le", default: Some("s16le") },
            ParamInfo { name: "rate", summary: "Sample rate (Hz)", default: Some("48000") },
            ParamInfo { name: "channels", summary: "Interleaved channels (mixed down to mono)", default: Some("1") },
        ],
        channels: vec!["rms", "peak", "centroid", "flux", "zcr", "low", "mid", "high"],
        events: vec!["onset", "loud"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let format = Format::parse(&util::param_str(spec, "format", "s16le")).ok_or_else(|| util::invalid(spec, "format", "expected s16le | f32le"))?;
    let path = util::param_str(spec, "path", "-").trim().to_string();
    if path.is_empty() {
        return Err(util::invalid(spec, "path", "empty"));
    }
    Ok(Box::new(AudioSource {
        cfg: Config {
            path,
            format,
            rate: util::param_f64_in(spec, "rate", 48000.0, 1000.0, 384_000.0)? as u32,
            channels: util::param_f64_in(spec, "channels", 1.0, 1.0, 32.0)? as usize,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(an: &mut Analyzer, samples: &[f32]) -> Vec<Out> {
        let mut out = Vec::new();
        for c in samples.chunks(1000) {
            an.push(c, &mut out);
        }
        out
    }

    #[test]
    fn fft_matches_dft_on_small_input() {
        let n = 16;
        let x: Vec<f32> = (0..n).map(|i| ((i * 7 % 5) as f32 - 2.0) * 0.3).collect();
        let (mut re, mut im) = (x.clone(), vec![0.0f32; n]);
        fft(&mut re, &mut im);
        for k in 0..n {
            let (mut r, mut i) = (0.0f32, 0.0f32);
            for (t, xt) in x.iter().enumerate() {
                let a = -2.0 * std::f32::consts::PI * (k * t) as f32 / n as f32;
                r += xt * a.cos();
                i += xt * a.sin();
            }
            assert!((r - re[k]).abs() < 1e-4 && (i - im[k]).abs() < 1e-4, "bin {k}");
        }
    }

    #[test]
    fn sine_centroid_rms_zcr_and_bands() {
        let rate = 48000u32;
        let f = 1000.0f32;
        let sig: Vec<f32> = (0..rate).map(|i| 0.5 * (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin()).collect();
        let mut an = Analyzer::new(rate);
        assert_eq!(an.hop(), 2400);
        assert_eq!(an.window(), 4096);
        assert_eq!(Analyzer::new(16000).window(), 2048);
        let out = run(&mut an, &sig);
        let frames: Vec<Frame> = out.iter().filter_map(|o| if let Out::Frame(f) = o { Some(*f) } else { None }).collect();
        assert_eq!(frames.len(), 20);
        let f = frames[frames.len() - 1];
        assert!((f.centroid - 1000.0).abs() < 50.0, "centroid {}", f.centroid);
        // amplitude 0.5 sine: rms 0.354 → −9 dBFS → 0.85
        assert!((f.rms - 0.85).abs() < 0.02, "rms {}", f.rms);
        assert!((f.peak - 0.5).abs() < 0.01);
        assert!((f.zcr - 2.0 * 1000.0 / 48000.0).abs() < 0.003, "zcr {}", f.zcr);
        assert!(f.mid > 0.95 && f.low < 0.03 && f.high < 0.03, "{f:?}");
        // Steady tone: no onsets after the first frames, one `loud` crossing.
        let onsets = out.iter().filter(|o| matches!(o, Out::Onset { .. })).count();
        let louds = out.iter().filter(|o| matches!(o, Out::Loud { .. })).count();
        assert_eq!(onsets, 0, "{out:?}");
        assert_eq!(louds, 1);
        assert!((an.centroid01(50.0)).abs() < 1e-9 && (an.centroid01(24000.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn click_train_yields_onsets_at_the_right_rate() {
        let rate = 48000u32;
        let secs = 4;
        let period = rate as usize / 4; // 4 clicks per second
        let mut sig = vec![0.0f32; rate as usize * secs];
        for (i, s) in sig.iter_mut().enumerate() {
            // tiny noise floor so MAD is not exactly zero
            *s = ((i * 2654435761 % 1000) as f32 / 1000.0 - 0.5) * 1e-3;
            if i % period < 20 {
                *s = 0.9;
            }
        }
        let mut an = Analyzer::new(rate);
        let out = run(&mut an, &sig);
        let onsets: Vec<u64> = out.iter().filter_map(|o| if let Out::Onset { frame, .. } = o { Some(frame.index) } else { None }).collect();
        // 16 clicks; the first 0.5 s (10 frames) is warm-up → ≥ 12 expected, never more than 16.
        assert!((12..=16).contains(&onsets.len()), "onsets {onsets:?}");
        // Clicks are 5 hops apart.
        for w in onsets.windows(2) {
            assert_eq!(w[1] - w[0], 5, "{onsets:?}");
        }
        // Digital silence produces nothing.
        let mut an = Analyzer::new(rate);
        let out = run(&mut an, &vec![0.0f32; rate as usize * 2]);
        assert!(out.iter().all(|o| matches!(o, Out::Frame(_))));
    }

    #[test]
    fn pcm_decoder_carries_partial_frames() {
        let mut d = PcmDecoder::new(Format::S16le, 2);
        let mut out = Vec::new();
        // L=16384 (0.5), R=-16384 (-0.5) → mono 0; then L=R=8192 → 0.25; split across pushes.
        let bytes: Vec<u8> = [16384i16, -16384, 8192, 8192].iter().flat_map(|v| v.to_le_bytes()).collect();
        d.push(&bytes[..3], &mut out);
        assert!(out.is_empty());
        d.push(&bytes[3..], &mut out);
        assert_eq!(out, vec![0.0, 0.25]);
        let mut d = PcmDecoder::new(Format::F32le, 1);
        let mut out = Vec::new();
        d.push(&0.75f32.to_le_bytes(), &mut out);
        d.push(&f32::NAN.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0.75, 0.0]);
        assert_eq!(Format::parse("S16LE"), Some(Format::S16le));
        assert!(Format::parse("u8").is_none());
        assert!(factory(&SourceSpec::new("audio.features")).is_ok());
        assert!(factory(&SourceSpec::new("audio.features").with("format", "mp3")).is_err());
        assert!(factory(&SourceSpec::new("audio.features").with("rate", 10)).is_err());
    }

    /// Full source reading a PCM file: readings and a `loud` event, then Idle; stops on shutdown.
    #[tokio::test]
    async fn source_reads_file_and_stops() {
        use sonder_core::Datum;
        let dir = std::env::temp_dir().join(format!("sonder-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.s16le");
        let rate = 48000u32;
        let bytes: Vec<u8> = (0..rate)
            .map(|i| (0.8 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin() * 32767.0) as i16)
            .flat_map(|v| v.to_le_bytes())
            .collect();
        std::fs::write(&path, bytes).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: "a".into(),
            tx,
            clock: std::sync::Arc::new(sonder_core::WallClock::new()),
            http: crate::default_http_client(),
            shutdown: srx,
            seed: 1,
        };
        let src = factory(&SourceSpec::new("audio.features").with("path", path.to_string_lossy().to_string())).unwrap();
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        stx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("stops on shutdown").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (mut readings, mut loud, mut idle) = (0, 0, false);
        while let Ok(s) = rx.try_recv() {
            match s.datum {
                Datum::Reading { values } => {
                    assert!(values.contains_key("centroid"));
                    readings += 1;
                }
                Datum::Event { name, .. } if name == "loud" => loud += 1,
                Datum::Status { state: SourceState::Idle, .. } => idle = true,
                _ => {}
            }
        }
        assert_eq!(readings, 10);
        assert_eq!(loud, 1);
        assert!(idle);
    }
}
