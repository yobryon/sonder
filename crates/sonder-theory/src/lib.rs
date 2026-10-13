//! Pure music theory for Sonder: modes, scales, chords, quantization, voice
//! leading, progressions and rhythm.
//!
//! Nothing here knows about time or owns random state. Anything stochastic
//! takes `&mut impl Rng` so the caller controls determinism.
//!
//! Conventions:
//! - **pitch class** (`pc`): 0..=11, C = 0.
//! - **midi**: integer note number, C4 = 60.
//! - **degree**: 0-based index into a scale (`0` = tonic). Degrees may exceed
//!   the scale length or go negative; they wrap into neighbouring octaves.

#![forbid(unsafe_code)]

mod chord;
mod interval;
mod mode;
pub mod progression;
mod quantize;
mod rhythm;
mod scale;
mod voicing;

pub use chord::{diatonic_chord, Chord};
pub use interval::interval_name;
pub use mode::{mode_by_name, modes_by_brightness, Mode};
pub use quantize::Quantizer;
pub use rhythm::euclidean;
pub use scale::{pitch_class_name, Scale};
pub use voicing::{voice_lead, Voicing};
