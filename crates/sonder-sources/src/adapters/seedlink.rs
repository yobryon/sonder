//! `seedlink.seismic` — live seismometer waveforms over the FDSN SeedLink protocol
//! (raw TCP, v3 handshake), decoded from miniSEED v2 records (Steim1/Steim2/int16/int32).
//!
//! Records arrive in bursts (a 512-byte Steim2 record of 40 Hz data holds ~14 s), so the
//! adapter *plays each record out in real time*: decoded samples are queued per stream and
//! consumed at the nominal sample rate by a 10 Hz tick, which emits readings, runs the
//! STA/LTA trigger and publishes a waveform `Viz` payload. The music therefore moves
//! continuously, roughly one record behind the ground.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::BytesMut;
use serde_json::{json, Value as Json};
use sonder_core::{Seconds, SourceSpec, SourceState};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

pub const DEFAULT_SERVER: &str = "rtserve.iris.washington.edu:18000";
/// Playout tick (readings cadence).
const TICK: Duration = Duration::from_millis(100);
/// Viz every N ticks (0.5 s).
const VIZ_EVERY: u32 = 5;
/// Reconnect when the server has been silent this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(300);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Stream selection
// ---------------------------------------------------------------------------

/// One NET.STA.LOC.CHA selection. An empty `loc` matches any location code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSel {
    pub net: String,
    pub sta: String,
    pub loc: String,
    pub cha: String,
}

impl StreamSel {
    /// Parse `"NET_STA_LOC_CHA"` (also `.` or `:` separators; `--` or empty = blank location).
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.trim().split(['_', '.', ':']).collect();
        let (net, sta, loc, cha) = match parts.as_slice() {
            [net, sta, loc, cha] => (*net, *sta, *loc, *cha),
            [net, sta, cha] => (*net, *sta, "", *cha),
            [net, sta] => (*net, *sta, "", "BHZ"),
            _ => return None,
        };
        Self::new(net, sta, loc, cha)
    }

    pub fn new(net: &str, sta: &str, loc: &str, cha: &str) -> Option<Self> {
        let clean = |s: &str| s.trim().to_ascii_uppercase();
        let (net, sta, mut loc, cha) = (clean(net), clean(sta), clean(loc), clean(cha));
        if loc == "--" {
            loc.clear();
        }
        let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric());
        if !ok(&net, 2) || !ok(&sta, 5) || !ok(&cha, 3) || loc.len() > 2 || !loc.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(Self { net, sta, loc, cha })
    }

    /// `SELECT` pattern: `LLCCC.D` (blank location → `??`).
    pub fn select_pattern(&self) -> String {
        let loc = if self.loc.is_empty() { "??".to_string() } else { format!("{:>2}", self.loc).replace(' ', '?'.to_string().as_str()) };
        format!("{loc}{}.D", self.cha)
    }

    pub fn matches(&self, h: &MsHeader) -> bool {
        self.net == h.net && self.sta == h.sta && self.cha == h.cha && (self.loc.is_empty() || self.loc == h.loc)
    }

    pub fn id(&self) -> String {
        format!("{}.{}.{}.{}", self.net, self.sta, self.loc, self.cha)
    }
}

// ---------------------------------------------------------------------------
// miniSEED v2
// ---------------------------------------------------------------------------

/// Fixed section + blockette 1000 of a miniSEED v2 record.
#[derive(Debug, Clone, PartialEq)]
pub struct MsHeader {
    pub seq: String,
    pub sta: String,
    pub loc: String,
    pub cha: String,
    pub net: String,
    /// Start time as Unix seconds.
    pub start_unix: f64,
    pub nsamp: usize,
    pub rate_hz: f64,
    /// 1 int16, 3 int32, 4 f32, 5 f64, 10 Steim1, 11 Steim2.
    pub encoding: u8,
    pub data_big_endian: bool,
    pub rec_len: usize,
    pub data_offset: usize,
}

impl MsHeader {
    /// End time (start + nsamp / rate) as Unix seconds.
    pub fn end_unix(&self) -> f64 {
        if self.rate_hz > 0.0 {
            self.start_unix + self.nsamp as f64 / self.rate_hz
        } else {
            self.start_unix
        }
    }
}

fn u16_at(b: &[u8], off: usize, be: bool) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    let arr = [s[0], s[1]];
    Some(if be { u16::from_be_bytes(arr) } else { u16::from_le_bytes(arr) })
}

fn i16_at(b: &[u8], off: usize, be: bool) -> Option<i16> {
    u16_at(b, off, be).map(|v| v as i16)
}

fn u32_at(b: &[u8], off: usize, be: bool) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    let arr = [s[0], s[1], s[2], s[3]];
    Some(if be { u32::from_be_bytes(arr) } else { u32::from_le_bytes(arr) })
}

fn ascii_field(b: &[u8], range: std::ops::Range<usize>) -> String {
    b.get(range).map(|s| String::from_utf8_lossy(s).trim().to_string()).unwrap_or_default()
}

/// Sample rate from the SEED factor/multiplier pair.
pub fn sample_rate(factor: i16, mult: i16) -> f64 {
    let (f, m) = (factor as f64, mult as f64);
    match (factor.signum(), mult.signum()) {
        (0, _) | (_, 0) => 0.0,
        (1, 1) => f * m,
        (1, -1) => -f / m,
        (-1, 1) => -m / f,
        _ => 1.0 / (f * m),
    }
}

/// BTIME → Unix seconds.
pub fn btime_to_unix(year: u16, doy: u16, hour: u8, min: u8, sec: u8, frac_0001: u16) -> Option<f64> {
    let date = chrono::NaiveDate::from_yo_opt(i32::from(year), u32::from(doy))?;
    let dt = date.and_hms_opt(u32::from(hour), u32::from(min), u32::from(sec.min(59)))?;
    Some(dt.and_utc().timestamp() as f64 + f64::from(frac_0001) / 10_000.0)
}

/// Parse the fixed header and the blockette chain (needs at least the first 64 bytes).
pub fn parse_header(rec: &[u8]) -> Result<MsHeader, String> {
    if rec.len() < 48 {
        return Err(format!("record too short ({} bytes)", rec.len()));
    }
    let seq = ascii_field(rec, 0..6);
    let q = rec[6];
    if !matches!(q, b'D' | b'R' | b'Q' | b'M') {
        return Err(format!("not a data record (quality byte {q:#04x})"));
    }
    // Header byte order: pick the one that yields a sane year.
    let year_ok = |be: bool| u16_at(rec, 20, be).is_some_and(|y| (1900..=2200).contains(&y));
    let hbe = if year_ok(true) {
        true
    } else if year_ok(false) {
        false
    } else {
        return Err("implausible start year in both byte orders".into());
    };
    let rd16 = |off| u16_at(rec, off, hbe).ok_or_else(|| "truncated header".to_string());
    let year = rd16(20)?;
    let doy = rd16(22)?;
    let frac = rd16(28)?;
    let start_unix = btime_to_unix(year, doy, rec[24], rec[25], rec[26], frac).ok_or_else(|| format!("bad BTIME {year}-{doy}"))?;
    let nsamp = rd16(30)? as usize;
    let rate_hz = sample_rate(i16_at(rec, 32, hbe).unwrap_or(0), i16_at(rec, 34, hbe).unwrap_or(0));
    let data_offset = rd16(44)? as usize;
    let mut blk = rd16(46)? as usize;

    let mut b1000 = None;
    for _ in 0..32 {
        if blk == 0 || blk + 4 > rec.len() {
            break;
        }
        let typ = u16_at(rec, blk, hbe).unwrap_or(0);
        let next = u16_at(rec, blk + 2, hbe).unwrap_or(0) as usize;
        if typ == 1000 && blk + 7 <= rec.len() {
            b1000 = Some((rec[blk + 4], rec[blk + 5] == 1, rec[blk + 6]));
        }
        if next <= blk {
            break;
        }
        blk = next;
    }
    let (encoding, data_big_endian, len_pow) = b1000.ok_or_else(|| "no blockette 1000".to_string())?;
    let rec_len = if (8..=20).contains(&len_pow) { 1usize << len_pow } else { 512 };
    Ok(MsHeader {
        seq,
        sta: ascii_field(rec, 8..13),
        loc: ascii_field(rec, 13..15),
        cha: ascii_field(rec, 15..18),
        net: ascii_field(rec, 18..20),
        start_unix,
        nsamp,
        rate_hz,
        encoding,
        data_big_endian,
        rec_len,
        data_offset,
    })
}

/// A decoded record.
#[derive(Debug, Clone, PartialEq)]
pub struct MsRecord {
    pub header: MsHeader,
    pub samples: Vec<f64>,
}

/// Parse and decode a complete record.
pub fn parse_record(rec: &[u8]) -> Result<MsRecord, String> {
    let header = parse_header(rec)?;
    let end = header.rec_len.min(rec.len());
    if header.data_offset < 48 || header.data_offset > end {
        return Err(format!("bad data offset {}", header.data_offset));
    }
    let data = &rec[header.data_offset..end];
    let be = header.data_big_endian;
    let n = header.nsamp;
    let samples: Vec<f64> = match header.encoding {
        0 => Vec::new(), // ASCII/log records carry no samples
        1 => (0..n).filter_map(|i| i16_at(data, i * 2, be)).map(f64::from).collect(),
        3 => (0..n).filter_map(|i| u32_at(data, i * 4, be)).map(|v| f64::from(v as i32)).collect(),
        4 => (0..n).filter_map(|i| u32_at(data, i * 4, be)).map(|v| f64::from(f32::from_bits(v))).collect(),
        5 => (0..n)
            .filter_map(|i| data.get(i * 8..i * 8 + 8))
            .map(|s| {
                let mut a = [0u8; 8];
                a.copy_from_slice(s);
                if be { f64::from_be_bytes(a) } else { f64::from_le_bytes(a) }
            })
            .collect(),
        10 => steim_decode(data, n, be, false)?.into_iter().map(f64::from).collect(),
        11 => steim_decode(data, n, be, true)?.into_iter().map(f64::from).collect(),
        e => return Err(format!("unsupported encoding {e}")),
    };
    if header.encoding != 0 && samples.len() != n {
        return Err(format!("decoded {} of {} samples (encoding {})", samples.len(), n, header.encoding));
    }
    Ok(MsRecord { header, samples })
}

// ---------------------------------------------------------------------------
// Steim1 / Steim2
// ---------------------------------------------------------------------------

/// Sign-extend the `width`-bit field at `shift` in `v`.
#[inline]
fn field(v: u32, shift: u32, width: u32) -> i32 {
    ((v >> shift) << (32 - width)) as i32 >> (32 - width)
}

/// Decode Steim1 (`steim2 = false`) or Steim2 data: 64-byte frames of sixteen 32-bit words,
/// word 0 holding sixteen 2-bit nibbles; frame 0 words 1–2 are the forward (x0) and reverse
/// (xn) integration constants. Returns exactly `nsamp` samples or an error.
pub fn steim_decode(data: &[u8], nsamp: usize, be: bool, steim2: bool) -> Result<Vec<i32>, String> {
    let name = if steim2 { "steim2" } else { "steim1" };
    if data.len() < 64 {
        return Err(format!("{name}: data shorter than one frame"));
    }
    if nsamp == 0 {
        return Ok(Vec::new());
    }
    let mut diffs: Vec<i32> = Vec::with_capacity(nsamp + 8);
    let mut x0: i32 = 0;
    'frames: for (f, frame) in data.chunks_exact(64).enumerate() {
        let ctrl = u32_at(frame, 0, be).unwrap_or(0);
        for w in 1..16usize {
            let v = u32_at(frame, w * 4, be).unwrap_or(0);
            if f == 0 && w == 1 {
                x0 = v as i32;
                continue;
            }
            if f == 0 && w == 2 {
                continue; // xn: reverse integration constant (checked by callers that care)
            }
            let nib = (ctrl >> (30 - 2 * w as u32)) & 3;
            match (nib, steim2) {
                (0, _) => {}
                (1, _) => diffs.extend((0..4).map(|k| field(v, 24 - 8 * k, 8))),
                (2, false) => diffs.extend([field(v, 16, 16), field(v, 0, 16)]),
                (3, false) => diffs.push(v as i32),
                (2, true) => match v >> 30 {
                    1 => diffs.push(field(v, 0, 30)),
                    2 => diffs.extend((0..2).map(|k| field(v, 15 - 15 * k, 15))),
                    3 => diffs.extend((0..3).map(|k| field(v, 20 - 10 * k, 10))),
                    d => return Err(format!("{name}: invalid dnib {d} for nibble 2")),
                },
                (3, true) => match v >> 30 {
                    0 => diffs.extend((0..5).map(|k| field(v, 24 - 6 * k, 6))),
                    1 => diffs.extend((0..6).map(|k| field(v, 25 - 5 * k, 5))),
                    2 => diffs.extend((0..7).map(|k| field(v, 24 - 4 * k, 4))),
                    d => return Err(format!("{name}: invalid dnib {d} for nibble 3")),
                },
                _ => unreachable!("nibble is 2 bits"),
            }
            if diffs.len() >= nsamp {
                break 'frames;
            }
        }
    }
    if diffs.len() < nsamp {
        return Err(format!("{name}: only {} of {nsamp} differences present", diffs.len()));
    }
    // The first difference is relative to the previous record; x0 anchors the series.
    let mut out = Vec::with_capacity(nsamp);
    let mut x = x0;
    out.push(x);
    for d in &diffs[1..nsamp] {
        x = x.wrapping_add(*d);
        out.push(x);
    }
    Ok(out)
}

/// The reverse integration constant (last sample) stored in frame 0 of a Steim block.
pub fn steim_xn(data: &[u8], be: bool) -> Option<i32> {
    u32_at(data, 8, be).map(|v| v as i32)
}

// ---------------------------------------------------------------------------
// SeedLink framing
// ---------------------------------------------------------------------------

/// Pull complete `SL` packets off the front of `buf`, decoding their records. `INFO`
/// packets are skipped. Returns an error on protocol desync.
pub fn drain_packets(buf: &mut BytesMut, out: &mut Vec<Result<MsRecord, String>>) -> Result<(), String> {
    loop {
        if buf.len() < 8 {
            return Ok(());
        }
        if buf.starts_with(b"END") {
            return Err("server sent END".into());
        }
        if buf.starts_with(b"ERROR") {
            return Err("server sent ERROR".into());
        }
        if !buf.starts_with(b"SL") {
            return Err(format!("protocol desync (got {:?})", String::from_utf8_lossy(&buf[..8.min(buf.len())])));
        }
        let info = buf.starts_with(b"SLINFO");
        // Need enough of the record to learn its length from blockette 1000.
        if buf.len() < 8 + 64 {
            return Ok(());
        }
        let rec_len = if info { 512 } else { parse_header(&buf[8..]).map(|h| h.rec_len).unwrap_or(512) };
        if buf.len() < 8 + rec_len {
            return Ok(());
        }
        let packet = buf.split_to(8 + rec_len);
        if !info {
            out.push(parse_record(&packet[8..]));
        }
    }
}

async fn read_line(rd: &mut BufReader<TcpStream>) -> Result<String, String> {
    let mut line = String::new();
    let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, rd.read_line(&mut line))
        .await
        .map_err(|_| "handshake timeout".to_string())?
        .map_err(|e| format!("read failed: {e}"))?;
    if n == 0 {
        return Err("server closed during handshake".into());
    }
    Ok(line.trim_end().to_string())
}

async fn command(rd: &mut BufReader<TcpStream>, cmd: &str) -> Result<String, String> {
    rd.get_mut().write_all(format!("{cmd}\r\n").as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
    let reply = read_line(rd).await?;
    if !reply.starts_with("OK") {
        return Err(format!("`{cmd}` rejected: {reply}"));
    }
    Ok(reply)
}

/// Connect and run the v3 multi-station handshake; returns the stream ready for packets.
pub async fn connect(server: &str, streams: &[StreamSel]) -> Result<(BufReader<TcpStream>, String), String> {
    let tcp = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(server))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let mut rd = BufReader::new(tcp);
    rd.get_mut().write_all(b"HELLO\r\n").await.map_err(|e| format!("write failed: {e}"))?;
    let banner = read_line(&mut rd).await?;
    let _org = read_line(&mut rd).await?;
    for s in streams {
        command(&mut rd, &format!("STATION {} {}", s.sta, s.net)).await?;
        command(&mut rd, &format!("SELECT {}", s.select_pattern())).await?;
        command(&mut rd, "DATA").await?;
    }
    rd.get_mut().write_all(b"END\r\n").await.map_err(|e| format!("write failed: {e}"))?;
    Ok((rd, banner))
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

/// Classic STA/LTA trigger on squared, demeaned samples.
#[derive(Debug, Clone)]
pub struct StaLta {
    sta: f64,
    lta: f64,
    a_sta: f64,
    a_lta: f64,
    seen: u64,
    warmup: u64,
    refractory: u64,
    quiet_until: u64,
    threshold: f64,
    pub ratio: f64,
}

impl StaLta {
    pub fn new(rate_hz: f64, sta_s: f64, lta_s: f64, threshold: f64, refractory_s: f64) -> Self {
        let rate = rate_hz.max(1e-3);
        Self {
            sta: 0.0,
            lta: 0.0,
            a_sta: (1.0 / (rate * sta_s)).min(1.0),
            a_lta: (1.0 / (rate * lta_s)).min(1.0),
            seen: 0,
            warmup: (rate * lta_s) as u64,
            refractory: (rate * refractory_s) as u64,
            quiet_until: 0,
            threshold,
            ratio: 1.0,
        }
    }

    /// Push one demeaned sample; `Some(ratio)` when a trigger fires.
    pub fn push(&mut self, y: f64) -> Option<f64> {
        let p = y * y;
        if self.seen == 0 {
            self.sta = p;
            self.lta = p.max(1e-12);
        } else {
            self.sta += (p - self.sta) * self.a_sta;
            self.lta += (p - self.lta) * self.a_lta;
        }
        self.seen += 1;
        self.ratio = self.sta / self.lta.max(1e-12);
        if self.seen > self.warmup && self.seen >= self.quiet_until && self.ratio > self.threshold {
            self.quiet_until = self.seen + self.refractory;
            return Some(self.ratio);
        }
        None
    }
}

/// Slow running min/max that maps a slowly varying quantity (log rms) into 0..1, so quiet
/// stations still move. The bounds relax toward the current value with time constant
/// `window` updates; the span never drops below `min_span`.
#[derive(Debug, Clone)]
pub struct AdaptiveRange {
    lo: Option<f64>,
    hi: f64,
    relax: f64,
    min_span: f64,
}

impl AdaptiveRange {
    pub fn new(window_updates: f64, min_span: f64) -> Self {
        Self { lo: None, hi: 0.0, relax: 1.0 / window_updates.max(1.0), min_span }
    }
    pub fn update(&mut self, v: f64) -> f64 {
        if !v.is_finite() {
            return 0.0;
        }
        let Some(lo) = self.lo.as_mut() else {
            self.lo = Some(v);
            self.hi = v;
            return 0.5;
        };
        *lo = lo.min(v);
        self.hi = self.hi.max(v);
        *lo += (v - *lo) * self.relax;
        self.hi += (v - self.hi) * self.relax;
        let span = (self.hi - *lo).max(self.min_span);
        // Centre the minimum span around the data when the range is still narrow.
        let base = if self.hi - *lo < self.min_span { (self.hi + *lo - span) / 2.0 } else { *lo };
        clamp01((v - base) / span)
    }
    pub fn bounds(&self) -> Option<(f64, f64)> {
        self.lo.map(|lo| (lo, self.hi))
    }
}

/// What one playout tick of one stream produced.
#[derive(Debug, Default)]
pub struct TickOut {
    pub readings: Vec<(&'static str, f64)>,
    /// `(ratio, peak_counts)` when the STA/LTA trigger fired.
    pub onset: Option<(f64, f64)>,
    /// `rms01` when sustained energy crossed 30 s.
    pub tremor: Option<f64>,
}

/// Per-stream playout + analysis state.
pub struct StreamState {
    pub sel: StreamSel,
    pub label: String,
    gain: f64,
    rate: f64,
    queue: VecDeque<f64>,
    carry: f64,
    /// Demeaned samples recently played (≈5 s).
    recent: VecDeque<f64>,
    dc: f64,
    dc_ready: bool,
    trigger: Option<StaLta>,
    range: AdaptiveRange,
    pub records: u64,
    last_end_unix: f64,
    last_rms_db: f64,
    last_peak_db: f64,
    last_ratio: f64,
    rms01: f64,
    tremor_above: f64,
    tremor_fired: bool,
}

impl StreamState {
    pub fn new(sel: StreamSel, label: String, gain: f64) -> Self {
        Self {
            sel,
            label,
            gain,
            rate: 0.0,
            queue: VecDeque::new(),
            carry: 0.0,
            recent: VecDeque::new(),
            dc: 0.0,
            dc_ready: false,
            trigger: None,
            range: AdaptiveRange::new(6000.0, 6.0), // ~10 min at 10 Hz; ≥6 dB span
            records: 0,
            last_end_unix: 0.0,
            last_rms_db: 0.0,
            last_peak_db: 0.0,
            last_ratio: 1.0,
            rms01: 0.0,
            tremor_above: 0.0,
            tremor_fired: false,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }
    pub fn queued_s(&self) -> f64 {
        if self.rate > 0.0 { self.queue.len() as f64 / self.rate } else { 0.0 }
    }

    /// Queue a decoded record for playout.
    pub fn push_record(&mut self, rec: &MsRecord) {
        if rec.header.rate_hz <= 0.0 || rec.samples.is_empty() {
            return;
        }
        if (self.rate - rec.header.rate_hz).abs() > 1e-6 {
            self.rate = rec.header.rate_hz;
            self.trigger = Some(StaLta::new(self.rate, 1.0, 30.0, 4.0, 10.0));
        }
        self.records += 1;
        self.last_end_unix = rec.header.end_unix();
        self.queue.extend(rec.samples.iter().map(|s| s * self.gain));
        // Fell far behind (long outage, then a burst): skip ahead to the newest 20 s.
        let max_q = (self.rate * 60.0) as usize;
        if self.queue.len() > max_q {
            let keep = (self.rate * 20.0) as usize;
            let drop = self.queue.len() - keep;
            self.queue.drain(..drop);
        }
    }

    /// Advance playout by `dt` seconds.
    pub fn tick(&mut self, dt: f64) -> TickOut {
        let mut out = TickOut::default();
        if self.rate <= 0.0 || self.records == 0 {
            return out;
        }
        self.carry += self.rate * dt;
        let want = self.carry.floor() as usize;
        self.carry -= want as f64;
        let n = want.min(self.queue.len());
        let dc_alpha = (1.0 / (self.rate * 30.0)).min(1.0);
        let cap = (self.rate * 5.0).ceil() as usize + 1;
        let (mut sum_sq, mut peak) = (0.0, 0.0);
        for _ in 0..n {
            let Some(x) = self.queue.pop_front() else { break };
            if !self.dc_ready {
                self.dc = x;
                self.dc_ready = true;
            } else {
                self.dc += (x - self.dc) * dc_alpha;
            }
            let y = x - self.dc;
            sum_sq += y * y;
            peak = f64::max(peak, y.abs());
            if let Some(t) = self.trigger.as_mut() {
                if let Some(ratio) = t.push(y) {
                    out.onset = Some((ratio, peak));
                }
                self.last_ratio = t.ratio;
            }
            self.recent.push_back(y);
            if self.recent.len() > cap {
                self.recent.pop_front();
            }
        }
        if n > 0 {
            let rms = (sum_sq / n as f64).sqrt();
            self.last_rms_db = 20.0 * rms.max(1e-3).log10();
            self.last_peak_db = 20.0 * peak.max(1e-3).log10();
            self.rms01 = self.range.update(self.last_rms_db);
        }
        let (lo, hi) = self.range.bounds().unwrap_or((self.last_rms_db, self.last_rms_db + 6.0));
        let span = (hi - lo).max(6.0);
        let peak01 = clamp01((self.last_peak_db - lo) / span);
        // Sustained energy: rms01 > 0.7 for 30 s, re-armed below 0.5.
        if self.rms01 > 0.7 {
            self.tremor_above += dt;
            if self.tremor_above >= 30.0 && !self.tremor_fired {
                self.tremor_fired = true;
                out.tremor = Some(self.rms01);
            }
        } else if self.rms01 < 0.5 {
            self.tremor_above = 0.0;
            self.tremor_fired = false;
        }
        let flowing = if dt > 0.0 { n as f64 / dt } else { 0.0 };
        out.readings = vec![
            ("rms", self.rms01),
            ("rms_db", self.last_rms_db),
            ("peak01", peak01),
            ("dominant_hz", self.dominant_hz()),
            ("samples_per_s", flowing),
            ("ratio", self.last_ratio),
            ("latency_s", (util::unix_now() - self.last_end_unix + self.queued_s()).max(0.0)),
        ];
        out
    }

    /// Zero-crossing rate over the last second, halved.
    pub fn dominant_hz(&self) -> f64 {
        let n = (self.rate.ceil() as usize).min(self.recent.len());
        if n < 2 {
            return 0.0;
        }
        let win = self.recent.iter().skip(self.recent.len() - n);
        let mut prev: Option<bool> = None;
        let mut crossings = 0usize;
        for y in win {
            let s = *y >= 0.0;
            if prev.is_some_and(|p| p != s) {
                crossings += 1;
            }
            prev = Some(s);
        }
        crossings as f64 / 2.0 / (n as f64 / self.rate)
    }

    /// Last ≈5 s of waveform, normalised to −1..1 and decimated to ≤ `max_points`.
    pub fn waveform(&self, max_points: usize) -> Vec<f64> {
        if self.recent.is_empty() {
            return Vec::new();
        }
        let scale = self.recent.iter().fold(0.0_f64, |m, y| m.max(y.abs())).max(1e-9);
        let stride = self.recent.len().div_ceil(max_points.max(1));
        self.recent.iter().step_by(stride).map(|y| (y / scale * 1000.0).round() / 1000.0).collect()
    }
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

struct SeedlinkSource {
    server: String,
    streams: Vec<StreamSel>,
    gain: f64,
}

/// Stream labels: the station when unique, else `STA.CHA`.
pub fn labels(streams: &[StreamSel]) -> Vec<String> {
    let unique = streams.iter().map(|s| &s.sta).collect::<std::collections::HashSet<_>>().len() == streams.len();
    streams.iter().map(|s| if unique { s.sta.clone() } else { format!("{}.{}", s.sta, s.cha) }).collect()
}

impl Source for SeedlinkSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut states: Vec<StreamState> = self
                .streams
                .iter()
                .cloned()
                .zip(labels(&self.streams))
                .map(|(sel, label)| StreamState::new(sel, label, self.gain))
                .collect();
            let mut rng = util::rng_from_seed(util::instance_seed(em.ctx.seed, em.ctx.id.as_str(), None));
            let mut attempt = 0u32;
            loop {
                if em.is_shutdown() {
                    return;
                }
                em.status_force(SourceState::Connecting, format!("connecting to {}", self.server)).await;
                let started = std::time::Instant::now();
                let res = session(&mut em, &self.server, &mut states).await;
                if em.is_shutdown() {
                    return;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = util::retry_delay(attempt, &mut rng);
                attempt = attempt.saturating_add(1);
                let msg = res.err().unwrap_or_else(|| "connection closed".into());
                tracing::warn!(source = %em.ctx.id, attempt, %msg, "seedlink connection lost");
                em.status_force(SourceState::Reconnecting, format!("{msg}; reconnect in {:.0}s", d.as_secs_f64())).await;
                if !em.sleep(d).await {
                    return;
                }
            }
        })
    }
}

async fn wait_shutdown(rx: &mut tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn session(em: &mut Emitter, server: &str, states: &mut [StreamState]) -> Result<(), String> {
    let sels: Vec<StreamSel> = states.iter().map(|s| s.sel.clone()).collect();
    let (mut rd, banner) = connect(server, &sels).await?;
    em.status(SourceState::Connecting, format!("handshake ok: {banner}")).await;
    let mut shutdown = em.ctx.shutdown.clone();
    let mut buf = BytesMut::with_capacity(16 * 1024);
    let mut decoded: Vec<Result<MsRecord, String>> = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_packet = std::time::Instant::now();
    let mut last_tick: Option<Seconds> = None;
    let mut ticks: u32 = 0;
    let multi = states.len() > 1;
    loop {
        tokio::select! {
            r = rd.read_buf(&mut buf) => {
                match r {
                    Ok(0) => return Err("server closed".into()),
                    Ok(_) => {}
                    Err(e) => return Err(format!("read error: {e}")),
                }
                drain_packets(&mut buf, &mut decoded)?;
                for rec in decoded.drain(..) {
                    match rec {
                        Ok(rec) => {
                            last_packet = std::time::Instant::now();
                            if let Some(st) = states.iter_mut().find(|s| s.sel.matches(&rec.header)) {
                                st.push_record(&rec);
                                em.status(SourceState::Live, "live").await;
                            } else {
                                tracing::debug!(source = %em.ctx.id, id = %rec.header.net, "record for unselected stream");
                            }
                        }
                        Err(e) => tracing::debug!(source = %em.ctx.id, %e, "bad record"),
                    }
                }
            }
            _ = tick.tick() => {
                if last_packet.elapsed() > SILENCE_LIMIT {
                    return Err(format!("no data for {} s", SILENCE_LIMIT.as_secs()));
                }
                if last_packet.elapsed() > Duration::from_secs(90) {
                    em.status(SourceState::Idle, "no records for 90 s").await;
                }
                let now = em.now();
                let dt = last_tick.map(|t| (now - t).clamp(0.0, 1.0)).unwrap_or(TICK.as_secs_f64());
                last_tick = Some(now);
                ticks = ticks.wrapping_add(1);
                let mut readings: Vec<(String, f64)> = Vec::new();
                let mut events: Vec<(&'static str, f64, Attrs)> = Vec::new();
                for st in states.iter_mut() {
                    let out = st.tick(dt);
                    if out.readings.is_empty() {
                        continue;
                    }
                    let prefix = if multi { format!("{}.", st.label) } else { String::new() };
                    readings.extend(out.readings.iter().map(|(k, v)| (format!("{prefix}{k}"), *v)));
                    let base = || Attrs::new().text("station", st.sel.sta.clone()).text("net", st.sel.net.clone()).text("channel", st.sel.cha.clone());
                    if let Some((ratio, peak)) = out.onset {
                        events.push(("onset", clamp01(ratio / 20.0), base().num("ratio", ratio).num("peak_counts", peak)));
                    }
                    if let Some(level) = out.tremor {
                        events.push(("tremor", clamp01(level), base().num("rms01", level).num("rms_db", st.last_rms_db)));
                    }
                }
                if !readings.is_empty() {
                    em.reading(readings).await;
                }
                for (name, intensity, attrs) in events {
                    em.event(name, intensity, attrs.into_map()).await;
                }
                if ticks.is_multiple_of(VIZ_EVERY) {
                    if let Some(st) = states.first() {
                        if st.records > 0 {
                            let payload: Json = json!({
                                "waveform": st.waveform(200),
                                "station": st.sel.sta,
                                "channel": st.sel.cha,
                                "rate": st.rate(),
                                "ratio": st.last_ratio,
                                "rms": st.rms01,
                            });
                            em.viz(payload).await;
                        }
                    }
                }
                if em.is_shutdown() {
                    return Ok(());
                }
            }
            _ = wait_shutdown(&mut shutdown) => return Ok(()),
        }
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "seedlink.seismic",
        summary: "Live seismometer waveforms via FDSN SeedLink (miniSEED Steim1/2); rms/peak/frequency readings, STA/LTA `onset` events",
        params: vec![
            ParamInfo { name: "server", summary: "SeedLink host:port", default: Some(DEFAULT_SERVER) },
            ParamInfo {
                name: "streams",
                summary: "Streams as \"NET_STA_LOC_CHA\" strings or {net, sta, loc, cha} tables (several → readings prefixed `STA.`)",
                default: Some("[\"IU_ANMO_00_BHZ\"]"),
            },
            ParamInfo { name: "gain", summary: "Multiply raw counts by this", default: Some("1") },
        ],
        channels: vec!["rms", "rms_db", "peak01", "dominant_hz", "samples_per_s", "ratio", "latency_s"],
        events: vec!["onset", "tremor"],
        network: true,
    }
}

/// Parse the `streams` parameter (string, comma list, array of strings and/or tables).
pub fn parse_streams(spec: &SourceSpec) -> Result<Vec<StreamSel>, SourceError> {
    let bad = |what: &str| util::invalid(spec, "streams", &format!("{what}; expected \"NET_STA_LOC_CHA\" or {{net, sta, loc, cha}}"));
    let Some(v) = spec.params.get("streams") else {
        return Ok(vec![StreamSel::new("IU", "ANMO", "00", "BHZ").expect("default stream is valid")]);
    };
    let items: Vec<Json> = match v {
        Json::Null => Vec::new(),
        Json::String(s) => s.split(',').filter(|p| !p.trim().is_empty()).map(|p| Json::String(p.to_string())).collect(),
        Json::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let mut out = Vec::new();
    for item in &items {
        let sel = match item {
            Json::String(s) => StreamSel::parse(s).ok_or_else(|| bad(&format!("`{s}` is not a stream id")))?,
            Json::Object(o) => {
                let g = |k: &str| o.get(k).and_then(Json::as_str).unwrap_or("");
                let net = if g("net").is_empty() { g("network") } else { g("net") };
                let sta = if g("sta").is_empty() { g("station") } else { g("sta") };
                let loc = if g("loc").is_empty() { g("location") } else { g("loc") };
                let cha = if g("cha").is_empty() { if g("channel").is_empty() { "BHZ" } else { g("channel") } } else { g("cha") };
                StreamSel::new(net, sta, loc, cha).ok_or_else(|| bad(&format!("bad stream table {item}")))?
            }
            other => return Err(bad(&format!("unexpected value {other}"))),
        };
        if !out.contains(&sel) {
            out.push(sel);
        }
    }
    if out.is_empty() {
        return Err(bad("no streams"));
    }
    if out.len() > 16 {
        return Err(util::invalid(spec, "streams", "at most 16 streams"));
    }
    Ok(out)
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let mut server = util::param_str(spec, "server", DEFAULT_SERVER).trim().to_string();
    if server.is_empty() {
        return Err(util::invalid(spec, "server", "empty host"));
    }
    if !server.contains(':') {
        server.push_str(":18000");
    }
    let gain = util::param_f64(spec, "gain", 1.0)?;
    if !gain.is_finite() || gain == 0.0 {
        return Err(util::invalid(spec, "gain", "expected a non-zero number"));
    }
    Ok(Box::new(SeedlinkSource { server, streams: parse_streams(spec)?, gain }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn steim1_reference_record() {
        let rec = unhex(fixtures::STEIM1_REF);
        let r = parse_record(&rec).unwrap();
        assert_eq!((r.header.encoding, r.header.nsamp, r.header.rate_hz), (10, 244, 40.0));
        assert_eq!((r.header.sta.as_str(), r.header.net.as_str(), r.header.cha.as_str()), ("TEST", "XX", "BHZ"));
        let got: Vec<i32> = r.samples.iter().map(|s| *s as i32).collect();
        assert_eq!(got, &fixtures::SINE[..244]);
        assert_eq!(steim_xn(&rec[64..], true), Some(63396));
        assert_eq!(got.last(), Some(&63396));
    }

    #[test]
    fn steim2_reference_record() {
        let rec = unhex(fixtures::STEIM2_REF);
        let r = parse_record(&rec).unwrap();
        assert_eq!((r.header.encoding, r.header.nsamp), (11, 247));
        let got: Vec<i32> = r.samples.iter().map(|s| *s as i32).collect();
        assert_eq!(got, &fixtures::SINE[..247]);
        assert_eq!(got.last(), Some(&-70169));
    }

    #[test]
    fn int16_and_int32_reference_records() {
        let r = parse_record(&unhex(fixtures::INT16_REF)).unwrap();
        assert_eq!(r.header.encoding, 1);
        assert_eq!(r.samples.iter().map(|s| *s as i32).collect::<Vec<_>>(), &fixtures::SINE[..220]);
        let r = parse_record(&unhex(fixtures::INT32_REF)).unwrap();
        assert_eq!(r.header.encoding, 3);
        assert_eq!(r.samples.iter().map(|s| *s as i32).collect::<Vec<_>>(), &fixtures::SINE[..114]);
    }

    #[test]
    fn live_iris_packet_decodes_and_frames() {
        let pkt = unhex(fixtures::IRIS_SL_PACKET);
        assert_eq!(pkt.len(), 520);
        assert_eq!(&pkt[..2], b"SL");
        let h = parse_header(&pkt[8..]).unwrap();
        assert_eq!((h.net.as_str(), h.sta.as_str(), h.loc.as_str(), h.cha.as_str()), ("IU", "ANMO", "00", "BHZ"));
        assert_eq!((h.rate_hz, h.nsamp, h.encoding, h.rec_len, h.data_offset), (40.0, 554, 11, 512, 64));
        assert!(h.data_big_endian);
        assert!(h.start_unix > 1.7e9, "start {}", h.start_unix);
        let r = parse_record(&pkt[8..]).unwrap();
        assert_eq!(r.samples.len(), 554);
        let xn = steim_xn(&pkt[8 + 64..], true).unwrap();
        assert_eq!(*r.samples.last().unwrap() as i32, xn, "reverse integration constant must match");
        assert!(StreamSel::parse("IU_ANMO_00_BHZ").unwrap().matches(&h));
        assert!(StreamSel::parse("IU_ANMO__BHZ").unwrap().matches(&h));
        assert!(!StreamSel::parse("IU_ANMO_10_BHZ").unwrap().matches(&h));

        // Framing: feed the packet in two halves plus a trailing partial packet.
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        buf.extend_from_slice(&pkt[..300]);
        drain_packets(&mut buf, &mut out).unwrap();
        assert!(out.is_empty());
        buf.extend_from_slice(&pkt[300..]);
        buf.extend_from_slice(&pkt[..100]);
        drain_packets(&mut buf, &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(buf.len(), 100);
        let mut junk = BytesMut::from(&b"HTTP/1.1 400"[..]);
        assert!(drain_packets(&mut junk, &mut out).is_err());
    }

    #[test]
    fn hand_built_steim1_frame() {
        // Control word: w1,w2 = 0 (x0/xn), w3 nibble 1 (four 8-bit), w4 nibble 2 (two 16-bit), w5 nibble 3 (32-bit).
        let ctrl: u32 = (1 << 24) | (2 << 22) | (3 << 20);
        let mut frame = Vec::new();
        for w in [ctrl, 100u32, (100 + 1 - 2 + 127 - 128 + 1000 - 1 + 70000) as u32, 0x01FE_7F80, 0x03E8_FFFF, 70000] {
            frame.extend_from_slice(&w.to_be_bytes());
        }
        frame.resize(64, 0);
        let s = steim_decode(&frame, 7, true, false).unwrap();
        assert_eq!(s, vec![100, 98, 225, 97, 1097, 1096, 71096]);
        assert!(steim_decode(&frame, 8, true, false).is_err(), "not enough differences");
        assert!(steim_decode(&frame[..32], 1, true, false).is_err(), "short frame");
    }

    #[test]
    fn header_helpers() {
        assert_eq!(sample_rate(40, 1), 40.0);
        assert_eq!(sample_rate(-10, 1), 0.1);
        assert_eq!(sample_rate(1, -10), 0.1);
        assert_eq!(sample_rate(-2, -5), 0.1);
        assert_eq!(sample_rate(0, 1), 0.0);
        assert_eq!(btime_to_unix(1970, 1, 0, 0, 0, 0), Some(0.0));
        assert_eq!(btime_to_unix(2012, 133, 0, 0, 0, 5000), Some(1336780800.5)); // 2012-05-12T00:00:00.5Z
        assert!(btime_to_unix(2012, 400, 0, 0, 0, 0).is_none());
        assert!(parse_header(&[0u8; 10]).is_err());
        let mut rec = unhex(fixtures::STEIM1_REF);
        rec[6] = b'X';
        assert!(parse_header(&rec).is_err());
    }

    #[test]
    fn stream_selection_and_factory() {
        let s = StreamSel::parse("iu_anmo_00_bhz").unwrap();
        assert_eq!(s.id(), "IU.ANMO.00.BHZ");
        assert_eq!(s.select_pattern(), "00BHZ.D");
        assert_eq!(StreamSel::parse("GE.STU..BHZ").unwrap().select_pattern(), "??BHZ.D");
        assert_eq!(StreamSel::parse("GE_STU_--_BHZ").unwrap().loc, "");
        assert!(StreamSel::parse("toolong_X_00_BHZ").is_none());
        assert!(StreamSel::parse("x").is_none());
        let spec = SourceSpec::new("seedlink.seismic").with(
            "streams",
            json!(["IU_ANMO_00_BHZ", {"net": "GE", "sta": "STU", "cha": "BHZ"}, "IU_ANMO_00_BHZ"]),
        );
        let v = parse_streams(&spec).unwrap();
        assert_eq!(v.len(), 2, "duplicates collapse");
        assert_eq!(labels(&v), vec!["ANMO", "STU"]);
        let two = parse_streams(&SourceSpec::new("x").with("streams", "IU_ANMO_00_BHZ, IU_ANMO_00_BHN")).unwrap();
        assert_eq!(labels(&two), vec!["ANMO.BHZ", "ANMO.BHN"]);
        assert!(factory(&SourceSpec::new("seedlink.seismic")).is_ok());
        assert!(factory(&SourceSpec::new("seedlink.seismic").with("streams", "nope")).is_err());
        assert!(factory(&SourceSpec::new("seedlink.seismic").with("gain", 0)).is_err());
        assert!(factory(&SourceSpec::new("seedlink.seismic").with("streams", json!([]))).is_err());
    }

    fn fake_record(rate: f64, samples: Vec<f64>) -> MsRecord {
        let header = MsHeader {
            seq: "000001".into(),
            sta: "TEST".into(),
            loc: "00".into(),
            cha: "BHZ".into(),
            net: "XX".into(),
            start_unix: 0.0,
            nsamp: samples.len(),
            rate_hz: rate,
            encoding: 3,
            data_big_endian: true,
            rec_len: 512,
            data_offset: 64,
        };
        MsRecord { header, samples }
    }

    #[test]
    fn sta_lta_triggers_on_spike_once() {
        let rate = 40.0;
        let mut t = StaLta::new(rate, 1.0, 30.0, 4.0, 10.0);
        let mut rng = util::rng_from_seed(7);
        let noise = |rng: &mut rand_chacha::ChaCha8Rng| rand::RngExt::random_range(rng, -10.0..10.0);
        let mut fired = Vec::new();
        for i in 0..(rate as usize * 120) {
            let s = i as f64 / rate;
            let mut y = noise(&mut rng);
            if (60.0..61.0).contains(&s) {
                y += 200.0 * ((s - 60.0) * 50.0).sin();
            }
            if let Some(r) = t.push(y) {
                fired.push((s, r));
            }
        }
        assert_eq!(fired.len(), 1, "fired {fired:?}");
        assert!((60.0..60.5).contains(&fired[0].0));
        assert!(fired[0].1 > 4.0);
        // Warm-up: a spike at t=5 s must not trigger.
        let mut t2 = StaLta::new(rate, 1.0, 30.0, 4.0, 10.0);
        let mut hits = 0;
        for i in 0..(rate as usize * 10) {
            let y = if i == 200 { 1e6 } else { noise(&mut rng) };
            hits += usize::from(t2.push(y).is_some());
        }
        assert_eq!(hits, 0);
    }

    #[test]
    fn playout_consumes_in_real_time_and_reports() {
        let sel = StreamSel::parse("XX_TEST_00_BHZ").unwrap();
        let mut st = StreamState::new(sel, "TEST".into(), 1.0);
        assert!(st.tick(0.1).readings.is_empty(), "nothing before the first record");
        // 10 s of a 2 Hz sine on a 5000-count DC offset.
        let samples: Vec<f64> = (0..400).map(|i| 5000.0 + 100.0 * (i as f64 / 40.0 * 2.0 * std::f64::consts::TAU).sin()).collect();
        st.push_record(&fake_record(40.0, samples));
        assert!((st.queued_s() - 10.0).abs() < 1e-9);
        let mut last = TickOut::default();
        for _ in 0..50 {
            last = st.tick(0.1);
        }
        assert!((st.queued_s() - 5.0).abs() < 1e-9, "half consumed: {}", st.queued_s());
        let r: std::collections::BTreeMap<_, _> = last.readings.iter().cloned().collect();
        assert_eq!(r["samples_per_s"], 40.0);
        assert!((r["dominant_hz"] - 2.0).abs() <= 0.6, "dominant {}", r["dominant_hz"]);
        assert!((0.0..=1.0).contains(&r["rms"]) && (0.0..=1.0).contains(&r["peak01"]));
        let wf = st.waveform(200);
        assert!(wf.len() <= 200 && wf.len() >= 100);
        assert!(wf.iter().all(|v| v.abs() <= 1.0));
        // Starvation: queue runs dry, readings keep coming with samples_per_s = 0.
        for _ in 0..60 {
            last = st.tick(0.1);
        }
        let r: std::collections::BTreeMap<_, _> = last.readings.iter().cloned().collect();
        assert_eq!(r["samples_per_s"], 0.0);
        // Catch-up: a huge backlog is trimmed to 20 s.
        st.push_record(&fake_record(40.0, vec![0.0; 40 * 100]));
        assert!((st.queued_s() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn adaptive_range_moves_quiet_signals() {
        let mut r = AdaptiveRange::new(100.0, 6.0);
        assert_eq!(r.update(-20.0), 0.5);
        // A 2 dB wiggle still spans a visible part of 0..1 thanks to the 6 dB floor.
        let a = r.update(-21.0);
        let b = r.update(-19.0);
        assert!(b - a > 0.2, "{a} {b}");
        for _ in 0..1000 {
            r.update(10.0);
        }
        // Converged on a constant: the value sits mid-span; steps outside the span pin to the ends.
        assert!((r.update(10.0) - 0.5).abs() < 0.01);
        let hi = r.update(16.0);
        let lo = r.update(4.0);
        assert!(hi > 0.9 && lo < 0.1, "{hi} {lo}");
        assert_eq!(r.update(f64::NAN), 0.0);
    }

    /// End to end against a mock SeedLink server: handshake, framing, playout readings,
    /// viz, status, and prompt shutdown.
    #[tokio::test]
    async fn runs_against_mock_server_and_stops() {
        use sonder_core::{Datum, SourceId, WallClock};
        use std::sync::Arc;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pkt = unhex(fixtures::IRIS_SL_PACKET);
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut rd = BufReader::new(sock);
            let mut line = String::new();
            rd.read_line(&mut line).await.unwrap();
            assert_eq!(line, "HELLO\r\n");
            rd.get_mut().write_all(b"SeedLink v3.1 (mock)\r\nmock org\r\n").await.unwrap();
            let mut cmds = Vec::new();
            loop {
                line.clear();
                if rd.read_line(&mut line).await.unwrap() == 0 {
                    return;
                }
                let l = line.trim_end().to_string();
                if l == "END" {
                    break;
                }
                cmds.push(l);
                rd.get_mut().write_all(b"OK\r\n").await.unwrap();
            }
            assert_eq!(cmds, vec!["STATION ANMO IU", "SELECT 00BHZ.D", "DATA"]);
            // Split a packet across writes, then keep a slow trickle going until the client leaves.
            rd.get_mut().write_all(&pkt[..100]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            rd.get_mut().write_all(&pkt[100..]).await.unwrap();
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if rd.get_mut().write_all(&pkt).await.is_err() {
                    return;
                }
            }
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: SourceId::new("quake"),
            tx,
            clock: Arc::new(WallClock::new()),
            http: crate::default_http_client(),
            shutdown,
            seed: 1,
        };
        let src = factory(&SourceSpec::new("seedlink.seismic").with("server", addr.to_string())).unwrap();
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("source must stop on shutdown").unwrap();
        let (mut readings, mut viz, mut live) = (0, 0, false);
        while let Ok(s) = rx.try_recv() {
            match s.datum {
                Datum::Audio { .. } => {}
                Datum::Reading { values } => {
                    assert!(values.contains_key("rms") && values.contains_key("dominant_hz") && values.contains_key("ratio"));
                    assert!((30.0..=50.0).contains(&values["samples_per_s"]), "playout rate ≈ 40 Hz: {}", values["samples_per_s"]);
                    readings += 1;
                }
                Datum::Viz { payload } => {
                    assert_eq!(payload["station"], "ANMO");
                    assert!(payload["waveform"].as_array().is_some_and(|w| !w.is_empty() && w.len() <= 200));
                    viz += 1;
                }
                Datum::Status { state, .. } => live |= state == SourceState::Live,
                Datum::Event { .. } => {}
            }
        }
        assert!(live, "must report Live after the first record");
        assert!(readings >= 8, "readings {readings}");
        assert!(viz >= 1, "viz {viz}");
    }

    /// `cargo test -p sonder-sources -- --ignored seedlink --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_iris_decodes_records() {
        let sel = StreamSel::parse("IU_ANMO_00_BHZ").unwrap();
        let (mut rd, banner) = connect(DEFAULT_SERVER, std::slice::from_ref(&sel)).await.unwrap();
        println!("banner: {banner}");
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        let mut records: Vec<MsRecord> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while records.len() < 3 && std::time::Instant::now() < deadline {
            let n = tokio::time::timeout(Duration::from_secs(60), rd.read_buf(&mut buf)).await.unwrap().unwrap();
            assert!(n > 0, "server closed");
            drain_packets(&mut buf, &mut out).unwrap();
            for r in out.drain(..) {
                records.push(r.unwrap());
            }
        }
        assert!(records.len() >= 3, "got {} records", records.len());
        let mut st = StreamState::new(sel, "ANMO".into(), 1.0);
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for r in &records {
            assert!(r.header.sta == "ANMO" && r.header.rate_hz > 0.0);
            let mean = r.samples.iter().sum::<f64>() / r.samples.len() as f64;
            let rms = (r.samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / r.samples.len() as f64).sqrt();
            lo = lo.min(rms);
            hi = hi.max(rms);
            println!(
                "{} {}.{}.{}.{} rate={} nsamp={} enc={} start={:.1} rms={rms:.1} counts",
                r.header.seq, r.header.net, r.header.sta, r.header.loc, r.header.cha, r.header.rate_hz, r.header.nsamp, r.header.encoding, r.header.start_unix
            );
            st.push_record(r);
        }
        println!("sample rate {} Hz; rms range {lo:.1}..{hi:.1} counts; queued {:.1} s", st.rate(), st.queued_s());
        let t = st.tick(0.1);
        assert_eq!(t.readings.len(), 7);
    }

    mod fixtures {
        /// libmseed's reference series (`test/testdata.h`): an expanding sinusoid, truncated to i32.
        pub const SINE: [i32; 500] = [
            0, 6, 10, 10, 6, 0, -7, -12, -12, -8,
            0, 8, 14, 15, 9, 0, -10, -17, -18, -11,
            0, 13, 21, 21, 13, 0, -16, -26, -26, -16,
            1, 19, 31, 31, 19, -1, -23, -37, -37, -23,
            2, 28, 45, 45, 27, -2, -34, -54, -54, -32,
            3, 41, 65, 65, 38, -5, -50, -79, -78, -46,
            6, 61, 95, 94, 55, -8, -74, -115, -113, -65,
            11, 90, 138, 135, 78, -14, -109, -167, -162, -92,
            19, 132, 201, 194, 110, -24, -160, -242, -233, -131,
            31, 193, 291, 280, 156, -39, -234, -351, -336, -185,
            50, 284, 422, 402, 220, -63, -343, -508, -483, -262,
            79, 416, 612, 579, 311, -100, -503, -737, -694, -370,
            125, 609, 887, 832, 440, -157, -737, -1068, -997, -522,
            196, 892, 1285, 1195, 620, -245, -1080, -1547, -1432, -736,
            306, 1306, 1862, 1716, 874, -381, -1580, -2241, -2057, -1037,
            473, 1912, 2697, 2465, 1231, -588, -2313, -3245, -2954, -1460,
            730, 2797, 3905, 3539, 1732, -905, -3383, -4699, -4240, -2053,
            1121, 4091, 5655, 5080, 2433, -1387, -4947, -6804, -6086, -2884,
            1714, 5981, 8186, 7291, 3416, -2118, -7231, -9848, -8734, -4046,
            2614, 8742, 11848, 10461, 4791, -3225, -10568, -14253, -12530, -5671,
            3975, 12774, 17146, 15006, 6711, -4897, -15440, -20625, -17971, -7940,
            6029, 18661, 24808, 21521, 9390, -7418, -22552, -29840, -25771, -11101,
            9123, 27253, 35890, 30858, 13120, -11212, -32932, -43165, -36947, -15501,
            13774, 39791, 51913, 44235, 18308, -16912, -48077, -62431, -52957, -21614,
            20756, 58084, 75077, 63396, 25508, -25462, -70169, -90282, -75887, -30092,
            31222, 84764, 108561, 90834, 35484, -38269, -102388, -130535, -108719, -41825,
            46887, 123670, 156952, 130117, 49276, -57425, -149366, -188707, -155716, -58028,
            70304, 180390, 226878, 186341, 68302, -86041, -217845, -272760, -222974, -80353,
            105265, 263063, 327906, 266792, 94480, -128742, -317647, -394187, -319201, -111030,
            157403, 383537, 473846, 381879, 130403, -192385, -463067, -569581, -456835, -153063,
            235071, 559057, 684630, 546467, 179546, -287144, -674908, -822885, -653642, -210470,
            350653, 814722, 989021, 781782, 246545, -428093, -983445, -1188650, -934978, -288589,
            522495, 1187046, 1428517, 1118118, 337539, -637550, -1432720, -1716720, -1337035, -394463,
            777745, 1729148, 2062985, 1598702, 460582, -948539, -2086795, -2478993, -1911440, -537283,
            1156565, 2518284, 2978771, 2285191, 626136, -1409890, -3038834, -3579162, -2731822, -728915,
            1718317, 3666796, 4300394, 3265502, 847610, -2093758, -4424295, -5166751, -3903146, -984449,
            2550691, 5338008, 6207395, 4664945, 1141905, -3106699, -6440095, -7457334, -5575000, -1322711,
            3783146, 7769327, 8958602, 6662072, 1529858, -4605977, -9372442, -10761660, -7960482, -1766591,
            5606698, 11305779, 12927085, 9511187, 2036388, -6823565, -13637247, -15527597, -11363048, -2342921,
            8303023, 16448700, 18650486, 13574359, 2689991, -10101450, -19838792, -22400528, -16214653, -3081429,
            12287276, 23926421, 26903486, 19366867, 3520957, -14943542, -28854881, -32310300, -23129911, -4011981,
            18171019, 34796853, 38802125, 27621733, 4557312, -22091965, -41960429, -46596369, -32982970, -5158786,
            26854699, 50596356, 55953935, 39381287, 5816755, -32639133, -61006773, -67187912, -47016556, -6529407,
            39663473, 73555723, 80673993, 56127012, 7291879, -48192344, -88681820, -96862974, -66996583, -8095097,
            58546627, 106913500, 116295749, 79963612, 8924265, -71115377, -128887386, -139621279, -95431237, -9756926,
            86370256, 155370411, 167618156, 113879744, 10560464, -104883012, -187286440, -201220454, -135881249, -11288915,
            127346645, 225748346, 241548722, 162117178, 11878900, -154601039, -272096614, -289947157, -193399036, -12244462,
            187663986, 327945826, 348028166, 230693116, 12270520, -227768756, -395240626, -417725794, -275149858, -11804612,
            276409565, 476323094, 501359772, 328138760, 10646479, -335396614, -574013851, -601712311, -391289845, -8534971,
            406922710, 691709700, 722120145, 466542952, 5131609, -493643879, -833501145, -866584864, -556206270, 0,
        ];
        /// libmseed `reference-testdata-steim1.mseed2`, record 0 (244 samples = SINE[..244])
        pub const STEIM1_REF: &str = "\
            30303030303052205445535420202042485a585807dc008500000000000000f400280001000000010000000000400030\
            03e800000a010900000000000000000001555555000000000000f7a400060400fcfaf9fb000408080601faf7f6f9ff07\
            0b0d0800f8f3f0f6000a11120c00f4eceaf2000e191a1100eee3e0ec0016232615556aaa1800e5d5d3e30120343722ff\
            d9c1bed702304c4f30fdc7a4a1c605466f7145f9ffacff7aff78ffae0009006600a200a20062fff5ff84ff3dff3dff8b\
            2aaaaaaa000f009700eb00ea008affecff4afee5fee8ff5b001900dd0155015100c4ffdffef4fe65fe6dff16002b0144\
            01ef01e40116ffc9fe78fdabfdbcfeb52aaaaaaa004701db02ce02b80189ffa6fdc1fc9ffcbdfe2d007302b8041203e8\
            022cff6efcb6fb19fb51fd6b00b803fc05e6059f0311ff18fb2ef8e5f943fc5c2aaaaaaa012305d6088e08130454fe92\
            f8f1f5b3f652fadc01cb088b0c660b9a061cfdc1f5a9f114f218f8bf02ce0c8211f610ab089dfc81f0ddea62ec07f5c7\
            2aaaaaaa045a12501a0417f00c22fa95e9dae0b0e351f19b06bb1acb25ae225f1114f7a4df99d2a8d6d1ebbf0a5e272f\
            369131581803f329d09dbe58c4e2e3882aaabebe0fe5394e4f0046d221bdec58bab6a0f4ab28d807184a53c6725b65a1\
            2f5ae202ffff9ab9ffff766c8643c7ee25027a6f0000a582000091d04261d25f\
        ";
        /// libmseed `reference-testdata-steim2.mseed2`, record 0 (247 samples = SINE[..247])
        pub const STEIM2_REF: &str = "\
            30303030303052205445535420202042485a585807dc008500000000000000f700280001000000010000000000400030\
            03e800000b010900000000000000000003ffff5500000000fffeede780640ca97602210643abdb3f4eb6a01833c3600a\
            1148c0342cab200e1969102e2382c01623261800e5d5d3e30120343722ffd9c1156aaaaabed702304c4f30fdc7a4a1c6\
            05466f71c45fe7acf7ade3aec09198a2ca218bf5f84cf73df8b03c97ceb3a88afecd2ae5ee8d6c19cdd55551cc4f7ef4\
            2aaaaaaae659b716c2b511efde445bc9bf3c7dabbede7eb5802381db816702b880c4ffa6bee0fc9fbe5efe2d803982b8\
            820903e881167f6ebe5b7b19bda8fd6b2aaaaaaa805c03fc82f3059f8188ff18bd9778e5bca1fc5c809185d684470813\
            822a7e92bc78f5b3bb297adc80e5888b86330b9a830e7dc1bad4f114b90c78bf2aaaaaaa81670c8288fb10ab844efc81\
            b86eea62b603f5c7822d12508d0217f086117a95b4ed60b0b1a8f19b835d9acb92d7225f888a77a4afccd2a8ab68ebbf\
            2aaaaaaa852f272f9b48b1588c01f3297fffd09d7fffbe58a271638887f2b94e40004f00400046d290deec587fffbab6\
            7fffa0f47fffab28ac03984a400053c62aaaaaaa4000725b400065a197ad62027fff9ab97fff766c7fff8643a3f72502\
            40007a6f4000a582400091d0400042617fffd25f7fff6c007fff38e67fff515d\
        ";
        /// libmseed `reference-testdata-int16.mseed2` (220 samples = SINE[..220])
        pub const INT16_REF: &str = "\
            30303030303052205445535420202042485a585807dc008500000000000000dc00280001000000010000000000380030\
            03e800000101090000000006000a000a00060000fff9fff4fff4fff800000008000e000f00090000fff6ffefffeefff5\
            0000000d00150015000d0000fff0ffe6ffe6fff000010013001f001f0013ffffffe9ffdbffdbffe90002001c002d002d\
            001bfffeffdeffcaffcaffe000030029004100410026fffbffceffb1ffb2ffd20006003d005f005e0037fff8ffb6ff8d\
            ff8fffbf000b005a008a0087004efff2ff93ff59ff5effa40013008400c900c2006effe8ff60ff0eff17ff7d001f00c1\
            01230118009cffd9ff16fea1feb0ff470032011c01a6019200dcffc1fea9fe04fe1dfefa004f01a0026402430137ff9c\
            fe09fd1ffd4afe8e007d02610377034001b8ff63fd1ffbd4fc1bfdf600c4037c050504ab026cff0bfbc8f9f5fa68fd20\
            0132051a074606b4036afe83f9d4f73ff7f7fbf301d907780a8909a104cffdb4f6f7f353f476fa4c02da0aed0f410dd3\
            06c4fc77f2c9eda5ef70f7fb04610ffb161713d80981fa95ecade56ce83af4bc06b2175d1ffa1c7b0d58f7bae3c1d988\
            dde2f0320a3622262e4828dd12b7f367d6b8c853cf0ee9d90f8731e642fa3a9e1a37ecdfc3b0af6fb9cde0fc178d48e5\
            60e8541124aee306a7e88b709b55d4a300000000000000000000000000000000\
        ";
        /// libmseed `reference-testdata-int32.mseed2`, record 0 (114 samples = SINE[..114])
        pub const INT32_REF: &str = "\
            30303030303052205445535420202042485a585807dc0085000000000000007200280001000000010000000000380030\
            03e800000301090000000000000000060000000a0000000a0000000600000000fffffff9fffffff4fffffff4fffffff8\
            00000000000000080000000e0000000f0000000900000000fffffff6ffffffefffffffeefffffff5000000000000000d\
            00000015000000150000000d00000000fffffff0ffffffe6ffffffe6fffffff000000001000000130000001f0000001f\
            00000013ffffffffffffffe9ffffffdbffffffdbffffffe9000000020000001c0000002d0000002d0000001bfffffffe\
            ffffffdeffffffcaffffffcaffffffe00000000300000029000000410000004100000026fffffffbffffffceffffffb1\
            ffffffb2ffffffd2000000060000003d0000005f0000005e00000037fffffff8ffffffb6ffffff8dffffff8fffffffbf\
            0000000b0000005a0000008a000000870000004efffffff2ffffff93ffffff59ffffff5effffffa40000001300000084\
            000000c9000000c20000006effffffe8ffffff60ffffff0effffff17ffffff7d0000001f000000c10000012300000118\
            0000009cffffffd9ffffff16fffffea1fffffeb0ffffff47000000320000011c000001a600000192000000dcffffffc1\
            fffffea9fffffe04fffffe1dfffffefa0000004f000001a00000026400000243\
        ";
        /// A live SeedLink packet captured from rtserve.iris.washington.edu (IU.ANMO.00.BHZ, Steim2, 554 samples): 8-byte `SL` header + 512-byte record
        pub const IRIS_SL_PACKET: &str = "\
            534c3835373835303430323534314420414e4d4f20303042485a495507ea012001170f0022f1022a0028000100200002\
            000000000040003003e800380b01090003e900006426460703fdfff7000002ac0000048d42e480ca47e11d034235840e\
            4620853f0508070944558bef50038d5f40f175087847dc0b7e5f217a48112c0300fd090574466cc73ffbffff783f2ffc\
            57d2f34b8ed712eb540b88418cc5da4fbffe00017182fb3c8f4b91f96d906e2207b7cfbb02bb9f79795edf2435cfe036\
            660dc73d65bedad23fffff7f4198f2b23bf73fbf2af02bb9413f76386fcdd3bd718d0f794baff68541cded5c72800c7c\
            521da9627ea095fb080b010e0e589f540928e1d30930f3953fffffef0d2cc44f0a5d62511958c15018182755094c938f\
            0a2c55450838344354e2a887072c6f50460500fa7690830559cefb9e7fffffff71c0dbff30079cc13ff7ffff39e74eb7\
            2bf35bb632df5b6e2ecadbee29b6fbe8f3efe7fa298b0aa62ccad92934925d2431ba5d6e2ecaccb324cfab712dcf6c3b\
            33b75e7439ef20b73fffffff2e13bf848af23fd37c22a85d4e11982c54142d2c0620f34a1054a3580f3d24530a5160dd\
            1c1145d21339d58d1739744a172d158f114924cc0d3d31cc3ffefffb163d23920c40cf940f08c44f063100c658929cec\
            53f704abff8040048e6f6e74723d67f6486e74bf7011ea247d81dbdb73d97f73ffbfebf96d4bee71\
        ";
    }
}
