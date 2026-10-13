//! MJPEG framing: turn a byte stream into complete JPEG frames.
//!
//! Two shapes of input are handled by one state machine:
//!
//! * `multipart/x-mixed-replace` (what IP cameras serve over HTTP): parts
//!   separated by `--boundary` lines with headers. When a part carries
//!   `Content-Length` we take exactly that many bytes (fast path, validated by
//!   checking the EOI marker); otherwise we fall through to structural scanning.
//! * raw concatenated JPEGs with no framing at all (`ffmpeg -f mjpeg -`).
//!
//! Structural scanning walks the JPEG marker segments (so an EXIF thumbnail's
//! embedded `FF D9` does not truncate a frame) and the entropy-coded data to
//! the real EOI. Anything that is not a JPEG — boundary lines, headers, line
//! noise — is skipped while looking for the next SOI (`FF D8 FF`). A frame that
//! turns out to be malformed, or grows past `max_frame`, is dropped and parsing
//! resyncs on the next SOI.
//!
//! [`Parser`] is sans-IO (push bytes, pull frames) and [`FrameStream`] drives it
//! from a `Stream<Item = Result<Bytes, E>>` (a reqwest body) or any `AsyncRead`
//! (stdin).

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use futures::stream::{Stream, StreamExt};
use tokio::io::{AsyncRead, ReadBuf};

use crate::VisionError;

/// Default upper bound on a single frame (bytes).
pub const DEFAULT_MAX_FRAME: usize = 8 << 20;
/// How much non-JPEG prefix (part headers) we keep while waiting for an SOI.
const KEEP_PREFIX: usize = 1024;

/// Extract `boundary=...` from a `Content-Type` header value.
pub fn boundary_from_content_type(ct: &str) -> Option<String> {
    for part in ct.split(';').skip(1) {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("boundary") {
            let v = v.trim().trim_matches('"');
            let v = v.strip_prefix("--").unwrap_or(v);
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Parser counters, for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Complete frames emitted.
    pub frames: u64,
    /// Frames emitted via the `Content-Length` fast path.
    pub fast_path: u64,
    /// Times a frame was abandoned because its structure was invalid.
    pub resyncs: u64,
    /// Frames dropped for exceeding `max_frame`.
    pub dropped: u64,
    /// Bytes skipped between frames (headers, boundaries, garbage).
    pub skipped_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Looking for an SOI; bytes before it are headers/garbage.
    Seek,
    /// A frame starts at `buf[0]`; expecting exactly `len` bytes.
    Fixed { len: usize },
    /// A frame starts at `buf[0]`; structural scan resumes at `cursor`.
    Scan { cursor: usize, entropy: bool },
}

enum ScanResult {
    Complete(usize),
    Incomplete,
    Invalid,
}

/// Incremental MJPEG framer. Push bytes, pull complete JPEG frames.
pub struct Parser {
    buf: BytesMut,
    state: State,
    max_frame: usize,
    boundary: Option<String>,
    stats: Stats,
}

impl fmt::Debug for Parser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parser")
            .field("buffered", &self.buf.len())
            .field("state", &self.state)
            .field("max_frame", &self.max_frame)
            .field("boundary", &self.boundary)
            .field("stats", &self.stats)
            .finish()
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self { buf: BytesMut::with_capacity(64 << 10), state: State::Seek, max_frame: DEFAULT_MAX_FRAME, boundary: None, stats: Stats::default() }
    }

    /// Record the multipart boundary from a `Content-Type` header, if any. The
    /// framer does not need it (it is SOI/EOI driven) but it is exposed for
    /// diagnostics.
    pub fn with_content_type(mut self, content_type: Option<&str>) -> Self {
        self.boundary = content_type.and_then(boundary_from_content_type);
        self
    }

    /// Set the largest frame accepted (bigger frames are dropped).
    pub fn with_max_frame(mut self, max_frame: usize) -> Self {
        self.max_frame = max_frame.max(1024);
        self
    }

    pub fn boundary(&self) -> Option<&str> {
        self.boundary.as_deref()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Bytes currently buffered.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Feed more bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete frame, if the buffered data contains one.
    pub fn next_frame(&mut self) -> Option<Bytes> {
        loop {
            match self.state {
                State::Seek => {
                    let Some(pos) = find_soi(&self.buf) else {
                        // Keep a tail so a split SOI (and recent headers) survive.
                        let keep = self.buf.len().min(KEEP_PREFIX);
                        let drop = self.buf.len() - keep;
                        if drop > 0 {
                            self.stats.skipped_bytes += drop as u64;
                            self.buf.advance(drop);
                        }
                        return None;
                    };
                    let len = content_length(&self.buf[..pos]);
                    self.stats.skipped_bytes += pos as u64;
                    self.buf.advance(pos);
                    self.state = match len {
                        Some(n) if n >= 4 && n <= self.max_frame => State::Fixed { len: n },
                        _ => State::Scan { cursor: 2, entropy: false },
                    };
                }
                State::Fixed { len } => {
                    if self.buf.len() < len {
                        return None;
                    }
                    // Validate: strip trailing CR/LF/padding, must end in EOI.
                    let mut end = len;
                    while end > 2 && matches!(self.buf[end - 1], b'\r' | b'\n' | b' ' | 0) {
                        end -= 1;
                    }
                    if end >= 4 && self.buf[end - 2] == 0xFF && self.buf[end - 1] == 0xD9 {
                        let frame = self.buf.split_to(len).freeze().slice(..end);
                        self.stats.frames += 1;
                        self.stats.fast_path += 1;
                        self.state = State::Seek;
                        return Some(frame);
                    }
                    // Lying Content-Length: fall back to a structural scan.
                    self.state = State::Scan { cursor: 2, entropy: false };
                }
                State::Scan { cursor, entropy } => {
                    let (res, cursor, entropy) = scan(&self.buf, cursor, entropy);
                    match res {
                        ScanResult::Complete(end) => {
                            let frame = self.buf.split_to(end).freeze();
                            self.stats.frames += 1;
                            self.state = State::Seek;
                            return Some(frame);
                        }
                        ScanResult::Invalid => {
                            self.stats.resyncs += 1;
                            self.resync();
                        }
                        ScanResult::Incomplete => {
                            if self.buf.len() > self.max_frame {
                                self.stats.dropped += 1;
                                self.resync();
                            } else {
                                self.state = State::Scan { cursor, entropy };
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Signal end of input: a frame still waiting on a (lying) `Content-Length`
    /// is re-examined structurally so a complete trailing JPEG is not lost.
    pub fn finish(&mut self) -> Option<Bytes> {
        if let State::Fixed { .. } = self.state {
            self.state = State::Scan { cursor: 2, entropy: false };
        }
        self.next_frame()
    }

    /// Skip the SOI at `buf[0]` and look for the next one.
    fn resync(&mut self) {
        let skip = self.buf.len().min(2);
        self.stats.skipped_bytes += skip as u64;
        self.buf.advance(skip);
        self.state = State::Seek;
    }
}

/// Position of the first `FF D8 FF` in `buf`.
fn find_soi(buf: &[u8]) -> Option<usize> {
    buf.windows(3).position(|w| w[0] == 0xFF && w[1] == 0xD8 && w[2] == 0xFF)
}

/// Parse the last `Content-Length:` header in a prefix (case-insensitive).
fn content_length(prefix: &[u8]) -> Option<usize> {
    const KEY: &[u8] = b"content-length:";
    let mut best = None;
    let mut i = 0;
    while i + KEY.len() <= prefix.len() {
        if prefix[i..i + KEY.len()].eq_ignore_ascii_case(KEY) {
            let rest = &prefix[i + KEY.len()..];
            let digits: String = rest.iter().skip_while(|b| **b == b' ' || **b == b'\t').take_while(|b| b.is_ascii_digit()).map(|&b| b as char).collect();
            if let Ok(n) = digits.parse::<usize>() {
                best = Some(n);
            }
            i += KEY.len();
        } else {
            i += 1;
        }
    }
    best
}

/// Resume a structural JPEG scan on `buf` (SOI at 0). Returns the result and
/// the position/phase to resume from if incomplete.
fn scan(buf: &[u8], mut cursor: usize, mut entropy: bool) -> (ScanResult, usize, bool) {
    let len = buf.len();
    loop {
        if entropy {
            if cursor >= len {
                return (ScanResult::Incomplete, cursor, true);
            }
            // Find a marker (FF followed by anything but 00 / RSTn / FF).
            let Some(off) = buf[cursor..].iter().position(|&b| b == 0xFF) else {
                return (ScanResult::Incomplete, len, true);
            };
            let i = cursor + off;
            if i + 1 >= len {
                return (ScanResult::Incomplete, i, true);
            }
            let m = buf[i + 1];
            if m == 0x00 || (0xD0..=0xD7).contains(&m) {
                cursor = i + 2;
            } else if m == 0xFF {
                cursor = i + 1;
            } else {
                entropy = false;
                cursor = i;
            }
            continue;
        }
        if cursor + 2 > len {
            return (ScanResult::Incomplete, cursor, false);
        }
        if buf[cursor] != 0xFF {
            return (ScanResult::Invalid, cursor, false);
        }
        let m = buf[cursor + 1];
        match m {
            0xFF => cursor += 1, // fill byte
            0xD9 => return (ScanResult::Complete(cursor + 2), cursor, false),
            0xD8 => {
                // A second SOI inside a frame: the first one was truncated.
                if cursor == 0 {
                    cursor += 2;
                } else {
                    return (ScanResult::Invalid, cursor, false);
                }
            }
            0x01 | 0xD0..=0xD7 => cursor += 2, // standalone markers
            0x00 => return (ScanResult::Invalid, cursor, false),
            _ => {
                if cursor + 4 > len {
                    return (ScanResult::Incomplete, cursor, false);
                }
                let seg = ((buf[cursor + 2] as usize) << 8) | buf[cursor + 3] as usize;
                if seg < 2 {
                    return (ScanResult::Invalid, cursor, false);
                }
                cursor += 2 + seg;
                if m == 0xDA {
                    entropy = true;
                }
            }
        }
    }
}

enum Inner {
    Stream(Pin<Box<dyn Stream<Item = Result<Bytes, VisionError>> + Send>>),
    Reader(Pin<Box<dyn AsyncRead + Send>>, Vec<u8>),
}

/// An async stream of complete JPEG frames (`Bytes`) parsed from an MJPEG
/// byte source.
pub struct FrameStream {
    inner: Inner,
    parser: Parser,
    done: bool,
}

impl fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameStream").field("parser", &self.parser).field("done", &self.done).finish()
    }
}

impl FrameStream {
    /// Wrap a chunked byte stream such as `reqwest::Response::bytes_stream()`.
    /// `content_type` is the response's `Content-Type`, if known.
    pub fn from_stream<S, E>(stream: S, content_type: Option<&str>) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: fmt::Display,
    {
        let mapped = stream.map(|r| r.map_err(|e| VisionError::Stream(e.to_string())));
        Self { inner: Inner::Stream(Box::pin(mapped)), parser: Parser::new().with_content_type(content_type), done: false }
    }

    /// Wrap an `AsyncRead` such as `tokio::io::stdin()`.
    pub fn from_reader<R>(reader: R, content_type: Option<&str>) -> Self
    where
        R: AsyncRead + Send + 'static,
    {
        Self { inner: Inner::Reader(Box::pin(reader), vec![0u8; 64 << 10]), parser: Parser::new().with_content_type(content_type), done: false }
    }

    /// Set the largest frame accepted.
    pub fn with_max_frame(mut self, max_frame: usize) -> Self {
        self.parser = std::mem::take(&mut self.parser).with_max_frame(max_frame);
        self
    }

    pub fn parser(&self) -> &Parser {
        &self.parser
    }
}

impl Stream for FrameStream {
    type Item = Result<Bytes, VisionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(frame) = self.parser.next_frame() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if self.done {
                return Poll::Ready(self.parser.finish().map(Ok));
            }
            let this = &mut *self;
            match &mut this.inner {
                Inner::Stream(s) => match s.as_mut().poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => this.done = true,
                    Poll::Ready(Some(Err(e))) => {
                        this.done = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(Some(Ok(chunk))) => this.parser.push(&chunk),
                },
                Inner::Reader(r, scratch) => {
                    let mut rb = ReadBuf::new(scratch.as_mut_slice());
                    match r.as_mut().poll_read(cx, &mut rb) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => {
                            this.done = true;
                            return Poll::Ready(Some(Err(VisionError::Stream(e.to_string()))));
                        }
                        Poll::Ready(Ok(())) => {
                            let n = rb.filled().len();
                            if n == 0 {
                                this.done = true;
                            } else {
                                let filled = rb.filled().to_vec();
                                this.parser.push(&filled);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;

    fn frames() -> Vec<Vec<u8>> {
        testgen::jpeg_sequence(5, 48, 32)
    }

    fn parse_all(data: &[u8], chunk: usize) -> (Vec<Bytes>, Stats) {
        let mut p = Parser::new();
        let mut out = Vec::new();
        for c in data.chunks(chunk.max(1)) {
            p.push(c);
            while let Some(f) = p.next_frame() {
                out.push(f);
            }
        }
        (out, p.stats())
    }

    #[test]
    fn boundary_parsing() {
        assert_eq!(boundary_from_content_type("multipart/x-mixed-replace; boundary=myboundary").as_deref(), Some("myboundary"));
        assert_eq!(boundary_from_content_type("multipart/x-mixed-replace;boundary=\"--abc\""), Some("abc".into()));
        assert_eq!(boundary_from_content_type("image/jpeg"), None);
    }

    #[test]
    fn multipart_with_content_length_uses_fast_path() {
        let fr = frames();
        let body = testgen::multipart(&fr, "frame", true);
        let (out, stats) = parse_all(&body, body.len());
        assert_eq!(out.len(), fr.len());
        for (a, b) in out.iter().zip(&fr) {
            assert_eq!(&a[..], &b[..]);
        }
        assert_eq!(stats.fast_path, fr.len() as u64);
        assert_eq!(stats.resyncs, 0);
    }

    #[test]
    fn multipart_without_content_length_scans() {
        let fr = frames();
        let body = testgen::multipart(&fr, "--boundarydonotcross", false);
        let (out, stats) = parse_all(&body, body.len());
        assert_eq!(out.len(), fr.len());
        for (a, b) in out.iter().zip(&fr) {
            assert_eq!(&a[..], &b[..]);
        }
        assert_eq!(stats.fast_path, 0);
        assert_eq!(stats.resyncs, 0);
    }

    #[test]
    fn raw_concatenated_jpegs() {
        let fr = frames();
        let body: Vec<u8> = fr.iter().flatten().copied().collect();
        let (out, _) = parse_all(&body, body.len());
        assert_eq!(out.len(), fr.len());
        for (a, b) in out.iter().zip(&fr) {
            assert_eq!(&a[..], &b[..]);
        }
    }

    #[test]
    fn arbitrary_chunk_boundaries() {
        let fr = frames();
        let bodies = [
            testgen::multipart(&fr, "b", true),
            testgen::multipart(&fr, "b", false),
            fr.iter().flatten().copied().collect::<Vec<u8>>(),
        ];
        for body in &bodies {
            for chunk in [1usize, 2, 3, 7, 13, 64, 100, 333, 1000, 4096, 65536] {
                let (out, stats) = parse_all(body, chunk);
                assert_eq!(out.len(), fr.len(), "chunk {chunk}: {stats:?}");
                for (a, b) in out.iter().zip(&fr) {
                    assert_eq!(&a[..], &b[..], "chunk {chunk}");
                }
            }
        }
    }

    #[test]
    fn garbage_between_frames_is_skipped_and_truncated_frames_resync() {
        let fr = frames();
        let mut body = Vec::new();
        body.extend_from_slice(b"HTTP junk\r\n\xff\xd8not a jpeg really\xff\xd9");
        body.extend_from_slice(&fr[0]);
        body.extend_from_slice(&[0u8; 300]);
        body.extend_from_slice(b"\xff\xd8\xff");
        // a truncated frame (first half) followed by a full one
        body.extend_from_slice(&fr[1][..fr[1].len() / 2]);
        body.extend_from_slice(&fr[2]);
        body.extend_from_slice(b"\xff\xff\xff\xd9 garbage \r\n--frame\r\n\r\n");
        body.extend_from_slice(&fr[3]);
        for chunk in [body.len(), 5, 97] {
            let (out, stats) = parse_all(&body, chunk);
            assert_eq!(out.len(), 3, "chunk {chunk}: {stats:?}");
            assert_eq!(&out[0][..], &fr[0][..]);
            assert_eq!(&out[1][..], &fr[2][..]);
            assert_eq!(&out[2][..], &fr[3][..]);
            assert!(stats.resyncs >= 1);
            assert!(stats.skipped_bytes > 300);
        }
    }

    #[test]
    fn lying_content_length_falls_back_to_scan() {
        let fr = frames();
        let mut body = Vec::new();
        for f in &fr {
            body.extend_from_slice(format!("--x\r\nContent-Length: {}\r\n\r\n", f.len() + 37).as_bytes());
            body.extend_from_slice(f);
            body.extend_from_slice(b"\r\n");
        }
        // Every frame but the last is pulled in when the next part's bytes arrive.
        let (out, stats) = parse_all(&body, 50);
        assert_eq!(out.len(), fr.len() - 1, "{stats:?}");
        assert_eq!(stats.fast_path, 0);
        // At end of input the pending frame is flushed structurally.
        let mut p = Parser::new();
        p.push(&body);
        let mut n = 0;
        while p.next_frame().is_some() {
            n += 1;
        }
        assert_eq!(n, fr.len() - 1);
        assert_eq!(p.finish().as_deref(), Some(&fr[fr.len() - 1][..]));
        assert!(p.finish().is_none());
    }

    #[test]
    fn oversized_frame_is_dropped() {
        let fr = frames();
        let mut p = Parser::new().with_max_frame(2048);
        // A fake endless frame: SOI then an APP segment claiming a huge length.
        let mut junk = vec![0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF];
        junk.extend(std::iter::repeat_n(0x11, 3000));
        p.push(&junk);
        assert!(p.next_frame().is_none());
        assert_eq!(p.stats().dropped, 1);
        p.push(&fr[0]);
        let got = p.next_frame().expect("recovers on next SOI");
        assert_eq!(&got[..], &fr[0][..]);
    }

    #[test]
    fn embedded_thumbnail_eoi_does_not_truncate() {
        // Build a JPEG with an APP1 segment that contains a complete tiny JPEG
        // (as EXIF thumbnails do), then a real frame body.
        let fr = frames();
        let thumb = &fr[0];
        let real = &fr[1];
        let mut jpeg = vec![0xFF, 0xD8];
        let seg_len = thumb.len() + 2;
        jpeg.extend_from_slice(&[0xFF, 0xE1, (seg_len >> 8) as u8, (seg_len & 0xFF) as u8]);
        jpeg.extend_from_slice(thumb);
        jpeg.extend_from_slice(&real[2..]); // everything after the real SOI
        let (out, _) = parse_all(&jpeg, 17);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), jpeg.len());
    }

    #[tokio::test]
    async fn frame_stream_from_chunked_stream_and_reader() {
        let fr = frames();
        let body = testgen::multipart(&fr, "s", false);
        let chunks: Vec<Result<Bytes, std::io::Error>> = body.chunks(777).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let s = FrameStream::from_stream(futures::stream::iter(chunks), Some("multipart/x-mixed-replace; boundary=s"));
        assert_eq!(s.parser().boundary(), Some("s"));
        let got: Vec<_> = s.collect().await;
        assert_eq!(got.len(), fr.len());
        for (g, f) in got.iter().zip(&fr) {
            assert_eq!(&g.as_ref().unwrap()[..], &f[..]);
        }

        let raw: Vec<u8> = fr.iter().flatten().copied().collect();
        let reader = std::io::Cursor::new(raw);
        let s = FrameStream::from_reader(reader, None);
        let got: Vec<_> = s.collect().await;
        assert_eq!(got.len(), fr.len());

        // an erroring stream surfaces the error then ends
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            vec![Ok(Bytes::copy_from_slice(&fr[0])), Err(std::io::Error::other("boom"))];
        let s = FrameStream::from_stream(futures::stream::iter(chunks), None);
        let got: Vec<_> = s.collect().await;
        assert_eq!(got.len(), 2);
        assert!(got[0].is_ok());
        assert!(matches!(got[1], Err(VisionError::Stream(_))));
    }
}
