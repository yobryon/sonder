//! Polyphonic synth, drums, effects, mixer, analyzer. Pure DSP, realtime-safe.
//!
//! The audio thread owns a [`Synth`]: it feeds it [`sonder_core::SynthCommand`]s
//! via [`Synth::handle`] and pulls interleaved stereo via [`Synth::render`].
//! Both are allocation- and lock-free after construction. Anything that must be
//! freed (instrument / effects boxes) is handed back through
//! [`Synth::take_recycled`] / [`Synth::take_recycled_effects`].
//!
//! The control thread uses [`presets`] to build instruments and [`Analyzer`] to
//! compute a spectrum for the UI from tapped audio.

#![forbid(unsafe_code)]

pub mod analyzer;
pub mod delay;
pub mod drums;
pub mod env;
pub mod filter;
pub mod lfo;
pub mod master;
pub mod math;
pub mod osc;
pub mod presets;
pub mod reverb;
pub mod synth;
pub mod voice;

pub use analyzer::Analyzer;
pub use synth::{Synth, BLOCK, DRUM_VOICES, MAX_VOICES};
