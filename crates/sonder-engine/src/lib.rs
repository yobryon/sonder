//! The engine: wires sources → features → composer → synth → output, and publishes
//! [`EngineSnapshot`]s for UIs.
//!
//! Two modes share one control loop ([`control::ControlLoop`]):
//! - **live** ([`start`]): sources run on the caller's tokio runtime, the control loop
//!   runs on its own thread at 50 Hz against a wall clock, audio runs on the backend's
//!   thread.
//! - **offline** ([`render_to_wav`]): everything runs in lockstep on one thread inside a
//!   paused-time tokio runtime driving a [`VirtualClock`], so a given scene + seed renders
//!   to an identical WAV every time (for sources that don't touch the network).

#![forbid(unsafe_code)]

pub mod compose_prompt;
pub mod captions;
pub mod control;
pub mod datalog;
pub mod dj;
pub mod instruments;
pub mod mapping;
pub mod recipes;
pub mod replay;
#[cfg(feature = "native")]
pub mod room;
#[cfg(feature = "native")]
pub mod live;
#[cfg(feature = "native")]
pub mod moments;
#[cfg(feature = "link")]
pub mod link;
#[cfg(feature = "midi")]
pub mod midi;
#[cfg(feature = "native")]
pub mod narrator;
#[cfg(feature = "osc")]
pub mod osc;
#[cfg(feature = "native")]
pub mod render;
pub mod sink;
pub mod scenes;
pub mod validate;

#[cfg(feature = "native")]
pub use live::{start, AudioBackendChoice, EngineHandle, EngineOptions, WebAudio};
#[cfg(feature = "native")]
pub use render::{render_stem, render_stems_to_wav, render_to_wav, render_to_wav_with, RenderOptions, RenderStats, StemFile};
pub use scenes::{builtin_scenes, load_scene, parse_scene};
pub use validate::validate;

use sonder_core::EngineSnapshot;
use std::sync::Arc;

/// Control-loop tick period. 50 Hz.
pub const TICK_SECONDS: f64 = 0.02;

/// Snapshot publish cadence, in ticks (every 2nd tick = 25 Hz).
pub const SNAPSHOT_EVERY_TICKS: u32 = 2;

/// Convenience alias.
pub type SnapshotRx = tokio::sync::watch::Receiver<Arc<EngineSnapshot>>;
