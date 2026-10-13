//! Deterministic offline rendering.

use crate::SynthHost;

/// Default block size (frames) for offline rendering.
pub const DEFAULT_OFFLINE_BLOCK: usize = 256;

/// Render `seconds` of audio as fast as possible, calling `on_block(t)` with the
/// time in seconds *before* each block so the caller can tick its control loop
/// (and push commands through its controller) in lockstep. Returns interleaved
/// stereo. Uses [`DEFAULT_OFFLINE_BLOCK`] frames per block.
pub fn render_offline(host: &mut SynthHost, seconds: f64, on_block: impl FnMut(f64)) -> Vec<f32> {
    render_offline_blocks(host, seconds, DEFAULT_OFFLINE_BLOCK, on_block)
}

/// [`render_offline`] with an explicit block size (frames; 1..=8192).
pub fn render_offline_blocks(host: &mut SynthHost, seconds: f64, block_frames: usize, mut on_block: impl FnMut(f64)) -> Vec<f32> {
    let sr = host.sample_rate() as f64;
    let block = block_frames.clamp(1, 8192);
    let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
    let total_frames = (seconds * sr).round() as usize;
    let mut out = vec![0.0f32; total_frames * 2];
    let mut pos = 0usize;
    while pos < total_frames {
        let n = block.min(total_frames - pos);
        on_block(pos as f64 / sr);
        host.render(&mut out[pos * 2..(pos + n) * 2]);
        pos += n;
    }
    out
}
