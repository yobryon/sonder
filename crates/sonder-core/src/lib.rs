//! `sonder-core` is the contract between every other Sonder crate.
//!
//! Data flows through the engine like this:
//!
//! ```text
//! sources ──Sample──▶ features ──signals/events──▶ composer ──SynthCommand──▶ dsp ──▶ out
//!                                                     │                        │
//!                                                     ▼                        ▼
//!                                              EngineSnapshot ◀──────── meters/spectrum
//! ```
//!
//! This crate holds only plain data types and tiny pure helpers so that it stays
//! dependency-light and every crate can depend on it without cycles.

pub mod command;
pub mod control;
pub mod instrument;
pub mod music;
pub mod sample;
pub mod scene;
pub mod snapshot;
pub mod time;

pub use command::*;
pub use control::*;
pub use instrument::*;
pub use music::*;
pub use sample::*;
pub use scene::*;
pub use snapshot::*;
pub use time::*;

/// Errors shared across crates. Crates define their own richer errors and
/// convert into this at boundaries.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid scene: {0}")]
    Scene(String),
    #[error("unknown source kind `{0}`")]
    UnknownSource(String),
    #[error("unknown instrument `{0}`")]
    UnknownInstrument(String),
    #[error("expression error in `{context}`: {message}")]
    Expr { context: String, message: String },
}
