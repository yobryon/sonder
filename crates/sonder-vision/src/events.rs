//! Turning tracker output into engine samples — shared by the native MJPEG source and
//! the browser (wasm) webcam path.

use sonder_core::{Sample, Seconds, SourceId};

use crate::tracker::Blob;

/// Blob area (fraction of the frame) → a 0..1 "size" that reads like a car vs a truck.
pub fn blob_size(area: f32) -> f64 {
    ((area as f64 * 100.0).max(0.0).sqrt()).clamp(0.0, 1.0)
}

/// An event sample (`blob`, `crossing`, …) for a tracked blob, with the attrs scenes map
/// from: `x`, `y`, `size`, `area`, `brightness`, `speed`, `dir`, `lane` (x quantized into
/// `lanes`), `duration`, `id`.
pub fn blob_sample(source: &SourceId, at: Seconds, name: &str, b: &Blob, lanes: u32) -> Sample {
    let lanes = lanes.max(1);
    let size = blob_size(b.area);
    let brightness = b.brightness.clamp(0.0, 1.0) as f64;
    let intensity = (size * brightness).clamp(0.0, 1.0);
    let lane = ((b.x.clamp(0.0, 0.999_99) * lanes as f32).floor() as i64).clamp(0, lanes as i64 - 1);
    Sample::event(source, at, name, intensity)
        .with_attr("x", b.x as f64)
        .with_attr("y", b.y as f64)
        .with_attr("size", size)
        .with_attr("area", b.area as f64)
        .with_attr("brightness", brightness)
        .with_attr("speed", b.speed as f64)
        .with_attr("dir", b.dir as f64)
        .with_attr("lane", lane)
        .with_attr("duration", b.age_s as f64)
        .with_attr("id", b.id as f64)
}
