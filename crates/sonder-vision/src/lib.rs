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
//! * [`factory`] / [`register`] expose it all as the `camera.mjpeg` source;
//! * [`jpeg_poll`] does the same for still images polled from a URL (`camera.jpeg`);
//! * [`detect`] (feature `detect`) runs a YOLO-class ONNX model so blobs become `car`,
//!   `truck`, `person`, … and the tracker follows objects by class.
//!
//! The crate is pure Rust (`#![forbid(unsafe_code)]`); the source never panics
//! on bad input and reports problems as `Datum::Status`.

#![forbid(unsafe_code)]

#[cfg(feature = "native")]
pub mod decode;
#[cfg(feature = "native")]
pub mod jpeg_poll;
#[cfg(feature = "native")]
pub mod mjpeg;
#[cfg(feature = "native")]
pub mod source;
pub mod detect;
pub mod events;
pub mod tracker;

#[cfg(any(test, feature = "testgen"))]
pub mod testgen;

#[cfg(feature = "native")]
pub use decode::{decode_gray, decode_gray_into, decode_rgb, decode_rgb_into, downscale_luma, Decoder};
#[cfg(feature = "detect")]
pub use detect::Detector;
pub use detect::{DetectParams, Detection, DetectorConfig};
#[cfg(feature = "native")]
pub use source::{factory, register, CameraParams, CameraSource, KIND};
pub use events::{blob_sample, blob_size, blob_viz, class_counts};
pub use tracker::{Blob, Class, FrameAnalysis, Mode, Tracker, TrackerConfig};

/// Errors from decoding and stream framing.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("jpeg: {0}")]
#[cfg(feature = "native")]
    Image(#[from] image::ImageError),
    #[error("decode: {0}")]
    Decode(String),
    #[error("unsupported JPEG color type {0}")]
    UnsupportedColor(String),
    #[error("downscale factor {0} out of range 1..=64")]
    BadDownscale(u32),
    #[error("stream: {0}")]
    Stream(String),
    #[error("model: {0}")]
    Model(String),
}
