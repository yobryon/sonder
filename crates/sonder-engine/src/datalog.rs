//! The `.sonder` data log: every sample the engine ingested, so a session can be replayed
//! later — the *data*, not the audio (time travel, docs/V3.md §1).
//!
//! Format: JSON Lines. Line 1 is a [`LogHeader`]; each following line is one sample,
//! `{"t": <engine seconds>, "s": "<source name>", "d": <Datum JSON>}`. `Datum::Audio`
//! chunks are never logged (far too big); instead a single
//! `{"t","s","d":{"type":"audio_dropped"}}` marker per source says they existed.
//! `Datum::Viz` payloads are kept (they are small and the UI wants them back).
//!
//! The reader is pure (works in wasm): it takes bytes and tolerates a truncated last line
//! (a recorder that was killed mid-write). Gzip is detected by its magic bytes; inflating
//! needs the `native` feature — the browser inflates with `DecompressionStream` first.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sonder_core::{Datum, Sample, Seconds, SourceId, SourceSpec};

/// Format version written in the header's `sonder` field.
pub const FORMAT_VERSION: u32 = 1;

/// A source the recorded scene ran, as written in the header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogSource {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub params: BTreeMap<String, serde_json::Value>,
}

/// Line 1 of a log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogHeader {
    /// Format version ([`FORMAT_VERSION`]).
    pub sonder: u32,
    /// The scene's `name` (its title, e.g. "Night Freeway").
    pub scene: String,
    pub seed: u64,
    /// Wall-clock Unix seconds of engine time 0.
    pub epoch_unix: f64,
    /// RFC 3339 timestamp of `epoch_unix` (for humans).
    pub created: String,
    #[serde(default)]
    pub sources: Vec<LogSource>,
}

impl LogHeader {
    /// A header for `scene`/`seed` whose engine clock started at `epoch_unix`.
    pub fn new(scene: &str, seed: u64, epoch_unix: f64, sources: &[(SourceId, SourceSpec)]) -> Self {
        Self {
            sonder: FORMAT_VERSION,
            scene: scene.to_string(),
            seed,
            epoch_unix,
            created: rfc3339(epoch_unix),
            sources: sources
                .iter()
                .map(|(id, spec)| LogSource { name: id.to_string(), kind: spec.kind.clone(), params: spec.params.clone() })
                .collect(),
        }
    }
}

/// One logged sample.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub t: Seconds,
    pub source: SourceId,
    pub datum: Datum,
}

/// Wire form of a sample line (`d` stays raw so markers can be told apart from data).
#[derive(Serialize, Deserialize)]
struct Line<'a> {
    t: f64,
    s: std::borrow::Cow<'a, str>,
    d: serde_json::Value,
}

const AUDIO_DROPPED: &str = "audio_dropped";

/// Serialize one sample line (no trailing newline). `None` for audio (use [`audio_marker`]).
pub fn sample_line(sample: &Sample) -> Option<String> {
    if matches!(sample.datum, Datum::Audio { .. }) {
        return None;
    }
    let d = serde_json::to_value(&sample.datum).ok()?;
    serde_json::to_string(&Line { t: sample.at, s: sample.source.as_str().into(), d }).ok()
}

/// The once-per-source line that stands in for audio chunks.
pub fn audio_marker(source: &SourceId, t: Seconds) -> String {
    serde_json::to_string(&Line { t, s: source.as_str().into(), d: serde_json::json!({ "type": AUDIO_DROPPED }) }).unwrap_or_default()
}

/// Is this the gzip magic?
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("empty log")]
    Empty,
    #[error("bad log header: {0}")]
    Header(String),
    #[error("log format version {0} is newer than this build understands ({FORMAT_VERSION})")]
    Version(u32),
    #[error("the log is gzip-compressed and this build cannot inflate it (inflate it first)")]
    Gzip,
    #[error("{0}")]
    Io(String),
}

/// A whole log in memory, entries sorted by time.
#[derive(Debug, Clone, PartialEq)]
pub struct DataLog {
    header: LogHeader,
    entries: Vec<LogEntry>,
    /// Sources whose audio was dropped at record time.
    audio_dropped: Vec<SourceId>,
    /// Lines that did not parse (excluding a truncated last line).
    skipped: usize,
}

impl DataLog {
    /// Parse a log from its bytes (plain JSONL, or gzip when the feature allows).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LogError> {
        if is_gzip(bytes) {
            #[cfg(feature = "native")]
            {
                use std::io::Read;
                let mut out = Vec::new();
                // A truncated gzip stream (recorder killed) still yields what it had before
                // the cut; only a stream that gave us nothing is an error.
                if let Err(e) = flate2::read::MultiGzDecoder::new(bytes).read_to_end(&mut out) {
                    if out.is_empty() {
                        return Err(LogError::Io(format!("inflating: {e}")));
                    }
                }
                return Self::from_text(&String::from_utf8_lossy(&out));
            }
            #[cfg(not(feature = "native"))]
            return Err(LogError::Gzip);
        }
        Self::from_text(&String::from_utf8_lossy(bytes))
    }

    /// Parse a log from JSONL text.
    pub fn from_text(text: &str) -> Result<Self, LogError> {
        let mut lines = text.split_inclusive('\n');
        let first = lines.next().ok_or(LogError::Empty)?;
        if first.trim().is_empty() {
            return Err(LogError::Empty);
        }
        let header: LogHeader = serde_json::from_str(first.trim_end()).map_err(|e| LogError::Header(e.to_string()))?;
        if header.sonder > FORMAT_VERSION {
            return Err(LogError::Version(header.sonder));
        }
        let mut entries = Vec::new();
        let mut audio_dropped = Vec::new();
        let mut skipped = 0;
        for raw in lines {
            let complete = raw.ends_with('\n');
            let line = raw.trim_end();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Line>(line) {
                Ok(l) => {
                    let sid = SourceId::new(l.s.as_ref());
                    if l.d.get("type").and_then(|v| v.as_str()) == Some(AUDIO_DROPPED) {
                        if !audio_dropped.contains(&sid) {
                            audio_dropped.push(sid);
                        }
                        continue;
                    }
                    match serde_json::from_value::<Datum>(l.d) {
                        Ok(datum) if l.t.is_finite() => entries.push(LogEntry { t: l.t, source: sid, datum }),
                        _ => skipped += 1,
                    }
                }
                // A cut-off final line is expected (recorder killed); anything else counts.
                Err(_) if !complete => {}
                Err(_) => skipped += 1,
            }
        }
        entries.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap_or(std::cmp::Ordering::Equal));
        Ok(Self { header, entries, audio_dropped, skipped })
    }

    /// Read a log file (gzip detected by content, not by name).
    #[cfg(feature = "native")]
    pub fn open(path: &std::path::Path) -> Result<Self, LogError> {
        let bytes = std::fs::read(path).map_err(|e| LogError::Io(format!("reading {}: {e}", path.display())))?;
        Self::from_bytes(&bytes)
    }

    pub fn header(&self) -> &LogHeader {
        &self.header
    }
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }
    /// Engine time of the last sample (0 for an empty log).
    pub fn length_seconds(&self) -> f64 {
        self.entries.last().map(|e| e.t).unwrap_or(0.0)
    }
    pub fn audio_dropped(&self) -> &[SourceId] {
        &self.audio_dropped
    }
    pub fn skipped_lines(&self) -> usize {
        self.skipped
    }
    /// Distinct source names in time-of-first-appearance order.
    pub fn source_names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            if !out.iter().any(|n| n == e.source.as_str()) {
                out.push(e.source.to_string());
            }
        }
        out
    }
    /// Index of the first entry with `t >= at`.
    pub fn index_at(&self, at: f64) -> usize {
        self.entries.partition_point(|e| e.t < at)
    }
    /// Iterator over `(t, source, datum)`.
    pub fn iter(&self) -> impl Iterator<Item = (Seconds, &SourceId, &Datum)> + '_ {
        self.entries.iter().map(|e| (e.t, &e.source, &e.datum))
    }
}

/// Convenience: parse from bytes into a shared handle.
pub fn load_bytes(bytes: &[u8]) -> Result<Arc<DataLog>, LogError> {
    DataLog::from_bytes(bytes).map(Arc::new)
}

// ---------------------------------------------------------------------------
// Writer (native)
// ---------------------------------------------------------------------------

/// Appends samples to a `.sonder` (or `.sonder.gz`) file. Buffered; flushed every second
/// of wall time and on drop. Write errors are reported once and then ignored, so a full
/// disk never stops the music.
#[cfg(feature = "native")]
pub struct DataLogWriter {
    out: Option<Box<dyn std::io::Write + Send>>,
    path: std::path::PathBuf,
    last_flush: std::time::Instant,
    audio_marked: std::collections::HashSet<SourceId>,
    lines: u64,
    failed: bool,
}

#[cfg(feature = "native")]
impl DataLogWriter {
    /// Create the file (parent directories too) and write the header.
    pub fn create(path: &std::path::Path, header: &LogHeader) -> std::io::Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::io::BufWriter::with_capacity(1 << 16, std::fs::File::create(path)?);
        let gz = path.extension().is_some_and(|e| e == "gz");
        let mut out: Box<dyn std::io::Write + Send> =
            if gz { Box::new(flate2::write::GzEncoder::new(file, flate2::Compression::default())) } else { Box::new(file) };
        let mut line = serde_json::to_string(header).map_err(std::io::Error::other)?;
        line.push('\n');
        out.write_all(line.as_bytes())?;
        Ok(Self {
            out: Some(out),
            path: path.to_path_buf(),
            last_flush: std::time::Instant::now(),
            audio_marked: std::collections::HashSet::new(),
            lines: 0,
            failed: false,
        })
    }

    /// Log one sample (audio becomes a one-time marker).
    pub fn write(&mut self, sample: &Sample) {
        if self.failed {
            return;
        }
        let line = match &sample.datum {
            Datum::Audio { .. } => {
                if !self.audio_marked.insert(sample.source.clone()) {
                    return;
                }
                audio_marker(&sample.source, sample.at)
            }
            _ => match sample_line(sample) {
                Some(l) => l,
                None => return,
            },
        };
        let res = (|| -> std::io::Result<()> {
            let out = self.out.as_mut().expect("writer open");
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
            if self.last_flush.elapsed() >= std::time::Duration::from_secs(1) {
                out.flush()?;
                self.last_flush = std::time::Instant::now();
            }
            Ok(())
        })();
        match res {
            Ok(()) => self.lines += 1,
            Err(e) => {
                self.failed = true;
                tracing::error!(path = %self.path.display(), "data log write failed: {e}; recording stopped");
            }
        }
    }

    /// Sample lines written so far.
    pub fn lines(&self) -> u64 {
        self.lines
    }

    /// Flush and close (also happens on drop).
    pub fn finish(&mut self) -> std::io::Result<()> {
        if let Some(mut out) = self.out.take() {
            out.flush()?;
        }
        Ok(())
    }
}

#[cfg(feature = "native")]
impl Drop for DataLogWriter {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            tracing::error!(path = %self.path.display(), "closing data log: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Time formatting (no chrono here: the engine is wasm-friendly)
// ---------------------------------------------------------------------------

/// Unix seconds → `2026-10-15T04:17:09Z` (seconds precision, UTC).
pub fn rfc3339(unix: f64) -> String {
    if !unix.is_finite() {
        return String::new();
    }
    let secs = unix.floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current Unix time in seconds (0 if the clock is before 1970).
#[cfg(feature = "native")]
pub fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::SourceState;

    fn samples() -> Vec<Sample> {
        let a = SourceId::new("traffic");
        let b = SourceId::new("quakes");
        vec![
            Sample::status(&a, 0.0, SourceState::Live, "sim"),
            Sample::reading(&a, 0.02, [("count", 3.0), ("mean_speed", 61.5)]),
            Sample::event(&b, 0.04, "quake", 0.7).with_attr("mag", 4.1).with_attr("place", "off Honshu").with_attr("deep", true),
            Sample::viz(&a, 0.1, serde_json::json!({"cars": [{"x": 0.5, "lane": 1}]})),
            Sample { source: a.clone(), at: 0.12, datum: Datum::Audio { rate: 48_000, samples: vec![0.1; 960] } },
            Sample { source: a.clone(), at: 0.14, datum: Datum::Audio { rate: 48_000, samples: vec![0.2; 960] } },
            Sample::event(&a, 1.5, "car", 0.3).with_attr("lane", 2.0),
        ]
    }

    fn text_log() -> String {
        let header = LogHeader::new("Night Freeway", 42, 1_760_500_000.0, &[(SourceId::new("traffic"), SourceSpec::new("synthetic.freeway").with("lanes", 4))]);
        let mut s = serde_json::to_string(&header).unwrap();
        s.push('\n');
        let mut marked = false;
        for smp in samples() {
            match sample_line(&smp) {
                Some(l) => {
                    s.push_str(&l);
                    s.push('\n');
                }
                None if !marked => {
                    marked = true;
                    s.push_str(&audio_marker(&smp.source, smp.at));
                    s.push('\n');
                }
                None => {}
            }
        }
        s
    }

    #[test]
    fn round_trip_keeps_every_non_audio_sample_in_order() {
        let text = text_log();
        assert_eq!(text.lines().count(), 1 + 5 + 1, "header, five data lines, one audio marker:\n{text}");
        let log = DataLog::from_text(&text).unwrap();
        assert_eq!(log.header().scene, "Night Freeway");
        assert_eq!(log.header().seed, 42);
        assert_eq!(log.header().created, "2025-10-15T03:46:40Z");
        assert_eq!(log.header().sources[0].kind, "synthetic.freeway");
        assert_eq!(log.header().sources[0].params["lanes"], 4);
        let want: Vec<Sample> = samples().into_iter().filter(|s| !matches!(s.datum, Datum::Audio { .. })).collect();
        assert_eq!(log.entries().len(), want.len());
        for (e, w) in log.entries().iter().zip(&want) {
            assert_eq!(e.t, w.at);
            assert_eq!(e.source, w.source);
            assert_eq!(e.datum, w.datum);
        }
        assert_eq!(log.audio_dropped(), &[SourceId::new("traffic")]);
        assert_eq!(log.length_seconds(), 1.5);
        assert_eq!(log.source_names(), vec!["traffic".to_string(), "quakes".to_string()]);
        assert_eq!(log.index_at(0.04), 2);
        assert_eq!(log.index_at(0.05), 3);
        assert_eq!(log.index_at(9.0), 5);
        assert_eq!(log.skipped_lines(), 0);
    }

    #[test]
    fn truncated_last_line_is_tolerated_and_garbage_is_counted() {
        let text = text_log();
        let cut = &text[..text.len() - 7];
        assert!(!cut.ends_with('\n'));
        let log = DataLog::from_text(cut).unwrap();
        assert_eq!(log.entries().len(), 4, "the cut line is dropped silently");
        assert_eq!(log.skipped_lines(), 0);
        let garbage = format!("{}not json\n{{\"t\":1,\"s\":\"x\",\"d\":{{\"type\":\"nope\"}}}}\n", text);
        let log = DataLog::from_text(&garbage).unwrap();
        assert_eq!(log.entries().len(), 5);
        assert_eq!(log.skipped_lines(), 2);
        assert!(matches!(DataLog::from_text(""), Err(LogError::Empty)));
        assert!(matches!(DataLog::from_text("{\"nope\":1}\n"), Err(LogError::Header(_))));
        assert!(matches!(DataLog::from_text("{\"sonder\":99,\"scene\":\"x\",\"seed\":1,\"epoch_unix\":0,\"created\":\"\"}\n"), Err(LogError::Version(99))));
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_760_500_000.9), "2025-10-15T03:46:40Z");
        assert_eq!(rfc3339(1_792_036_800.0), "2026-10-15T04:00:00Z");
        assert_eq!(rfc3339(f64::NAN), "");
    }

    #[cfg(feature = "native")]
    #[test]
    fn writer_round_trips_plain_and_gzip() {
        let dir = std::env::temp_dir().join(format!("sonder-datalog-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        for name in ["a.sonder", "b.sonder.gz"] {
            let path = dir.join(name);
            let header = LogHeader::new("Night Freeway", 7, 1.0, &[]);
            {
                let mut w = DataLogWriter::create(&path, &header).unwrap();
                for s in samples() {
                    w.write(&s);
                }
                assert_eq!(w.lines(), 6, "{name}: five data lines + one audio marker");
            }
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(is_gzip(&bytes), name.ends_with(".gz"), "{name}");
            let log = DataLog::open(&path).unwrap();
            assert_eq!(log.entries().len(), 5, "{name}");
            assert_eq!(log.audio_dropped().len(), 1);
            assert_eq!(log.header().seed, 7);
            assert_eq!(log.length_seconds(), 1.5);
        }
        // A gzip member cut mid-stream still yields the complete lines before the cut.
        let gz = std::fs::read(dir.join("b.sonder.gz")).unwrap();
        let cut = &gz[..gz.len() - 12];
        let log = DataLog::from_bytes(cut).unwrap();
        assert!(log.entries().len() >= 3, "{}", log.entries().len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
