//! Output backends. Every backend owns the [`SynthHost`] on its own thread and
//! stops when the [`AudioStream`] is dropped.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::SynthHost;

/// Frames rendered per iteration by the timer-driven backends.
const PACED_BLOCK_FRAMES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// Native audio is compiled out or no output device could be opened.
    #[error("no audio output device: {0}")]
    NoDevice(String),
    /// The device exists but rejected every configuration we tried.
    #[error("unsupported audio configuration: {0}")]
    Unsupported(String),
    /// The backend failed while starting or running.
    #[error("audio backend error: {0}")]
    Backend(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("wav error: {0}")]
    Wav(String),
}

/// Where rendered audio goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// The system's default output device via cpal (feature `native-audio`).
    Native,
    /// Render `seconds` of audio to a 16-bit stereo WAV file as fast as possible.
    Wav { path: PathBuf, seconds: f64 },
    /// Write s16le interleaved stereo to stdout at real-time pace. Play with
    /// `sonder ... --pcm-stdout | ffplay -f s16le -ar 48000 -ch_layout stereo -`
    /// (or `| aplay -f S16_LE -r 48000 -c 2`).
    PcmStdout,
    /// No output: a thread renders at real-time pace so meters, feedback and the
    /// tap keep working (WSL / headless / browser-only playback).
    Null,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Wav { .. } => "wav",
            Backend::PcmStdout => "pcm-stdout",
            Backend::Null => "null",
        }
    }
}

/// A running output stream. Dropping it stops the backend and joins its thread.
pub struct AudioStream {
    sample_rate: u32,
    name: &'static str,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    description: String,
}

impl std::fmt::Debug for AudioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioStream")
            .field("backend", &self.name)
            .field("sample_rate", &self.sample_rate)
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl AudioStream {
    /// Start `backend`, consuming `host`. Returns an error (instead of panicking)
    /// whenever the backend cannot start, so the engine can fall back to [`Backend::Null`].
    pub fn start(backend: Backend, host: SynthHost) -> Result<AudioStream, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let host_rate = host.sample_rate().round() as u32;
        let name = backend.name();
        match backend {
            Backend::Null => {
                let thread = spawn_paced("sonder-audio-null", stop.clone(), finished.clone(), host, |_buf| Ok(()))?;
                Ok(Self { sample_rate: host_rate, name, stop, finished, thread: Some(thread), description: "silent timer".into() })
            }
            Backend::PcmStdout => {
                let stdout = std::io::stdout();
                let mut writer = std::io::BufWriter::with_capacity(PACED_BLOCK_FRAMES * 4 * 4, stdout);
                let mut bytes = Vec::with_capacity(PACED_BLOCK_FRAMES * 4);
                let thread = spawn_paced("sonder-audio-pcm", stop.clone(), finished.clone(), host, move |buf| {
                    bytes.clear();
                    for s in buf {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    writer.write_all(&bytes)?;
                    writer.flush()?;
                    Ok(())
                })?;
                Ok(Self { sample_rate: host_rate, name, stop, finished, thread: Some(thread), description: "s16le stereo on stdout".into() })
            }
            Backend::Wav { path, seconds } => {
                let spec = hound::WavSpec { channels: 2, sample_rate: host_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
                let mut writer = hound::WavWriter::create(&path, spec).map_err(|e| AudioError::Wav(e.to_string()))?;
                let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
                let total_frames = (seconds * host_rate as f64).round() as usize;
                let stop2 = stop.clone();
                let finished2 = finished.clone();
                let mut host = host;
                let thread = std::thread::Builder::new()
                    .name("sonder-audio-wav".into())
                    .spawn(move || {
                        let mut buf = vec![0.0f32; PACED_BLOCK_FRAMES * 2];
                        let mut done = 0usize;
                        let mut failed = false;
                        while done < total_frames && !stop2.load(Ordering::Relaxed) {
                            let n = PACED_BLOCK_FRAMES.min(total_frames - done);
                            host.render(&mut buf[..n * 2]);
                            for s in &buf[..n * 2] {
                                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                                if writer.write_sample(v).is_err() {
                                    failed = true;
                                    break;
                                }
                            }
                            if failed {
                                break;
                            }
                            done += n;
                        }
                        if let Err(e) = writer.finalize() {
                            tracing::error!("wav finalize failed: {e}");
                        }
                        finished2.store(true, Ordering::Release);
                    })
                    .map_err(AudioError::Io)?;
                Ok(Self {
                    sample_rate: host_rate,
                    name,
                    stop,
                    finished,
                    thread: Some(thread),
                    description: format!("{} ({seconds} s)", path.display()),
                })
            }
            Backend::Native => Self::start_native(host, stop, finished),
        }
    }

    #[cfg(feature = "native-audio")]
    fn start_native(host: SynthHost, stop: Arc<AtomicBool>, finished: Arc<AtomicBool>) -> Result<AudioStream, AudioError> {
        let started = crate::native::start(host, stop.clone())?;
        Ok(Self {
            sample_rate: started.sample_rate,
            name: "native",
            stop,
            finished,
            thread: Some(started.thread),
            description: started.description,
        })
    }

    #[cfg(not(feature = "native-audio"))]
    fn start_native(_host: SynthHost, _stop: Arc<AtomicBool>, _finished: Arc<AtomicBool>) -> Result<AudioStream, AudioError> {
        Err(AudioError::NoDevice("built without the `native-audio` feature".into()))
    }

    /// Actual output sample rate. For [`Backend::Native`] this may differ from the
    /// host's rate if the device refused it (a warning is logged in that case).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn backend_name(&self) -> &'static str {
        self.name
    }

    /// Human-readable description (device name, file path, …).
    pub fn description(&self) -> &str {
        &self.description
    }

    /// True once a finite backend (WAV) has written everything, or a streaming
    /// backend has stopped because of an error (e.g. stdout closed).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Block until the backend thread exits (WAV completes; others need `stop` first).
    pub fn wait(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Ask the backend to stop (returns immediately; `Drop` joins).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn a thread that renders `PACED_BLOCK_FRAMES` at real-time pace and hands
/// each block to `sink`. If `sink` fails the thread stops and marks `finished`.
fn spawn_paced(
    name: &str,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    mut host: SynthHost,
    mut sink: impl FnMut(&[f32]) -> std::io::Result<()> + Send + 'static,
) -> Result<JoinHandle<()>, AudioError> {
    let sr = host.sample_rate().max(1.0) as f64;
    let period = Duration::from_secs_f64(PACED_BLOCK_FRAMES as f64 / sr);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut buf = vec![0.0f32; PACED_BLOCK_FRAMES * 2];
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                host.render(&mut buf);
                if let Err(e) = sink(&buf) {
                    tracing::warn!("audio sink stopped: {e}");
                    break;
                }
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else if now - next > Duration::from_millis(200) {
                    // We fell far behind (suspend, debugger…): resync instead of bursting.
                    host.report_underrun();
                    next = now;
                }
            }
            finished.store(true, Ordering::Release);
        })
        .map_err(AudioError::Io)
}
