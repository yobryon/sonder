//! Synthetic frames for tests and demos: a dark background, bright soft-edged
//! discs, deterministic noise, and JPEG / multipart encoders so the whole
//! ingest path can be exercised without a camera.
//!
//! Available in tests, or with the `testgen` feature.

use std::io::Cursor;

use image::{GrayImage, ImageEncoder};

/// A bright disc in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Disc {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    /// Peak luma 0..255.
    pub brightness: u8,
}

/// Tiny deterministic xorshift generator (no `rand` dependency needed here).
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
}

/// Render a `w×h` luma frame: `bg` background, uniform noise of amplitude
/// `±noise`, and the given discs (soft 1-px edge; overlapping discs take the max).
pub fn render(w: u32, h: u32, bg: u8, noise: u8, discs: &[Disc], seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let n = (w * h) as usize;
    let mut out = vec![bg; n];
    if noise > 0 {
        let span = noise as u32 * 2 + 1;
        for px in out.iter_mut() {
            let d = (rng.next_u32() % span) as i32 - noise as i32;
            *px = (*px as i32 + d).clamp(0, 255) as u8;
        }
    }
    for d in discs {
        if d.r <= 0.0 {
            continue;
        }
        let x0 = (d.x - d.r - 1.0).floor().max(0.0) as u32;
        let x1 = (d.x + d.r + 1.0).ceil().min(w as f32) as u32;
        let y0 = (d.y - d.r - 1.0).floor().max(0.0) as u32;
        let y1 = (d.y + d.r + 1.0).ceil().min(h as f32) as u32;
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = x as f32 + 0.5 - d.x;
                let dy = y as f32 + 0.5 - d.y;
                let dist = (dx * dx + dy * dy).sqrt();
                let cover = (d.r + 0.5 - dist).clamp(0.0, 1.0);
                if cover > 0.0 {
                    let i = (y * w + x) as usize;
                    let v = (d.brightness as f32 * cover) as u8;
                    out[i] = out[i].max(v);
                }
            }
        }
    }
    out
}

/// Encode a luma frame as a baseline JPEG.
pub fn encode_jpeg(gray: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, image::ImageError> {
    let img = GrayImage::from_raw(w, h, gray.to_vec()).ok_or_else(|| {
        image::ImageError::Parameter(image::error::ParameterError::from_kind(
            image::error::ParameterErrorKind::DimensionMismatch,
        ))
    })?;
    let mut buf = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality).write_image(img.as_raw(), w, h, image::ExtendedColorType::L8)?;
    Ok(buf.into_inner())
}

/// Build a `multipart/x-mixed-replace` body from JPEG frames.
/// `with_length` adds `Content-Length` headers (the fast path).
pub fn multipart(frames: &[Vec<u8>], boundary: &str, with_length: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        out.extend_from_slice(format!("--{boundary}\r\nContent-Type: image/jpeg\r\n").as_bytes());
        if with_length {
            out.extend_from_slice(format!("Content-Length: {}\r\n", f.len()).as_bytes());
        }
        out.extend_from_slice(b"X-Timestamp: 12345.678\r\n\r\n");
        out.extend_from_slice(f);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// A short sequence of JPEG frames with one disc sliding down, for stream tests.
pub fn jpeg_sequence(n: usize, w: u32, h: u32) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let frame = render(
                w,
                h,
                20,
                6,
                &[Disc {
                    x: w as f32 / 2.0,
                    y: i as f32 * 4.0,
                    r: 4.0,
                    brightness: 230,
                }],
                i as u64 + 1,
            );
            encode_jpeg(&frame, w, h, 85).unwrap_or_default()
        })
        .collect()
}
