//! `/audio`: PCM streaming to browsers.
//!
//! A dedicated thread polls the [`AudioSource`] every ~10 ms, packs interleaved
//! stereo `f32` into little-endian `i16` frames of ~20 ms and fans them out via a
//! `tokio::sync::broadcast` channel. Each WebSocket client subscribes; lagging
//! clients drop frames rather than growing a queue.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use tokio::sync::broadcast;

use crate::AppState;

/// Where the server gets rendered audio. Implemented by `sonder-audio`'s tap.
///
/// `pop_chunk` must be non-blocking: it appends whatever interleaved stereo
/// `f32` samples are available to `out` and returns how many *samples* (not
/// frames) it appended; `0` when the ring is empty.
pub trait AudioSource: Send + 'static {
    /// Append available interleaved stereo samples to `out`; return count appended.
    fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize;
}

impl<F> AudioSource for F
where
    F: FnMut(&mut Vec<f32>) -> usize + Send + 'static,
{
    fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize {
        self(out)
    }
}

const CHANNELS: usize = 2;
/// Chunk length as a fraction of a second (1/50 = 20 ms).
const CHUNKS_PER_SEC: u32 = 50;
/// Broadcast capacity in chunks (~1.3 s). Slow clients lag past this and drop.
const BROADCAST_CAPACITY: usize = 64;

/// Shared between the pump thread and WebSocket handlers.
#[derive(Clone)]
pub(crate) struct AudioHub {
    pub tx: broadcast::Sender<Bytes>,
    pub sample_rate: u32,
}

/// Handle to the pump thread; `stop` asks it to exit.
pub(crate) struct Pump {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Pump {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl AudioHub {
    pub fn start(source: Box<dyn AudioSource>, sample_rate: u32) -> (AudioHub, Pump) {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let hub = AudioHub { tx: tx.clone(), sample_rate };
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sonder-web-audio".into())
            .spawn(move || pump_loop(source, sample_rate, tx, stop2))
            .ok();
        (hub, Pump { stop, thread })
    }

    /// Samples (not frames) per broadcast chunk.
    pub fn chunk_samples(&self) -> usize {
        chunk_samples(self.sample_rate)
    }
}

fn chunk_samples(sample_rate: u32) -> usize {
    (sample_rate / CHUNKS_PER_SEC).max(1) as usize * CHANNELS
}

/// Convert interleaved f32 → i16 little-endian bytes with hard clipping.
pub(crate) fn pack_s16le(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 2);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn pump_loop(mut source: Box<dyn AudioSource>, sample_rate: u32, tx: broadcast::Sender<Bytes>, stop: Arc<AtomicBool>) {
    let chunk = chunk_samples(sample_rate);
    let mut pending: Vec<f32> = Vec::with_capacity(chunk * 4);
    let mut scratch: Vec<f32> = Vec::with_capacity(chunk * 4);
    let mut bytes: Vec<u8> = Vec::with_capacity(chunk * 2);

    while !stop.load(Ordering::Relaxed) {
        scratch.clear();
        let n = source.pop_chunk(&mut scratch);
        if n > 0 {
            pending.extend_from_slice(&scratch[..n.min(scratch.len())]);
        }
        // Emit as many full chunks as we have.
        let mut emitted = 0;
        while pending.len() - emitted >= chunk {
            bytes.clear();
            pack_s16le(&pending[emitted..emitted + chunk], &mut bytes);
            // Err only means "no receivers": keep draining the source regardless.
            let _ = tx.send(Bytes::copy_from_slice(&bytes));
            emitted += chunk;
        }
        if emitted > 0 {
            pending.drain(..emitted);
        }
        // If the source is far ahead (nobody drained for a while), keep only the tail.
        if pending.len() > chunk * 16 {
            let excess = pending.len() - chunk * 4;
            pending.drain(..excess);
        }
        if n == 0 {
            std::thread::sleep(Duration::from_millis(10));
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub(crate) async fn audio_ws(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(mut socket: WebSocket, state: Arc<AppState>) {
    let Some(hub) = state.audio.clone() else {
        let _ = socket.send(Message::text(r#"{"error":"no audio tap"}"#)).await;
        let _ = socket.send(Message::Close(None)).await;
        return;
    };

    let header = serde_json::json!({
        "sample_rate": hub.sample_rate,
        "channels": CHANNELS,
        "format": "s16le",
        "frame_samples": hub.chunk_samples(),
    });
    if socket.send(Message::text(header.to_string())).await.is_err() {
        return;
    }

    let mut rx = hub.tx.subscribe();
    // The hub keeps a broadcast sender alive for the server's lifetime, so the
    // engine ending is observed through the snapshot watch, not `RecvError::Closed`.
    let mut engine = state.snapshot.clone();
    loop {
        tokio::select! {
            changed = engine.changed() => {
                if changed.is_err() {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
            frame = rx.recv() => {
                match frame {
                    Ok(bytes) => {
                        if socket.send(Message::Binary(bytes)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(dropped = n, "audio client lagging; dropped frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_clipped_s16le() {
        let mut out = Vec::new();
        pack_s16le(&[0.0, 1.0, -1.0, 2.0, 0.5], &mut out);
        assert_eq!(out.len(), 10);
        assert_eq!(i16::from_le_bytes([out[2], out[3]]), 32767);
        assert_eq!(i16::from_le_bytes([out[4], out[5]]), -32767);
        assert_eq!(i16::from_le_bytes([out[6], out[7]]), 32767);
        assert_eq!(i16::from_le_bytes([out[8], out[9]]), 16384);
    }

    #[test]
    fn chunk_is_20ms_stereo() {
        assert_eq!(chunk_samples(48000), 960 * 2);
        assert_eq!(chunk_samples(44100), 882 * 2);
    }
}
