//! MJPEG ingest and motion/blob tracking for camera sources.
//!
//! The founding image of Sonder is a camera above a freeway at night: headlights
//! sliding across the frame become notes. This crate turns any MJPEG camera
//! into [`sonder_core::Sample`]s:
//!
//! * [`mjpeg::FrameStream`] frames JPEGs out of an HTTP multipart body or a raw
//!   `ffmpeg -f mjpeg -` pipe;
//! * [`decode_gray`] / [`Decoder`] decode a JPEG to downscaled 8-bit luma;
//! * [`Tracker`] finds bright or moving blobs, tracks them across frames and
//!   reports entries, exits and line crossings;
//! * [`factory`] / [`register`] expose it all as the `camera.mjpeg` source.
//!
//! The crate is pure Rust (`#![forbid(unsafe_code)]`); the source never panics
//! on bad input and reports problems as `Datum::Status`.

#![forbid(unsafe_code)]

pub mod decode;
pub mod mjpeg;
pub mod source;
pub mod tracker;

#[cfg(any(test, feature = "testgen"))]
pub mod testgen;

pub use decode::{decode_gray, decode_gray_into, downscale_luma, Decoder};
pub use source::{factory, register, CameraParams, CameraSource, KIND};
pub use tracker::{Blob, FrameAnalysis, Mode, Tracker, TrackerConfig};

/// Errors from decoding and stream framing.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("jpeg: {0}")]
    Image(#[from] image::ImageError),
    #[error("decode: {0}")]
    Decode(String),
    #[error("unsupported JPEG color type {0}")]
    UnsupportedColor(String),
    #[error("downscale factor {0} out of range 1..=64")]
    BadDownscale(u32),
    #[error("stream: {0}")]
    Stream(String),
}
