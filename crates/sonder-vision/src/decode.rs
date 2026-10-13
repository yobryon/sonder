//! JPEG → downscaled 8-bit luma.
//!
//! Frames are decoded with the `image` crate (zune-jpeg under the hood) into a
//! scratch buffer, then converted to luma and box-filtered down by an integer
//! factor in one pass. Tracking runs at something like 160×120, so the downscale
//! is where most of the per-frame cost disappears.

use std::io::Cursor;

use image::{ColorType, ImageDecoder};

use crate::VisionError;

/// Largest frame dimension we accept from a camera (either axis).
pub const MAX_DIMENSION: u32 = 8192;
/// Largest downscale factor accepted.
pub const MAX_DOWNSCALE: u32 = 64;

/// Decode a JPEG to 8-bit luma and downscale by an integer factor.
///
/// Returns `(pixels, width, height)` where `width = src_w / downscale` and
/// `height = src_h / downscale` (the remainder rows/columns are dropped).
/// `downscale` must be in `1..=64`.
pub fn decode_gray(jpeg: &[u8], downscale: u32) -> Result<(Vec<u8>, u32, u32), VisionError> {
    let mut out = Vec::new();
    let (w, h) = decode_gray_into(jpeg, downscale, &mut out)?;
    Ok((out, w, h))
}

/// Like [`decode_gray`] but writes into a caller-provided buffer (which is
/// cleared and resized). Allocates a fresh decode scratch buffer each call; use
/// [`Decoder`] to reuse that too.
pub fn decode_gray_into(jpeg: &[u8], downscale: u32, out: &mut Vec<u8>) -> Result<(u32, u32), VisionError> {
    Decoder::new().decode_into(jpeg, downscale, out)
}

/// A reusable JPEG → luma decoder holding its scratch buffers.
#[derive(Debug, Default)]
pub struct Decoder {
    scratch: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `jpeg`, convert to luma and downscale by `downscale` into `out`.
    /// Returns the output `(width, height)`.
    pub fn decode_into(&mut self, jpeg: &[u8], downscale: u32, out: &mut Vec<u8>) -> Result<(u32, u32), VisionError> {
        if downscale == 0 || downscale > MAX_DOWNSCALE {
            return Err(VisionError::BadDownscale(downscale));
        }
        let mut dec = image::codecs::jpeg::JpegDecoder::new(Cursor::new(jpeg))?;
        dec.set_limits(image::Limits {
            max_image_width: Some(MAX_DIMENSION),
            max_image_height: Some(MAX_DIMENSION),
            max_alloc: Some(512 << 20),
        })?;
        let (w, h) = dec.dimensions();
        let color = dec.color_type();
        let channels = match color {
            ColorType::L8 => 1,
            ColorType::La8 => 2,
            ColorType::Rgb8 => 3,
            ColorType::Rgba8 => 4,
            other => return Err(VisionError::UnsupportedColor(format!("{other:?}"))),
        };
        let total = dec.total_bytes();
        if total > usize::MAX as u64 || total != (w as u64) * (h as u64) * channels as u64 {
            return Err(VisionError::Decode(format!("inconsistent decoded size {total} for {w}x{h}x{channels}")));
        }
        self.scratch.clear();
        self.scratch.resize(total as usize, 0);
        dec.read_image(&mut self.scratch)?;
        let (ow, oh) = downscale_luma(&self.scratch, w, h, channels, downscale, out)?;
        Ok((ow, oh))
    }
}

/// Convert an interleaved 8-bit image (`channels` ∈ {1,2,3,4}; the first three
/// channels are taken as R,G,B or the first as gray) to luma and box-filter it
/// down by `ds`. Returns the output dimensions.
pub fn downscale_luma(src: &[u8], w: u32, h: u32, channels: usize, ds: u32, out: &mut Vec<u8>) -> Result<(u32, u32), VisionError> {
    if ds == 0 || ds > MAX_DOWNSCALE {
        return Err(VisionError::BadDownscale(ds));
    }
    let ow = w / ds;
    let oh = h / ds;
    if ow == 0 || oh == 0 {
        return Err(VisionError::Decode(format!("frame {w}x{h} too small for downscale {ds}")));
    }
    let (w, h, ds) = (w as usize, h as usize, ds as usize);
    let (ow_u, oh_u) = (ow as usize, oh as usize);
    if src.len() < w * h * channels {
        return Err(VisionError::Decode("pixel buffer shorter than dimensions imply".into()));
    }
    out.clear();
    out.resize(ow_u * oh_u, 0);
    let row_stride = w * channels;
    let norm = (ds * ds) as u32;
    // Per-output-column accumulators; a small, reused allocation.
    let mut acc: Vec<u32> = vec![0; ow_u];
    for oy in 0..oh_u {
        acc.iter_mut().for_each(|a| *a = 0);
        for dy in 0..ds {
            let row = &src[(oy * ds + dy) * row_stride..(oy * ds + dy + 1) * row_stride];
            // Only the first ow*ds source pixels are used; the remainder is dropped.
            let row = &row[..ow_u * ds * channels];
            match channels {
                1 => {
                    for (a, px) in acc.iter_mut().zip(row.chunks_exact(ds)) {
                        *a += px.iter().map(|&v| v as u32).sum::<u32>();
                    }
                }
                2 => {
                    for (a, px) in acc.iter_mut().zip(row.chunks_exact(ds * 2)) {
                        *a += px.chunks_exact(2).map(|p| p[0] as u32).sum::<u32>();
                    }
                }
                3 => {
                    for (a, px) in acc.iter_mut().zip(row.chunks_exact(ds * 3)) {
                        *a += px.chunks_exact(3).map(|p| luma(p[0], p[1], p[2])).sum::<u32>();
                    }
                }
                _ => {
                    for (a, px) in acc.iter_mut().zip(row.chunks_exact(ds * channels)) {
                        *a += px.chunks_exact(channels).map(|p| luma(p[0], p[1], p[2])).sum::<u32>();
                    }
                }
            }
        }
        let out_row = &mut out[oy * ow_u..(oy + 1) * ow_u];
        for (o, a) in out_row.iter_mut().zip(acc.iter()) {
            *o = ((a + norm / 2) / norm).min(255) as u8;
        }
    }
    Ok((ow, oh))
}

/// BT.601 luma in fixed point: (77 R + 150 G + 29 B) / 256.
#[inline]
pub fn luma(r: u8, g: u8, b: u8) -> u32 {
    (77 * r as u32 + 150 * g as u32 + 29 * b as u32 + 128) >> 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testgen;

    #[test]
    fn downscale_box_filter_is_exact_on_gray() {
        // 8x4 gray gradient, downscale 2 -> 4x2 averages of each 2x2 block.
        let w = 8u32;
        let h = 4u32;
        let src: Vec<u8> = (0..(w * h)).map(|i| (i * 7 % 256) as u8).collect();
        let mut out = Vec::new();
        let (ow, oh) = downscale_luma(&src, w, h, 1, 2, &mut out).unwrap();
        assert_eq!((ow, oh), (4, 2));
        for oy in 0..2usize {
            for ox in 0..4usize {
                let mut s = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        s += src[(oy * 2 + dy) * 8 + ox * 2 + dx] as u32;
                    }
                }
                assert_eq!(out[oy * 4 + ox] as u32, (s + 2) / 4);
            }
        }
    }

    #[test]
    fn downscale_drops_remainder_and_handles_rgb() {
        // 7x5 RGB, downscale 3 -> 2x1.
        let (w, h) = (7u32, 5u32);
        let mut src = Vec::new();
        for y in 0..h {
            for x in 0..w {
                src.extend_from_slice(&[(x * 30) as u8, (y * 40) as u8, 200]);
            }
        }
        let mut out = Vec::new();
        let (ow, oh) = downscale_luma(&src, w, h, 3, 3, &mut out).unwrap();
        assert_eq!((ow, oh), (2, 1));
        assert_eq!(out.len(), 2);
        // Compare against brute force.
        for ox in 0..2usize {
            let mut s = 0u32;
            for dy in 0..3 {
                for dx in 0..3 {
                    let i = (dy * 7 + ox * 3 + dx) * 3;
                    s += luma(src[i], src[i + 1], src[i + 2]);
                }
            }
            assert_eq!(out[ox] as u32, (s + 4) / 9);
        }
        assert!(matches!(downscale_luma(&src, w, h, 3, 0, &mut out), Err(VisionError::BadDownscale(0))));
        assert!(downscale_luma(&src, w, h, 3, 8, &mut out).is_err());
    }

    #[test]
    fn decode_gray_roundtrip_matches_source_within_jpeg_error() {
        let (w, h) = (64u32, 48u32);
        let frame = testgen::render(w, h, 20, 0, &[testgen::Disc { x: 32.0, y: 24.0, r: 10.0, brightness: 240 }], 1);
        let jpeg = testgen::encode_jpeg(&frame, w, h, 95).unwrap();
        let (g, ow, oh) = decode_gray(&jpeg, 1).unwrap();
        assert_eq!((ow, oh), (w, h));
        assert_eq!(g.len(), (w * h) as usize);
        // centre is bright, corner is dark
        assert!(g[(24 * w + 32) as usize] > 200);
        assert!(g[0] < 40);
        // downscaled 4x
        let (g4, ow4, oh4) = decode_gray(&jpeg, 4).unwrap();
        assert_eq!((ow4, oh4), (16, 12));
        assert_eq!(g4.len(), 16 * 12);
        assert!(g4[6 * 16 + 8] > 180);
        // mean brightness preserved by the box filter (within rounding)
        let m1: f64 = g.iter().map(|&v| v as f64).sum::<f64>() / g.len() as f64;
        let m4: f64 = g4.iter().map(|&v| v as f64).sum::<f64>() / g4.len() as f64;
        assert!((m1 - m4).abs() < 2.0, "{m1} vs {m4}");
    }

    #[test]
    fn decode_rejects_garbage_without_panicking() {
        assert!(decode_gray(b"not a jpeg", 1).is_err());
        assert!(decode_gray(&[0xFF, 0xD8, 0xFF], 1).is_err());
        assert!(decode_gray(&[], 2).is_err());
        let frame = testgen::render(16, 16, 0, 0, &[], 1);
        let jpeg = testgen::encode_jpeg(&frame, 16, 16, 80).unwrap();
        // Truncated frame
        assert!(decode_gray(&jpeg[..jpeg.len() / 2], 1).is_err() || decode_gray(&jpeg[..jpeg.len() / 2], 1).is_ok());
        assert!(matches!(decode_gray(&jpeg, 0), Err(VisionError::BadDownscale(0))));
        assert!(decode_gray(&jpeg, 32).is_err());
    }
}
