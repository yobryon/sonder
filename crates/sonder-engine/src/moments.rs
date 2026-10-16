//! "Save this moment": a rolling in-memory data log of the last [`WINDOW_SECONDS`] of
//! ingested samples, written out as `<dir>/<timestamp>.sonder.gz` on request (`/moment`
//! in a room's chat, or any `Chat` command with that text). Audio is not kept.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use sonder_core::{Datum, Sample, Scene};

use crate::control::{ControlLoop, SampleSink};
use crate::datalog::{DataLogWriter, LogHeader};

/// How much history a moment holds, in engine seconds.
pub const WINDOW_SECONDS: f64 = 600.0;
/// Hard cap on buffered samples (a firehose scene at 600 s could otherwise grow unbounded).
const MAX_SAMPLES: usize = 200_000;

/// The rolling buffer; cheap to clone (shared).
#[derive(Clone)]
pub struct MomentBuffer {
    dir: PathBuf,
    buf: Arc<Mutex<VecDeque<Sample>>>,
}

impl MomentBuffer {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), buf: Arc::new(Mutex::new(VecDeque::new())) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A sink that keeps the last [`WINDOW_SECONDS`] of non-audio samples.
    pub fn sink(&self) -> SampleSink {
        let buf = self.buf.clone();
        Box::new(move |s: &Sample| {
            if matches!(s.datum, Datum::Audio { .. }) {
                return;
            }
            let mut b = buf.lock();
            b.push_back(s.clone());
            let horizon = s.at - WINDOW_SECONDS;
            while b.front().is_some_and(|f| f.at < horizon) || b.len() > MAX_SAMPLES {
                b.pop_front();
            }
        })
    }

    /// Samples currently held.
    pub fn len(&self) -> usize {
        self.buf.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write the buffer as a data log whose time starts at its first sample. Returns the
    /// path and the number of samples written.
    pub fn save(&self, scene: &Scene, seed: u64, engine_now: f64) -> std::io::Result<(PathBuf, usize)> {
        let samples: Vec<Sample> = self.buf.lock().iter().cloned().collect();
        let t0 = samples.first().map(|s| s.at).unwrap_or(engine_now);
        let epoch = crate::datalog::unix_now() - (engine_now - t0);
        let stamp = crate::datalog::rfc3339(epoch).replace(':', "-");
        let path = self.dir.join(format!("{stamp}.sonder.gz"));
        let header = LogHeader::new(&scene.name, seed, epoch, &ControlLoop::source_specs(scene));
        let mut w = DataLogWriter::create(&path, &header)?;
        for mut s in samples {
            s.at -= t0;
            w.write(&s);
        }
        let n = w.lines() as usize;
        w.finish()?;
        Ok((path, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::SourceId;

    #[test]
    fn buffer_keeps_a_window_and_saves_a_replayable_log() {
        let dir = std::env::temp_dir().join(format!("sonder-moments-{}", std::process::id()));
        let m = MomentBuffer::new(&dir);
        let mut sink = m.sink();
        let src = SourceId::new("traffic");
        for i in 0..700 {
            sink(&Sample::reading(&src, i as f64, [("count", i as f64)]));
        }
        sink(&Sample { source: src.clone(), at: 699.5, datum: Datum::Audio { rate: 48_000, samples: vec![0.0; 10] } });
        // 600 s window: samples at t < 99 are gone; audio never enters.
        assert_eq!(m.len(), 601);
        let scene = crate::load_scene("night-freeway").unwrap();
        let (path, n) = m.save(&scene, 9, 700.0).unwrap();
        assert_eq!(n, 601);
        assert!(path.extension().is_some_and(|e| e == "gz"));
        let log = crate::datalog::DataLog::open(&path).unwrap();
        assert_eq!(log.header().scene, "Night Freeway");
        assert_eq!(log.header().seed, 9);
        assert_eq!(log.entries().first().unwrap().t, 0.0);
        assert!((log.length_seconds() - 600.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
