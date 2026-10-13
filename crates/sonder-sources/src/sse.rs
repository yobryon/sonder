//! Hand-rolled Server-Sent Events framing.
//!
//! Feed raw bytes with [`SseParser::push`]; complete events come out as
//! [`SseEvent`]s. Handles `data:` continuation (multi-line joined with `\n`),
//! `event:`, `id:`, `retry:`, comments (`:` lines) and CRLF.

/// One parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: String,
    pub retry_ms: Option<u64>,
}

/// Incremental SSE parser.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    cur_event: Option<String>,
    cur_id: Option<String>,
    cur_data: Vec<String>,
    cur_retry: Option<u64>,
    /// Last `id:` seen, for `Last-Event-ID` on reconnect.
    pub last_id: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        // Guard against a runaway line without newlines.
        if self.buf.len() > 16 * 1024 * 1024 {
            self.buf.clear();
            self.reset_event();
        }
        let mut out = Vec::new();
        loop {
            let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else { break };
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let mut line = &line[..line.len() - 1];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            if let Some(ev) = self.feed_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    fn reset_event(&mut self) {
        self.cur_event = None;
        self.cur_id = None;
        self.cur_data.clear();
        self.cur_retry = None;
    }

    /// Process one line (without its terminator). Returns an event on a blank line
    /// if any data was accumulated.
    pub fn feed_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            if self.cur_data.is_empty() && self.cur_event.is_none() && self.cur_id.is_none() {
                self.cur_retry = None;
                return None;
            }
            let ev = SseEvent {
                event: self.cur_event.take(),
                id: self.cur_id.take(),
                data: self.cur_data.join("\n"),
                retry_ms: self.cur_retry.take(),
            };
            self.cur_data.clear();
            // Per spec, an event with no data is dropped.
            if ev.data.is_empty() {
                return None;
            }
            return Some(ev);
        }
        if line.starts_with(':') {
            return None; // comment / keepalive
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        match field {
            "data" => self.cur_data.push(value.to_string()),
            "event" => self.cur_event = Some(value.to_string()),
            "id" => {
                if !value.contains('\0') {
                    self.cur_id = Some(value.to_string());
                    self.last_id = Some(value.to_string());
                }
            }
            "retry" => self.cur_retry = value.trim().parse().ok(),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_incrementally() {
        let mut p = SseParser::new();
        let mut evs = p.push(b":ok\n\nevent: message\nid: 42\ndata: {\"a\":1,\n");
        assert!(evs.is_empty());
        evs.extend(p.push(b"data: \"b\":2}\n\r\ndata:x\n\n"));
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].id.as_deref(), Some("42"));
        assert_eq!(evs[0].data, "{\"a\":1,\n\"b\":2}");
        assert_eq!(evs[1].data, "x");
        assert_eq!(p.last_id.as_deref(), Some("42"));
    }

    #[test]
    fn wikimedia_style_frame() {
        let frame = "event: message\nid: [{\"topic\":\"eqiad.mediawiki.recentchange\",\"partition\":0,\"timestamp\":1700000000001},{\"topic\":\"codfw.mediawiki.recentchange\",\"partition\":0,\"offset\":-1}]\ndata: {\"$schema\":\"/mediawiki/recentchange/1.0.0\",\"type\":\"edit\",\"wiki\":\"enwiki\",\"length\":{\"old\":10,\"new\":30}}\n\n";
        let mut p = SseParser::new();
        let evs = p.push(frame.as_bytes());
        assert_eq!(evs.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&evs[0].data).unwrap();
        assert_eq!(v["wiki"], "enwiki");
    }

    #[test]
    fn ignores_empty_events_and_handles_retry() {
        let mut p = SseParser::new();
        let evs = p.push(b"event: ping\n\nretry: 3000\ndata: hi\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].retry_ms, Some(3000));
        assert_eq!(evs[0].data, "hi");
    }
}
