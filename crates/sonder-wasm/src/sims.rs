//! Sources that run inside wasm: the synthetic sims, the astronomical clock, and the
//! webcam tracker. They mirror what the native async adapters emit.

use sonder_core::*;
use sonder_sources::adapters::{astro, freeway, walk};
use sonder_sources::SourceError;
use sonder_vision::{blob_sample, blob_viz, class_counts, DetectParams, FrameAnalysis, Mode, Tracker, TrackerConfig};

/// A Rust-stepped source.
pub enum Sim {
    Freeway { id: SourceId, sim: freeway::Freeway, lanes: usize, next_reading: f64, next_viz: f64 },
    Walk { id: SourceId, sim: walk::Walk },
    Astro { id: SourceId, lat: f64, lon: f64, utc_offset: f64, next: f64, was_day: Option<bool>, epoch_unix: f64 },
}

impl Sim {
    /// `Ok(None)` when the kind is not a builtin (JS must feed it).
    pub fn new(id: &SourceId, spec: &SourceSpec, seed: u64) -> Result<Option<Sim>, SourceError> {
        let seed = sonder_sources::util::instance_seed(seed, id.as_str(), sonder_sources::util::param_seed(spec)?);
        Ok(Some(match spec.kind.as_str() {
            "synthetic.freeway" => {
                let cfg = freeway::FreewayConfig::from_spec(spec)?;
                let lanes = cfg.lanes;
                Sim::Freeway { id: id.clone(), sim: freeway::Freeway::new(cfg, seed, 0.0), lanes, next_reading: 0.0, next_viz: 0.0 }
            }
            "synthetic.walk" | "synthetic.random-walk" => {
                let cfg = walk::WalkConfig::from_spec(spec)?;
                Sim::Walk { id: id.clone(), sim: walk::Walk::new(cfg, seed, 0.0) }
            }
            "clock.astro" => Sim::Astro {
                id: id.clone(),
                lat: spec.get_f64("lat").unwrap_or(51.5),
                lon: spec.get_f64("lon").unwrap_or(-0.12),
                utc_offset: spec.get_f64("utc_offset").unwrap_or(0.0),
                next: 0.0,
                was_day: None,
                epoch_unix: js_unix_now(),
            },
            _ => return Ok(None),
        }))
    }

    /// Step to engine time `t` and return the samples produced.
    pub fn advance_to(&mut self, t: Seconds) -> Vec<Sample> {
        let mut out = Vec::new();
        match self {
            Sim::Freeway { id, sim, lanes, next_reading, next_viz } => {
                for exit in sim.advance_to(t) {
                    let mut s = Sample::event(id, t, "car", exit.intensity());
                    if let Datum::Event { attrs, .. } = &mut s.datum {
                        *attrs = exit.attrs(*lanes).into_map();
                    }
                    out.push(s);
                }
                if t >= *next_reading {
                    *next_reading = t + 0.5;
                    out.push(Sample::reading(id, t, sim.readings()));
                }
                if t >= *next_viz {
                    *next_viz = t + 0.1;
                    out.push(Sample::viz(id, t, sim.viz()));
                }
            }
            Sim::Walk { id, sim } => {
                let o = sim.advance_to(t);
                for (at, vals) in o.readings {
                    out.push(Sample::reading(id, at, vals));
                }
                for (at, n) in o.events {
                    let intensity = sim.values.first().copied().unwrap_or(0.5);
                    out.push(Sample::event(id, at, "tick", intensity).with_attr("n", n as f64).with_attr("value", intensity));
                }
            }
            Sim::Astro { id, lat, lon, utc_offset, next, was_day, epoch_unix } => {
                if t >= *next {
                    *next = t + 1.0;
                    let r = astro::compute(*epoch_unix + t, *lat, *lon, *utc_offset);
                    let day = r.is_day;
                    if let Some(prev) = *was_day {
                        if day != prev {
                            out.push(Sample::event(id, t, if day { "sunrise" } else { "sunset" }, 1.0));
                        }
                    }
                    *was_day = Some(day);
                    out.push(Sample::reading(id, t, r.values()));
                }
            }
        }
        out
    }
}

/// Webcam → tracker → samples, mirroring the native `camera.mjpeg` source. With the
/// `detect` feature and `detect = true` in the spec, RGB frames pushed through
/// [`Camera::process_rgb`] go through the object detector instead of the threshold.
pub struct Camera {
    id: SourceId,
    tracker: Tracker,
    lanes: u32,
    line: Option<f32>,
    next_reading: f64,
    next_viz: f64,
    frames: u32,
    last_fps_at: f64,
    fps: f64,
    /// Detection params (`detect`, `model_url`, `model_input`, `classes`, `min_score`);
    /// unparsable values fall back to detection off.
    pub detect: DetectParams,
    detect_ms: f32,
}

impl Camera {
    pub fn new(id: &SourceId, spec: &SourceSpec) -> Self {
        let mode = match spec.get_str("mode") {
            Some("motion") => Mode::Motion,
            _ => Mode::Bright,
        };
        let mut cfg = TrackerConfig { mode, ..TrackerConfig::default() };
        if let Some(t) = spec.get_f64("threshold") {
            cfg.threshold = t as f32;
        } else if cfg.mode == Mode::Motion {
            cfg.threshold = 0.25;
        }
        if let Some(l) = spec.get_f64("line") {
            cfg.line = Some(l as f32);
        }
        if let Some(m) = spec.get_f64("min_blob") {
            cfg.min_blob_px = m.max(1.0) as u32;
        }
        cfg.downscale = 1; // JS already downscaled
        Self {
            id: id.clone(),
            line: cfg.line,
            tracker: Tracker::new(cfg),
            lanes: spec.get_f64("lanes").unwrap_or(4.0).clamp(1.0, 12.0) as u32,
            next_reading: 0.0,
            next_viz: 0.0,
            frames: 0,
            last_fps_at: 0.0,
            fps: 0.0,
            detect: DetectParams::from_spec(spec).unwrap_or_default(),
            detect_ms: 0.0,
        }
    }

    /// Threshold path (gray frame).
    pub fn process(&mut self, gray: &[u8], w: u32, h: u32, now: Seconds) -> Vec<Sample> {
        let a = self.tracker.process(gray, w, h, now);
        self.detect_ms = 0.0;
        self.finish(a, w, h, now)
    }

    /// Detector path (interleaved RGB frame): detections filtered by the spec's `classes`
    /// / `min_score`, then tracked by class.
    #[cfg(feature = "detect")]
    pub fn process_rgb(&mut self, det: &mut sonder_vision::Detector, rgb: &[u8], w: u32, h: u32, now: Seconds) -> Result<Vec<Sample>, String> {
        let dets: Vec<_> = det.detect_rgb(rgb, w, h).map_err(|e| e.to_string())?.into_iter().filter(|d| self.detect.accepts(d)).collect();
        self.detect_ms = det.last_ms();
        let mut a = self.tracker.process_detections(&dets, w, h, now);
        a.mean_brightness = {
            let (mut sum, mut n) = (0u64, 0u64);
            for px in rgb.chunks_exact(3).step_by(4) {
                sum += (77 * px[0] as u64 + 150 * px[1] as u64 + 29 * px[2] as u64) >> 8;
                n += 1;
            }
            if n == 0 { 0.0 } else { sum as f32 / n as f32 / 255.0 }
        };
        Ok(self.finish(a, w, h, now))
    }

    fn finish(&mut self, a: FrameAnalysis, w: u32, h: u32, now: Seconds) -> Vec<Sample> {
        let mut out = Vec::new();
        for b in &a.exited {
            out.push(blob_sample(&self.id, now, "blob", b, self.lanes));
        }
        for b in &a.crossings {
            out.push(blob_sample(&self.id, now, "crossing", b, self.lanes));
        }
        self.frames += 1;
        if now - self.last_fps_at >= 1.0 {
            self.fps = self.frames as f64 / (now - self.last_fps_at).max(1e-3);
            self.frames = 0;
            self.last_fps_at = now;
        }
        if now >= self.next_reading {
            self.next_reading = now + 0.25;
            let n = a.blobs.len().max(1) as f64;
            let mean_speed = a.blobs.iter().map(|b| b.speed as f64).sum::<f64>() / n;
            let mean_brightness = if a.blobs.iter().any(|b| b.class.is_some()) { a.mean_brightness as f64 } else { a.blobs.iter().map(|b| b.brightness as f64).sum::<f64>() / n };
            let mut values: Vec<(String, f64)> = vec![
                ("motion".into(), a.motion_energy as f64),
                ("blobs".into(), a.blobs.len() as f64),
                ("mean_speed".into(), mean_speed),
                ("mean_brightness".into(), mean_brightness),
                ("fps".into(), self.fps),
            ];
            if self.detect.enabled {
                values.extend(class_counts(&a.blobs, &self.detect.count_classes()));
                values.push(("detect_ms".into(), self.detect_ms as f64));
            }
            out.push(Sample::reading(&self.id, now, values));
        }
        if now >= self.next_viz {
            self.next_viz = now + 0.1;
            let blobs: Vec<serde_json::Value> = a.blobs.iter().map(blob_viz).collect();
            out.push(Sample::viz(
                &self.id,
                now,
                serde_json::json!({"blobs": blobs, "line": self.line, "w": w, "h": h, "motion": a.motion_energy}),
            ));
        }
        out
    }
}

/// Wall time from JS (`Date.now()`), so the astro clock is real even though engine time
/// starts at zero.
fn js_unix_now() -> f64 {
    js_sys::Date::now() / 1000.0
}
