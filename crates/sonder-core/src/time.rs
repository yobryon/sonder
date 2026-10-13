//! Engine time.
//!
//! Everything in Sonder is stamped with [`Seconds`] since the engine epoch (the
//! moment `play`/`render` started). Real-time runs use a wall clock; offline
//! renders use a virtual clock that advances with the audio stream, which is what
//! makes renders deterministic.

use serde::{Deserialize, Serialize};

/// Seconds since the engine epoch, as f64.
pub type Seconds = f64;

/// A source of engine time. Implementations: wall clock, virtual (render) clock.
pub trait Clock: Send + Sync {
    /// Seconds since the engine epoch.
    fn now(&self) -> Seconds;
}

/// Wall-clock [`Clock`] anchored at construction.
#[derive(Debug, Clone)]
pub struct WallClock {
    epoch: std::time::Instant,
}

impl WallClock {
    pub fn new() -> Self {
        Self { epoch: std::time::Instant::now() }
    }
}

impl Default for WallClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for WallClock {
    fn now(&self) -> Seconds {
        self.epoch.elapsed().as_secs_f64()
    }
}

/// A manually advanced [`Clock`] for offline rendering and tests.
#[derive(Debug, Default)]
pub struct VirtualClock {
    now: std::sync::atomic::AtomicU64,
}

impl VirtualClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, t: Seconds) {
        self.now.store(t.to_bits(), std::sync::atomic::Ordering::Release);
    }
    pub fn advance(&self, dt: Seconds) {
        self.set(self.now() + dt);
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Seconds {
        f64::from_bits(self.now.load(std::sync::atomic::Ordering::Acquire))
    }
}

/// Position on the musical transport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct TransportPos {
    /// Beats per minute currently in effect.
    pub bpm: f64,
    /// Beats per bar.
    pub beats_per_bar: u8,
    /// Zero-based bar index since start.
    pub bar: u64,
    /// Zero-based beat within the bar.
    pub beat: u8,
    /// Fractional position within the beat, 0..1.
    pub phase: f64,
    /// Total elapsed beats as a float (bar*beats_per_bar + beat + phase).
    pub beats: f64,
}
