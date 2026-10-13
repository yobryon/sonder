//! cpal output. The stream is created and kept alive on a dedicated thread so
//! that [`crate::AudioStream`] stays `Send` regardless of platform quirks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize};

use crate::backend::AudioError;
use crate::SynthHost;

/// Preferred callback size in frames.
const PREFERRED_BUFFER_FRAMES: u32 = 256;
/// Scratch space: enough for very large callbacks without reallocating.
const SCRATCH_FRAMES: usize = 16_384;

pub(crate) struct Started {
    pub sample_rate: u32,
    pub thread: JoinHandle<()>,
    pub description: String,
}

/// The default output device's default sample rate, if a device exists. Use it
/// to build the synth at the rate the hardware will actually run.
pub fn native_default_sample_rate() -> Result<u32, AudioError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| AudioError::NoDevice("no default output device".into()))?;
    let cfg = device.default_output_config().map_err(|e| AudioError::NoDevice(e.to_string()))?;
    Ok(cfg.sample_rate())
}

struct Chosen {
    config: StreamConfig,
    format: SampleFormat,
}

/// Pick a configuration: prefer the host's sample rate with stereo f32, fall
/// back to the device default (reporting its rate).
fn choose_config(device: &cpal::Device, want_rate: u32) -> Result<Chosen, AudioError> {
    let default = device.default_output_config().map_err(|e| AudioError::Unsupported(e.to_string()))?;
    let mut best: Option<(i32, cpal::SupportedStreamConfigRange)> = None;
    if let Ok(ranges) = device.supported_output_configs() {
        for r in ranges {
            if r.channels() == 0 {
                continue;
            }
            let rate_ok = r.min_sample_rate() <= want_rate && want_rate <= r.max_sample_rate();
            let mut score = 0;
            if rate_ok {
                score += 100;
            }
            if r.channels() == 2 {
                score += 40;
            } else if r.channels() == 1 {
                score += 10;
            }
            match r.sample_format() {
                SampleFormat::F32 => score += 20,
                SampleFormat::I16 => score += 15,
                SampleFormat::U16 | SampleFormat::I32 => score += 10,
                _ => score -= 100,
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, r));
            }
        }
    }
    let (supported, rate) = match best {
        Some((score, r)) if score >= 100 => (r.try_with_sample_rate(want_rate), want_rate),
        Some((score, r)) if score > 0 => {
            // Rate unsupported: use the range's rate closest to what we want.
            let rate = want_rate.clamp(r.min_sample_rate(), r.max_sample_rate());
            (r.try_with_sample_rate(rate), rate)
        }
        _ => (None, default.sample_rate()),
    };
    let supported = supported.unwrap_or(default);
    let format = supported.sample_format();
    if !matches!(format, SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16 | SampleFormat::I32) {
        return Err(AudioError::Unsupported(format!("sample format {format:?}")));
    }
    let buffer_size = match supported.buffer_size() {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(PREFERRED_BUFFER_FRAMES.clamp(*min, (*max).max(*min))),
        SupportedBufferSize::Unknown => BufferSize::Default,
    };
    Ok(Chosen { config: StreamConfig { channels: supported.channels(), sample_rate: rate, buffer_size }, format })
}

fn build_stream<T>(device: &cpal::Device, config: &StreamConfig, mut host: SynthHost) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * 2];
    let shared = host.underrun_counter();
    let data_fn = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        let frames = data.len() / channels;
        if frames * 2 > scratch.len() {
            // Pathologically large callback: grows once, then stays.
            scratch.resize(frames * 2, 0.0);
        }
        let buf = &mut scratch[..frames * 2];
        host.render(buf);
        match channels {
            2 => {
                for (o, s) in data.iter_mut().zip(buf.iter()) {
                    *o = T::from_sample(*s);
                }
            }
            1 => {
                for (o, f) in data.iter_mut().zip(buf.chunks_exact(2)) {
                    *o = T::from_sample((f[0] + f[1]) * 0.5);
                }
            }
            n => {
                for (o, f) in data.chunks_exact_mut(n).zip(buf.chunks_exact(2)) {
                    o[0] = T::from_sample(f[0]);
                    o[1] = T::from_sample(f[1]);
                    for x in &mut o[2..] {
                        *x = T::from_sample(0.0);
                    }
                }
            }
        }
    };
    let err_fn = move |e: cpal::Error| {
        shared.add_underrun();
        tracing::warn!("cpal stream error: {e}");
    };
    device.build_output_stream(config.clone(), data_fn, err_fn, Some(Duration::from_secs(2)))
}

pub(crate) fn start(host: SynthHost, stop: Arc<AtomicBool>) -> Result<Started, AudioError> {
    let want_rate = host.sample_rate().round() as u32;
    let (tx, rx) = mpsc::channel::<Result<(u32, String), AudioError>>();
    let thread = std::thread::Builder::new()
        .name("sonder-audio-cpal".into())
        .spawn(move || {
            let result = (|| {
                let cpal_host = cpal::default_host();
                let device = cpal_host.default_output_device().ok_or_else(|| AudioError::NoDevice("no default output device".into()))?;
                let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "unknown device".into());
                let chosen = choose_config(&device, want_rate)?;
                if chosen.config.sample_rate != want_rate {
                    tracing::warn!(
                        "audio device runs at {} Hz but the synth was built for {} Hz; pitch will be off. Build the synth with native_default_sample_rate().",
                        chosen.config.sample_rate,
                        want_rate
                    );
                }
                let stream = match chosen.format {
                    SampleFormat::F32 => build_stream::<f32>(&device, &chosen.config, host),
                    SampleFormat::I16 => build_stream::<i16>(&device, &chosen.config, host),
                    SampleFormat::U16 => build_stream::<u16>(&device, &chosen.config, host),
                    SampleFormat::I32 => build_stream::<i32>(&device, &chosen.config, host),
                    other => return Err(AudioError::Unsupported(format!("sample format {other:?}"))),
                }
                .map_err(|e| AudioError::Backend(e.to_string()))?;
                stream.play().map_err(|e| AudioError::Backend(e.to_string()))?;
                let desc = format!("{name} @ {} Hz, {} ch, {:?}", chosen.config.sample_rate, chosen.config.channels, chosen.format);
                Ok((chosen.config.sample_rate, desc, stream))
            })();
            match result {
                Ok((rate, desc, stream)) => {
                    let _ = tx.send(Ok((rate, desc)));
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(stream);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        })
        .map_err(AudioError::Io)?;
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok((sample_rate, description))) => Ok(Started { sample_rate, thread, description }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Release);
            Err(AudioError::Backend("timed out opening the audio device".into()))
        }
    }
}
