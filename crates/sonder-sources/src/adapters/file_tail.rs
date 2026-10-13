//! `file-tail` — follow a growing file like `tail -f`: JSON lines, CSV, or plain lines.

use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value as Json;
use sonder_core::{Datum, SourceSpec, SourceState, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::adapters::generic::{Converter, EventsOn, GenericConfig};
use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jsonl,
    Csv,
    Lines,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "jsonl" | "json" | "ndjson" => Format::Jsonl,
            "csv" => Format::Csv,
            "lines" | "text" | "txt" => Format::Lines,
            _ => return None,
        })
    }
}

/// Converts lines of a given format into data. Holds the CSV header once seen.
#[derive(Debug)]
pub struct LineConverter {
    pub format: Format,
    pub conv: Converter,
    pub csv_header: Option<Vec<String>>,
}

impl LineConverter {
    pub fn new(format: Format, cfg: GenericConfig) -> Self {
        Self { format, conv: Converter::new(cfg), csv_header: None }
    }

    /// Convert one line (without newline). Empty lines produce nothing.
    pub fn line(&mut self, line: &str) -> Vec<Datum> {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            return Vec::new();
        }
        match self.format {
            Format::Jsonl => match serde_json::from_str::<Json>(line) {
                Ok(v) => self.conv.convert(&v),
                Err(_) => Vec::new(),
            },
            Format::Csv => {
                let cells = split_csv(line);
                match &self.csv_header {
                    None => {
                        self.csv_header = Some(cells.into_iter().map(|c| c.trim().to_string()).collect());
                        Vec::new()
                    }
                    Some(header) => {
                        let mut obj = serde_json::Map::new();
                        for (h, c) in header.iter().zip(cells) {
                            let v = match c.trim().parse::<f64>() {
                                Ok(n) if n.is_finite() => Json::from(n),
                                _ => Json::String(c),
                            };
                            obj.insert(h.clone(), v);
                        }
                        self.conv.convert(&Json::Object(obj))
                    }
                }
            }
            Format::Lines => {
                let mut attrs = std::collections::BTreeMap::new();
                attrs.insert("text".to_string(), Value::Text(line.chars().take(512).collect()));
                attrs.insert("len".to_string(), Value::Num(line.len() as f64));
                vec![
                    Datum::Reading { values: [("len".to_string(), line.len() as f64)].into_iter().collect() },
                    Datum::Event { name: self.conv.cfg.event_name.clone(), intensity: 1.0, attrs },
                ]
            }
        }
    }
}

/// Minimal CSV split with double-quote support.
pub fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_q && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_q = !in_q,
            ',' if !in_q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

struct FileTailSource {
    path: PathBuf,
    format: Format,
    from_start: bool,
    cfg: GenericConfig,
    poll: Duration,
}

impl Source for FileTailSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let mut em = Emitter::new(ctx);
            let mut lc = LineConverter::new(self.format, self.cfg.clone());
            em.status(SourceState::Connecting, format!("waiting for {}", self.path.display())).await;
            let mut pos: Option<u64> = None;
            let mut partial = String::new();
            loop {
                if em.is_shutdown() {
                    return;
                }
                let meta = match tokio::fs::metadata(&self.path).await {
                    Ok(m) => m,
                    Err(_) => {
                        em.status(SourceState::Reconnecting, format!("{} not found; waiting", self.path.display())).await;
                        pos = None;
                        if !em.sleep(self.poll.max(Duration::from_secs(1))).await {
                            return;
                        }
                        continue;
                    }
                };
                let len = meta.len();
                let start = match pos {
                    Some(p) if p <= len => p,
                    Some(_) => {
                        // Truncated/rotated: start over.
                        partial.clear();
                        lc.csv_header = None;
                        0
                    }
                    None => {
                        if self.from_start {
                            0
                        } else {
                            len
                        }
                    }
                };
                if len > start {
                    match read_from(&self.path, start, len - start).await {
                        Ok(bytes) => {
                            partial.push_str(&String::from_utf8_lossy(&bytes));
                            let mut emitted = 0;
                            while let Some(nl) = partial.find('\n') {
                                let line = partial[..nl].to_string();
                                partial.drain(..=nl);
                                for d in lc.line(&line) {
                                    emitted += 1;
                                    em.datum(d).await;
                                }
                            }
                            if emitted > 0 {
                                em.status(SourceState::Live, "live").await;
                            }
                            pos = Some(len);
                        }
                        Err(e) => {
                            em.status(SourceState::Reconnecting, format!("read failed: {e}")).await;
                        }
                    }
                } else {
                    pos = Some(start);
                    if em.state() == Some(SourceState::Connecting) {
                        em.status(SourceState::Idle, "following").await;
                    }
                }
                if !em.sleep(self.poll).await {
                    return;
                }
            }
        })
    }
}

async fn read_from(path: &std::path::Path, start: u64, n: u64) -> std::io::Result<Vec<u8>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(start)).await?;
    let mut buf = Vec::with_capacity(n.min(8 * 1024 * 1024) as usize);
    let mut limited = f.take(n.min(64 * 1024 * 1024));
    limited.read_to_end(&mut buf).await?;
    Ok(buf)
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "file-tail",
        summary: "Follow a growing file (`tail -f`): JSON lines, CSV (header row → fields) or plain lines",
        params: vec![
            ParamInfo { name: "path", summary: "File path", default: None },
            ParamInfo { name: "format", summary: "jsonl | csv | lines", default: Some("jsonl") },
            ParamInfo { name: "from_start", summary: "Read existing content first", default: Some("false") },
            ParamInfo { name: "fields", summary: "Paths or \"auto\"", default: Some("auto") },
            ParamInfo { name: "events_on", summary: "change | every | none", default: Some("every") },
            ParamInfo { name: "event_name", summary: "Name of the emitted event", default: Some("line") },
            ParamInfo { name: "poll", summary: "Seconds between checks for new data", default: Some("0.25") },
        ],
        channels: vec!["<numeric fields>", "len"],
        events: vec!["line"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let path = PathBuf::from(crate::require_str(spec, "path")?);
    let format = Format::parse(&util::param_str(spec, "format", "jsonl"))
        .ok_or_else(|| util::invalid(spec, "format", "expected jsonl | csv | lines"))?;
    Ok(Box::new(FileTailSource {
        path,
        format,
        from_start: util::param_bool(spec, "from_start", false)?,
        cfg: GenericConfig::from_spec(spec, EventsOn::Every, "line")?,
        poll: util::param_secs(spec, "poll", 0.25, 0.05)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GenericConfig {
        GenericConfig {
            fields: crate::adapters::generic::Fields::Auto,
            events_on: EventsOn::Every,
            event_name: "line".into(),
            data_path: None,
            filter: None,
        }
    }

    #[test]
    fn csv_header_then_rows() {
        let mut lc = LineConverter::new(Format::Csv, cfg());
        assert!(lc.line("time,temp,\"label, quoted\"").is_empty());
        let d = lc.line("1,21.5,\"a \"\"b\"\"\"");
        assert_eq!(d.len(), 2);
        match &d[0] {
            Datum::Reading { values } => {
                assert_eq!(values["temp"], 21.5);
                assert_eq!(values["time"], 1.0);
                assert!(!values.contains_key("label, quoted"));
            }
            _ => panic!(),
        }
        match &d[1] {
            Datum::Event { attrs, .. } => assert_eq!(attrs["label, quoted"], Value::Text("a \"b\"".into())),
            _ => panic!(),
        }
    }

    #[test]
    fn jsonl_and_lines() {
        let mut lc = LineConverter::new(Format::Jsonl, cfg());
        assert_eq!(lc.line(r#"{"x": 1, "s": "hi"}"#).len(), 2);
        assert!(lc.line("not json").is_empty());
        assert!(lc.line("").is_empty());
        let mut ll = LineConverter::new(Format::Lines, cfg());
        let d = ll.line("hello world\r");
        assert_eq!(d.len(), 2);
        match &d[1] {
            Datum::Event { name, attrs, .. } => {
                assert_eq!(name, "line");
                assert_eq!(attrs["len"], Value::Num(11.0));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn tails_appended_lines() {
        let dir = std::env::temp_dir().join(format!("sonder-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.jsonl");
        std::fs::write(&path, "{\"old\": 1}\n").unwrap();
        let spec = SourceSpec::new("file-tail").with("path", path.to_string_lossy().to_string()).with("poll", 0.05);
        let src = factory(&spec).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let (stx, srx) = tokio::sync::watch::channel(false);
        let ctx = SourceCtx {
            id: sonder_core::SourceId::new("t"),
            tx,
            clock: std::sync::Arc::new(sonder_core::WallClock::new()),
            http: reqwest::Client::new(),
            shutdown: srx,
            seed: 1,
        };
        let task = tokio::spawn(src.run(ctx));
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{{\"x\": 2.5}}").unwrap();
        }
        let mut got_reading = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !got_reading {
            if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                if let Datum::Reading { values } = &s.datum {
                    assert!(!values.contains_key("old"), "from_start=false must skip existing content");
                    assert_eq!(values.get("x"), Some(&2.5));
                    got_reading = true;
                }
            }
        }
        assert!(got_reading);
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
