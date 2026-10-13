//! Audio output backends (cpal, wav, pcm stdout, web tap) and the realtime synth host.
//!
//! ```text
//!  control thread                         audio thread
//!  ┌──────────────────┐  commands (rtrb)  ┌──────────────────┐
//!  │ SynthController  │ ────────────────▶ │ SynthHost        │
//!  │  .send(cmd)      │ ◀──────────────── │  .render(&mut[]) │
//!  │  .poll_feedback  │  feedback (rtrb)  │   owns Synth     │
//!  │  .take_recycled  │ ◀──────────────── │                  │
//!  │  .master_meter   │  recycled boxes   │                  │
//!  └──────────────────┘ ◀ ─ atomics ─ ─ ─ └──────────────────┘
//!           ▲                     tap (rtrb<f32>)   │
//!       AudioTap  ◀────────────────────────────────┘
//! ```
//!
//! [`synth_pair`] builds the two halves. A [`SynthHost`] is then driven by an
//! [`AudioStream`] (cpal / WAV / PCM-to-stdout / silent timer) or by
//! [`render_offline`] for deterministic renders.

#![forbid(unsafe_code)]

mod backend;
mod host;
#[cfg(feature = "native-audio")]
mod native;
mod offline;

pub use backend::{AudioError, AudioStream, Backend};
pub use host::{synth_pair, AudioTap, SynthController, SynthHost, TapConfig};
pub use offline::{render_offline, render_offline_blocks, DEFAULT_OFFLINE_BLOCK};

#[cfg(feature = "native-audio")]
pub use native::native_default_sample_rate;
