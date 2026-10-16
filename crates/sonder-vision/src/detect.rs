//! Object detection for camera sources: a YOLOv8 / YOLO11-format ONNX model run with
//! `tract-onnx` (pure Rust, no system dependencies, wasm-capable).
//!
//! The pure parts — [`DetectorConfig`], [`DetectParams`], the [`letterbox`] maths,
//! [`decode_output`] (box decoding + class-aware NMS) and the COCO labels — are always
//! compiled; the model runner [`Detector`] needs the crate feature `detect`.
//!
//! Pipeline per frame: letterbox the RGB frame to `input × input` (gray 114 padding),
//! normalise to `0..1` NCHW `f32`, run the model, decode `[1, 4 + classes, N]` (centre x,
//! centre y, width, height, per-class scores), keep anchors whose best class scores at
//! least `min_score`, class-aware NMS at `iou`, and map the boxes back to normalized frame
//! coordinates (`0..1`, x right, y down — the same space as [`crate::tracker::Blob`]).
//!
//! The default model is Ultralytics' YOLO11n COCO export (same output layout as YOLOv8n;
//! `yolov8n.onnx` is not published on the assets release page). Any model exported with
//! `yolo export format=onnx` works, static or dynamic input shape; the input size is set
//! by `model_input` (multiple of 32).

use sonder_core::SourceSpec;

use crate::tracker::Class;

/// Where the default model is downloaded from (≈11 MB).
pub const DEFAULT_MODEL_URL: &str = "https://github.com/ultralytics/assets/releases/download/v8.3.0/yolo11n.onnx";
/// Default model input size (pixels, square). 320 runs ~4× faster than 640.
pub const DEFAULT_INPUT: u32 = 320;
/// Default minimum class score.
pub const DEFAULT_MIN_SCORE: f32 = 0.35;
/// Default NMS IoU threshold (class-aware).
pub const DEFAULT_IOU: f32 = 0.5;
/// Classes counted in readings when the source gives no `classes` filter.
pub const DEFAULT_COUNT_CLASSES: &[&str] = &["car", "truck", "bus", "motorcycle", "person", "bicycle"];
/// Letterbox padding value (Ultralytics convention).
pub const PAD_VALUE: f32 = 114.0 / 255.0;

/// The 80 COCO class names, in model output order.
pub const COCO_CLASSES: [&str; 80] = [
    "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck", "boat", "traffic light",
    "fire hydrant", "stop sign", "parking meter", "bench", "bird", "cat", "dog", "horse", "sheep", "cow",
    "elephant", "bear", "zebra", "giraffe", "backpack", "umbrella", "handbag", "tie", "suitcase", "frisbee",
    "skis", "snowboard", "sports ball", "kite", "baseball bat", "baseball glove", "skateboard", "surfboard",
    "tennis racket", "bottle", "wine glass", "cup", "fork", "knife", "spoon", "bowl", "banana", "apple",
    "sandwich", "orange", "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair", "couch",
    "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse", "remote", "keyboard",
    "cell phone", "microwave", "oven", "toaster", "sink", "refrigerator", "book", "clock", "vase", "scissors",
    "teddy bear", "hair drier", "toothbrush",
];

/// One detected object. Box centre `(x, y)` and size `(w, h)` are normalized to the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detection {
    pub class_id: u16,
    pub class_name: &'static str,
    /// Class score `0..1`.
    pub score: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Detection {
    pub fn class(&self) -> Class {
        Class { id: self.class_id, name: self.class_name }
    }
    /// Box corners `(x0, y0, x1, y1)`.
    pub fn corners(&self) -> (f32, f32, f32, f32) {
        (self.x - self.w / 2.0, self.y - self.h / 2.0, self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Intersection over union of two detections' boxes.
pub fn iou(a: &Detection, b: &Detection) -> f32 {
    let (ax0, ay0, ax1, ay1) = a.corners();
    let (bx0, by0, bx1, by1) = b.corners();
    let iw = (ax1.min(bx1) - ax0.max(bx0)).max(0.0);
    let ih = (ay1.min(by1) - ay0.max(by0)).max(0.0);
    let inter = iw * ih;
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Detector tuning.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectorConfig {
    /// Model input size (square, pixels). Rounded up to a multiple of 32.
    pub input: u32,
    /// Minimum class score.
    pub min_score: f32,
    /// Class-aware NMS IoU threshold.
    pub iou: f32,
    /// After class-aware NMS, boxes of *different* classes overlapping more than this are
    /// deduplicated too (a vehicle scored as both `car` and `truck` becomes one object).
    /// `>= 1` disables.
    pub dedupe_iou: f32,
    /// Keep at most this many detections per frame (highest scores first).
    pub max_detections: usize,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self { input: DEFAULT_INPUT, min_score: DEFAULT_MIN_SCORE, iou: DEFAULT_IOU, dedupe_iou: 0.8, max_detections: 100 }
    }
}

/// Source-level detection parameters (`detect`, `model_url`, `model_input`, `classes`,
/// `min_score`), parsed independently of the `detect` feature so scenes always validate.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectParams {
    pub enabled: bool,
    pub model_url: String,
    pub input: u32,
    /// Keep only these classes (names, case-insensitive) when set.
    pub classes: Option<Vec<String>>,
    pub min_score: f32,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self { enabled: false, model_url: DEFAULT_MODEL_URL.into(), input: DEFAULT_INPUT, classes: None, min_score: DEFAULT_MIN_SCORE }
    }
}

impl DetectParams {
    /// Parse from a source spec. `Err((param, message))` on a bad value.
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, (&'static str, String)> {
        use serde_json::Value as J;
        let enabled = match spec.params.get("detect") {
            None | Some(J::Null) => false,
            Some(J::Bool(b)) => *b,
            Some(J::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
            Some(J::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => true,
                "false" | "no" | "off" | "0" | "" => false,
                _ => return Err(("detect", "expected true/false".into())),
            },
            Some(_) => return Err(("detect", "expected true/false".into())),
        };
        let model_url = spec.get_str("model_url").map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_MODEL_URL).to_string();
        let input = match spec.params.get("model_input") {
            None | Some(J::Null) => DEFAULT_INPUT,
            Some(v) => {
                let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).ok_or(("model_input", "expected a number".to_string()))?;
                if !(64.0..=1280.0).contains(&n) {
                    return Err(("model_input", format!("{n} out of range 64..=1280")));
                }
                n as u32
            }
        };
        let classes = match spec.params.get("classes") {
            None | Some(J::Null) => None,
            Some(J::String(s)) => Some(s.split(',').map(|p| p.trim().to_ascii_lowercase()).filter(|p| !p.is_empty()).collect::<Vec<_>>()),
            Some(J::Array(a)) => {
                let mut v = Vec::with_capacity(a.len());
                for x in a {
                    match x.as_str() {
                        Some(s) if !s.trim().is_empty() => v.push(s.trim().to_ascii_lowercase()),
                        _ => return Err(("classes", "expected a list of class names".into())),
                    }
                }
                Some(v)
            }
            Some(_) => return Err(("classes", "expected a list of class names".into())),
        };
        let min_score = match spec.params.get("min_score") {
            None | Some(J::Null) => DEFAULT_MIN_SCORE,
            Some(v) => {
                let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).ok_or(("min_score", "expected a number".to_string()))?;
                if !(0.0..=1.0).contains(&n) {
                    return Err(("min_score", format!("{n} out of range 0..=1")));
                }
                n as f32
            }
        };
        Ok(Self { enabled, model_url, input, classes, min_score })
    }

    /// The detector configuration these parameters imply.
    pub fn detector_config(&self) -> DetectorConfig {
        DetectorConfig { input: self.input, min_score: self.min_score, ..Default::default() }
    }

    /// Whether a detection passes the class filter.
    pub fn accepts(&self, d: &Detection) -> bool {
        d.score >= self.min_score && self.classes.as_ref().is_none_or(|c| c.iter().any(|n| n.eq_ignore_ascii_case(d.class_name)))
    }

    /// Classes reported as per-class count readings: the filter list, or
    /// [`DEFAULT_COUNT_CLASSES`].
    pub fn count_classes(&self) -> Vec<String> {
        match &self.classes {
            Some(c) => c.clone(),
            None => DEFAULT_COUNT_CLASSES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// How a `w × h` frame is placed in an `input × input` model tensor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    pub frame_w: u32,
    pub frame_h: u32,
    pub input: u32,
    /// Frame pixels → input pixels.
    pub scale: f32,
    /// Resized frame size inside the input.
    pub new_w: u32,
    pub new_h: u32,
    /// Top-left offset of the resized frame inside the input.
    pub pad_x: u32,
    pub pad_y: u32,
}

/// Letterbox geometry: scale the frame to fit, centre it, pad the rest.
pub fn letterbox(frame_w: u32, frame_h: u32, input: u32) -> Letterbox {
    let input = input.max(1);
    let (fw, fh) = (frame_w.max(1) as f32, frame_h.max(1) as f32);
    let scale = (input as f32 / fw).min(input as f32 / fh);
    let new_w = ((fw * scale).round() as u32).clamp(1, input);
    let new_h = ((fh * scale).round() as u32).clamp(1, input);
    Letterbox { frame_w: frame_w.max(1), frame_h: frame_h.max(1), input, scale, new_w, new_h, pad_x: (input - new_w) / 2, pad_y: (input - new_h) / 2 }
}

impl Letterbox {
    /// Map a model-space box (centre + size in input pixels) to a normalized frame box
    /// (centre + size), clipped to the frame.
    pub fn to_frame(&self, cx: f32, cy: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
        let fw = self.frame_w as f32;
        let fh = self.frame_h as f32;
        let x0 = (((cx - w / 2.0) - self.pad_x as f32) / self.scale / fw).clamp(0.0, 1.0);
        let x1 = (((cx + w / 2.0) - self.pad_x as f32) / self.scale / fw).clamp(0.0, 1.0);
        let y0 = (((cy - h / 2.0) - self.pad_y as f32) / self.scale / fh).clamp(0.0, 1.0);
        let y1 = (((cy + h / 2.0) - self.pad_y as f32) / self.scale / fh).clamp(0.0, 1.0);
        ((x0 + x1) / 2.0, (y0 + y1) / 2.0, x1 - x0, y1 - y0)
    }
}

/// Fill `out` with the model input (`1 × 3 × input × input`, planar RGB `0..1`) from an
/// interleaved 8-bit frame with `channels` ∈ {1, 2, 3, 4} (1–2: gray replicated; 3–4:
/// RGB). Bilinear sampling; padding is [`PAD_VALUE`]. Returns `false` (and leaves `out`
/// untouched) when `src` is shorter than the dimensions imply.
pub fn preprocess(src: &[u8], w: u32, h: u32, channels: usize, lb: &Letterbox, out: &mut Vec<f32>) -> bool {
    let (wu, hu) = (w as usize, h as usize);
    if w == 0 || h == 0 || !(1..=4).contains(&channels) || src.len() < wu * hu * channels {
        return false;
    }
    let s = lb.input as usize;
    out.clear();
    out.resize(3 * s * s, PAD_VALUE);
    let inv = 1.0 / lb.scale;
    let (px, py) = (lb.pad_x as usize, lb.pad_y as usize);
    let (nw, nh) = ((lb.new_w as usize).min(s - px), (lb.new_h as usize).min(s - py));
    let inv255 = 1.0 / 255.0;
    for oy in 0..nh {
        let sy = ((oy as f32 + 0.5) * inv - 0.5).clamp(0.0, (hu - 1) as f32);
        let y0 = sy as usize;
        let y1 = (y0 + 1).min(hu - 1);
        let fy = sy - y0 as f32;
        for ox in 0..nw {
            let sx = ((ox as f32 + 0.5) * inv - 0.5).clamp(0.0, (wu - 1) as f32);
            let x0 = sx as usize;
            let x1 = (x0 + 1).min(wu - 1);
            let fx = sx - x0 as f32;
            let i00 = (y0 * wu + x0) * channels;
            let i01 = (y0 * wu + x1) * channels;
            let i10 = (y1 * wu + x0) * channels;
            let i11 = (y1 * wu + x1) * channels;
            let o = (py + oy) * s + px + ox;
            let nc = if channels >= 3 { 3 } else { 1 };
            for c in 0..3 {
                let cc = if nc == 1 { 0 } else { c };
                let v = (src[i00 + cc] as f32 * (1.0 - fx) + src[i01 + cc] as f32 * fx) * (1.0 - fy)
                    + (src[i10 + cc] as f32 * (1.0 - fx) + src[i11 + cc] as f32 * fx) * fy;
                out[c * s * s + o] = v * inv255;
            }
        }
    }
    true
}

/// Decode a YOLOv8/11 output tensor laid out `[4 + classes, anchors]` (row-major: row `r`
/// is `out[r * anchors ..]`): threshold on the best class, class-aware NMS, map to frame
/// coordinates. `labels` names the classes (index = class id; missing names become
/// `"class{id}"` — only the first 80 COCO names are static, so anything else must be
/// passed by the caller).
pub fn decode_output(out: &[f32], anchors: usize, n_classes: usize, lb: &Letterbox, cfg: &DetectorConfig, labels: &[&'static str]) -> Vec<Detection> {
    let rows = 4 + n_classes;
    if anchors == 0 || n_classes == 0 || out.len() < rows * anchors {
        return Vec::new();
    }
    let mut cands: Vec<Detection> = Vec::new();
    for i in 0..anchors {
        let mut best = 0usize;
        let mut best_s = f32::NEG_INFINITY;
        for c in 0..n_classes {
            let s = out[(4 + c) * anchors + i];
            if s > best_s {
                best_s = s;
                best = c;
            }
        }
        if best_s.is_nan() || best_s < cfg.min_score {
            continue;
        }
        let (cx, cy, w, h) = (out[i], out[anchors + i], out[2 * anchors + i], out[3 * anchors + i]);
        if !(cx.is_finite() && cy.is_finite() && w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
            continue;
        }
        let (x, y, bw, bh) = lb.to_frame(cx, cy, w, h);
        if bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        cands.push(Detection { class_id: best as u16, class_name: labels.get(best).copied().unwrap_or("object"), score: best_s.min(1.0), x, y, w: bw, h: bh });
    }
    nms(&mut cands, cfg)
}

/// Greedy NMS: class-aware at `cfg.iou`, then class-agnostic at `cfg.dedupe_iou`.
pub fn nms(cands: &mut [Detection], cfg: &DetectorConfig) -> Vec<Detection> {
    cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let mut keep: Vec<Detection> = Vec::new();
    for d in cands.iter() {
        if keep.len() >= cfg.max_detections {
            break;
        }
        let suppressed = keep.iter().any(|k| {
            let o = iou(k, d);
            (k.class_id == d.class_id && o > cfg.iou) || o > cfg.dedupe_iou
        });
        if !suppressed {
            keep.push(*d);
        }
    }
    keep
}

/// Labels for an `n`-class model: COCO for 80, `class{i}` otherwise (leaked once; models
/// are loaded a handful of times per process).
pub fn labels_for(n: usize) -> &'static [&'static str] {
    if n == COCO_CLASSES.len() {
        return &COCO_CLASSES;
    }
    let v: Vec<&'static str> = (0..n).map(|i| &*Box::leak(format!("class{i}").into_boxed_str())).collect();
    Box::leak(v.into_boxed_slice())
}

/// The on-disk cache for downloaded models: `$XDG_CACHE_HOME/sonder/models` or
/// `$HOME/.cache/sonder/models` (native only).
#[cfg(feature = "native")]
pub fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|s| !s.is_empty()).map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("sonder").join("models")
}

/// Where `url` is cached: `<cache_dir>/<last path segment>` (sanitized; a hash of the
/// URL when there is no usable segment). A plain path or `file://` URL maps to itself.
#[cfg(feature = "native")]
pub fn cache_path(url: &str) -> std::path::PathBuf {
    if let Some(p) = local_model_path(url) {
        return p;
    }
    let seg = url.rsplit('/').next().unwrap_or("").split(['?', '#']).next().unwrap_or("");
    let name: String = seg.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let name = if name.is_empty() || name == "." || name == ".." { format!("model-{:016x}.onnx", sonder_sources::util::hash_str(url)) } else { name };
    cache_dir().join(name)
}

#[cfg(feature = "native")]
fn local_model_path(url: &str) -> Option<std::path::PathBuf> {
    if let Some(p) = url.strip_prefix("file://") {
        return Some(std::path::PathBuf::from(p));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return None;
    }
    Some(std::path::PathBuf::from(url))
}

/// Model bytes for `url`: a local path / `file://` URL is read directly; an `http(s)` URL
/// is served from the cache or downloaded into it (written to a temp file, then renamed).
#[cfg(feature = "native")]
pub async fn fetch_model(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let path = cache_path(url);
    if let Ok(bytes) = tokio::fs::read(&path).await {
        if !bytes.is_empty() {
            return Ok(bytes);
        }
    }
    if local_model_path(url).is_some() {
        return Err(format!("model file {} not found", path.display()));
    }
    tracing::info!("downloading detection model {url} → {}", path.display());
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, sonder_sources::util::USER_AGENT)
        .timeout(std::time::Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("model download failed: {}", sonder_sources::util::short_err(&e)))?;
    if !resp.status().is_success() {
        return Err(format!("model download failed: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("model download failed: {}", sonder_sources::util::short_err(&e)))?;
    if bytes.len() < 1024 {
        return Err(format!("model download failed: {} bytes is not an ONNX model", bytes.len()));
    }
    if let Some(dir) = path.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    let tmp = path.with_extension(format!("part{}", std::process::id()));
    match tokio::fs::write(&tmp, &bytes).await.and_then(|_| std::fs::rename(&tmp, &path)) {
        Ok(()) => {}
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::warn!("could not cache model at {}: {e}", path.display());
        }
    }
    Ok(bytes.to_vec())
}

#[cfg(feature = "detect")]
mod model {
    use std::io::Cursor;

    use tract_onnx::prelude::*;

    use super::{decode_output, labels_for, letterbox, preprocess, Detection, DetectorConfig};
    use crate::VisionError;

    /// A loaded detection model with its reusable input buffer.
    pub struct Detector {
        plan: std::sync::Arc<TypedRunnableModel>,
        cfg: DetectorConfig,
        labels: &'static [&'static str],
        n_classes: usize,
        anchors: usize,
        input: Vec<f32>,
        last_ms: f32,
    }

    impl std::fmt::Debug for Detector {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Detector").field("cfg", &self.cfg).field("n_classes", &self.n_classes).field("anchors", &self.anchors).finish()
        }
    }

    impl Detector {
        /// Load an ONNX model from bytes (YOLOv8/11 export: input `[1,3,S,S]` RGB `0..1`,
        /// output `[1, 4 + classes, anchors]`). The input size comes from `cfg.input`
        /// (rounded up to a multiple of 32); dynamic-shape exports are fixed to it.
        pub fn from_bytes(bytes: &[u8], mut cfg: DetectorConfig) -> Result<Self, VisionError> {
            cfg.input = cfg.input.clamp(32, 4096).div_ceil(32) * 32;
            let s = cfg.input as usize;
            let err = |e: TractError| VisionError::Model(format!("{e:#}"));
            let model = tract_onnx::onnx()
                .with_ignore_value_info(true)
                .with_ignore_output_shapes(true)
                .model_for_read(&mut Cursor::new(bytes))
                .map_err(err)?;
            if model.inputs.len() != 1 || model.outputs.len() != 1 {
                return Err(VisionError::Model(format!("expected 1 input and 1 output, got {} and {}", model.inputs.len(), model.outputs.len())));
            }
            let plan = model
                .with_input_fact(0, f32::fact([1, 3, s, s]).into())
                .map_err(err)?
                .into_optimized()
                .map_err(err)?
                .into_runnable()
                .map_err(err)?;
            let out_shape: Vec<usize> = plan
                .model()
                .output_fact(0)
                .map_err(err)?
                .shape
                .as_concrete()
                .ok_or_else(|| VisionError::Model("output shape is not static".into()))?
                .to_vec();
            let (rows, anchors) = match out_shape.as_slice() {
                [1, rows, anchors] if *rows > 4 => (*rows, *anchors),
                other => return Err(VisionError::Model(format!("unsupported output shape {other:?}; expected [1, 4 + classes, anchors]"))),
            };
            let n_classes = rows - 4;
            Ok(Self { plan, cfg, labels: labels_for(n_classes), n_classes, anchors, input: Vec::new(), last_ms: 0.0 })
        }

        pub fn config(&self) -> &DetectorConfig {
            &self.cfg
        }

        /// Replace thresholds; the input size is fixed at load time.
        pub fn set_thresholds(&mut self, min_score: f32, iou: f32) {
            self.cfg.min_score = min_score;
            self.cfg.iou = iou;
        }

        /// Class names (index = class id).
        pub fn labels(&self) -> &'static [&'static str] {
            self.labels
        }

        pub fn n_classes(&self) -> usize {
            self.n_classes
        }

        /// Milliseconds the last `detect_*` call spent in preprocessing + inference +
        /// decoding (0 before the first call; wall clock, native only).
        pub fn last_ms(&self) -> f32 {
            self.last_ms
        }

        /// Detect objects in an interleaved 8-bit RGB frame.
        pub fn detect_rgb(&mut self, rgb: &[u8], w: u32, h: u32) -> Result<Vec<Detection>, VisionError> {
            self.detect(rgb, w, h, 3)
        }

        /// Detect objects in an 8-bit gray frame (the channel is replicated).
        pub fn detect_gray(&mut self, gray: &[u8], w: u32, h: u32) -> Result<Vec<Detection>, VisionError> {
            self.detect(gray, w, h, 1)
        }

        /// Detect objects in an interleaved 8-bit frame with `channels` ∈ {1, 2, 3, 4}.
        pub fn detect(&mut self, src: &[u8], w: u32, h: u32, channels: usize) -> Result<Vec<Detection>, VisionError> {
            #[cfg(not(target_arch = "wasm32"))]
            let t0 = std::time::Instant::now();
            let lb = letterbox(w, h, self.cfg.input);
            if !preprocess(src, w, h, channels, &lb, &mut self.input) {
                return Err(VisionError::Decode(format!("frame buffer too small for {w}x{h}x{channels}")));
            }
            let s = self.cfg.input as usize;
            let tensor = Tensor::from_shape(&[1, 3, s, s], &self.input).map_err(|e| VisionError::Model(format!("{e:#}")))?;
            let outputs = self.plan.run(tvec!(tensor.into())).map_err(|e| VisionError::Model(format!("{e:#}")))?;
            let out = outputs.first().ok_or_else(|| VisionError::Model("model produced no output".into()))?;
            let view = out.view();
            let data = view.as_slice::<f32>().map_err(|e| VisionError::Model(format!("{e:#}")))?;
            let dets = decode_output(data, self.anchors, self.n_classes, &lb, &self.cfg, self.labels);
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.last_ms = t0.elapsed().as_secs_f32() * 1000.0;
            }
            Ok(dets)
        }
    }
}

#[cfg(feature = "detect")]
pub use model::Detector;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DetectorConfig {
        DetectorConfig::default()
    }

    #[test]
    fn letterbox_fits_centres_and_maps_back() {
        // Landscape 640×480 into 320: scale 0.5, 320×240, padded 40 top/bottom.
        let lb = letterbox(640, 480, 320);
        assert_eq!((lb.scale, lb.new_w, lb.new_h, lb.pad_x, lb.pad_y), (0.5, 320, 240, 0, 40));
        // A box at the frame centre, 64×48 frame px → 32×24 model px at (160, 160).
        let (x, y, w, h) = lb.to_frame(160.0, 160.0, 32.0, 24.0);
        assert!((x - 0.5).abs() < 1e-5 && (y - 0.5).abs() < 1e-5, "{x} {y}");
        assert!((w - 0.1).abs() < 1e-5 && (h - 0.1).abs() < 1e-5, "{w} {h}");
        // Portrait 300×600 into 320: scale 320/600, padded left/right.
        let lb = letterbox(300, 600, 320);
        assert_eq!((lb.new_w, lb.new_h, lb.pad_y), (160, 320, 0));
        assert_eq!(lb.pad_x, 80);
        // Top-left corner of the frame is at the padding offset.
        let (x, y, _, _) = lb.to_frame(80.0, 0.0, 0.0, 0.0);
        assert!(x.abs() < 1e-5 && y.abs() < 1e-5);
        // Boxes are clipped to the frame.
        let (_, _, w, _) = lb.to_frame(80.0, 160.0, 400.0, 10.0);
        assert!((w - 1.0).abs() < 1e-5);
        // Degenerate sizes never panic.
        let lb = letterbox(0, 0, 0);
        assert_eq!(lb.input, 1);
        let lb = letterbox(5000, 1, 320);
        assert_eq!(lb.new_h, 1);
    }

    #[test]
    fn preprocess_letterboxes_rgb_and_replicates_gray() {
        let (w, h) = (8u32, 4u32);
        let mut rgb = Vec::new();
        for _ in 0..(w * h) {
            rgb.extend_from_slice(&[255, 0, 128]);
        }
        let lb = letterbox(w, h, 8);
        let mut out = Vec::new();
        assert!(preprocess(&rgb, w, h, 3, &lb, &mut out));
        assert_eq!(out.len(), 3 * 64);
        // Rows 2..6 hold the frame, rows 0..2 and 6..8 the padding.
        assert_eq!(out[0], PAD_VALUE);
        assert_eq!(out[2 * 8], 1.0);
        assert_eq!(out[64 + 2 * 8], 0.0);
        assert!((out[128 + 2 * 8] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(out[7 * 8], PAD_VALUE);
        let gray: Vec<u8> = vec![200; (w * h) as usize];
        assert!(preprocess(&gray, w, h, 1, &lb, &mut out));
        for c in 0..3 {
            assert!((out[c * 64 + 3 * 8 + 4] - 200.0 / 255.0).abs() < 1e-6);
        }
        assert!(!preprocess(&gray[..10], w, h, 1, &lb, &mut out));
        assert!(!preprocess(&gray, w, h, 5, &lb, &mut out));
    }

    /// Build a `[84, N]` tensor from (cx, cy, w, h, class, score) rows in model pixels.
    fn tensor(rows: &[(f32, f32, f32, f32, usize, f32)]) -> (Vec<f32>, usize) {
        let n = rows.len();
        let mut out = vec![0.0f32; 84 * n];
        for (i, &(cx, cy, w, h, c, s)) in rows.iter().enumerate() {
            out[i] = cx;
            out[n + i] = cy;
            out[2 * n + i] = w;
            out[3 * n + i] = h;
            out[(4 + c) * n + i] = s;
        }
        (out, n)
    }

    #[test]
    fn decode_thresholds_nms_and_maps_to_frame() {
        let lb = letterbox(640, 480, 320);
        // Two overlapping cars (keep the better), a truck fully overlapping the car
        // (deduped by the agnostic pass), a person elsewhere, a weak detection, a
        // partially overlapping car (kept: IoU < 0.5).
        let (out, n) = tensor(&[
            (160.0, 160.0, 40.0, 30.0, 2, 0.9),
            (162.0, 161.0, 40.0, 30.0, 2, 0.6),
            (160.0, 160.0, 42.0, 30.0, 7, 0.5),
            (40.0, 100.0, 10.0, 30.0, 0, 0.8),
            (250.0, 200.0, 20.0, 20.0, 5, 0.2),
            (190.0, 160.0, 40.0, 30.0, 2, 0.7),
        ]);
        let dets = decode_output(&out, n, 80, &lb, &cfg(), &COCO_CLASSES);
        let names: Vec<&str> = dets.iter().map(|d| d.class_name).collect();
        assert_eq!(names, vec!["car", "person", "car"], "{dets:?}");
        let car = &dets[0];
        assert!((car.score - 0.9).abs() < 1e-6);
        assert!((car.x - 0.5).abs() < 1e-5 && (car.y - 0.5).abs() < 1e-5);
        assert!((car.w - 40.0 / 320.0).abs() < 1e-5, "{}", car.w);
        assert!((car.h - 30.0 / 240.0).abs() < 1e-5, "{}", car.h);
        let person = &dets[1];
        assert!((person.x - 40.0 / 320.0).abs() < 1e-5 && (person.y - 60.0 / 240.0).abs() < 1e-5);
        assert_eq!(person.class_id, 0);
        // Short tensor / zero anchors: empty, no panic.
        assert!(decode_output(&out[..10], n, 80, &lb, &cfg(), &COCO_CLASSES).is_empty());
        assert!(decode_output(&out, 0, 80, &lb, &cfg(), &COCO_CLASSES).is_empty());
        // max_detections caps.
        let dets = decode_output(&out, n, 80, &lb, &DetectorConfig { max_detections: 1, ..cfg() }, &COCO_CLASSES);
        assert_eq!(dets.len(), 1);
    }

    #[test]
    fn iou_is_symmetric_and_bounded() {
        let a = Detection { class_id: 0, class_name: "a", score: 1.0, x: 0.5, y: 0.5, w: 0.2, h: 0.2 };
        let b = Detection { x: 0.6, ..a };
        let o = iou(&a, &b);
        assert!((o - iou(&b, &a)).abs() < 1e-6);
        // overlap 0.1×0.2 = 0.02; union 0.04+0.04-0.02 = 0.06
        assert!((o - 1.0 / 3.0).abs() < 1e-5, "{o}");
        assert_eq!(iou(&a, &Detection { x: 0.9, ..a }), 0.0);
        assert!((iou(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn params_parse_from_spec() {
        let p = DetectParams::from_spec(&SourceSpec::new("camera.jpeg")).unwrap();
        assert!(!p.enabled);
        assert_eq!(p.model_url, DEFAULT_MODEL_URL);
        assert_eq!(p.input, DEFAULT_INPUT);
        assert_eq!(p.count_classes(), DEFAULT_COUNT_CLASSES);
        let spec = SourceSpec::new("camera.jpeg")
            .with("detect", true)
            .with("model_url", "https://x/y.onnx")
            .with("model_input", 640)
            .with("classes", serde_json::json!(["Car", "truck"]))
            .with("min_score", 0.5);
        let p = DetectParams::from_spec(&spec).unwrap();
        assert!(p.enabled);
        assert_eq!(p.input, 640);
        assert_eq!(p.classes.as_deref(), Some(&["car".to_string(), "truck".to_string()][..]));
        let car = Detection { class_id: 2, class_name: "car", score: 0.6, x: 0.5, y: 0.5, w: 0.1, h: 0.1 };
        assert!(p.accepts(&car));
        assert!(!p.accepts(&Detection { score: 0.4, ..car }));
        assert!(!p.accepts(&Detection { class_name: "person", ..car }));
        let p = DetectParams::from_spec(&SourceSpec::new("x").with("detect", "yes").with("classes", "bird, boat")).unwrap();
        assert!(p.enabled);
        assert_eq!(p.classes.as_deref(), Some(&["bird".to_string(), "boat".to_string()][..]));
        assert_eq!(DetectParams::from_spec(&SourceSpec::new("x").with("detect", "maybe")).unwrap_err().0, "detect");
        assert_eq!(DetectParams::from_spec(&SourceSpec::new("x").with("model_input", 10)).unwrap_err().0, "model_input");
        assert_eq!(DetectParams::from_spec(&SourceSpec::new("x").with("min_score", 2)).unwrap_err().0, "min_score");
        assert_eq!(DetectParams::from_spec(&SourceSpec::new("x").with("classes", 3)).unwrap_err().0, "classes");
    }

    #[test]
    fn labels_cover_coco_and_custom_sizes() {
        assert_eq!(labels_for(80)[2], "car");
        let l = labels_for(3);
        assert_eq!(l, &["class0", "class1", "class2"]);
    }

    /// Real model, real traffic. `SONDER_MODEL=/path/to/model.onnx` (or a URL) overrides
    /// the default download; `SONDER_TEST_IMAGE=/path.jpg` overrides the Caltrans fetch.
    ///
    /// `SONDER_MODEL=… cargo test -p sonder-vision --release --features detect -- --ignored --nocapture real_model`
    #[cfg(all(feature = "native", feature = "detect"))]
    #[tokio::test]
    #[ignore]
    async fn real_model_detects_vehicles_on_a_caltrans_still() {
        let client = reqwest::Client::new();
        let url = std::env::var("SONDER_MODEL").unwrap_or_else(|_| DEFAULT_MODEL_URL.to_string());
        let bytes = fetch_model(&client, &url).await.expect("model");
        let cams = [
            "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv102i580westofsr24/tv102i580westofsr24.jpg",
            "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tvd33i80baybridgesastowerwest/tvd33i80baybridgesastowerwest.jpg",
            "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv317i2806thstreetofframp/tv317i2806thstreetofframp.jpg",
            "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tvc19us101justnorthofi680i280/tvc19us101justnorthofi680i280.jpg",
            "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv974i80atnorthtower/tv974i80atnorthtower.jpg",
        ];
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        if let Ok(p) = std::env::var("SONDER_TEST_IMAGE") {
            for p in p.split(',') {
                images.push((p.to_string(), std::fs::read(p).expect("test image")));
            }
        } else {
            for c in cams {
                match client.get(c).send().await {
                    Ok(r) if r.status().is_success() => images.push((c.to_string(), r.bytes().await.unwrap().to_vec())),
                    other => eprintln!("{c}: {other:?}"),
                }
            }
        }
        assert!(!images.is_empty());
        let mut total = 0usize;
        for input in [320u32, 640] {
            let mut det = Detector::from_bytes(&bytes, DetectorConfig { input, ..Default::default() }).expect("load");
            assert_eq!(det.n_classes(), 80);
            let mut ms = Vec::new();
            for (name, jpeg) in &images {
                let (rgb, w, h) = crate::decode::decode_rgb(jpeg).expect("jpeg");
                let dets = det.detect_rgb(&rgb, w, h).expect("detect");
                // Warm-up excluded from timing: run again.
                let dets2 = det.detect_rgb(&rgb, w, h).expect("detect");
                assert_eq!(dets.len(), dets2.len(), "inference must be deterministic");
                ms.push(det.last_ms());
                let gray: Vec<u8> = rgb.chunks_exact(3).map(|p| crate::decode::luma(p[0], p[1], p[2]) as u8).collect();
                let gray_dets = det.detect_gray(&gray, w, h).expect("gray");
                eprintln!("[{input}] {name}: {w}x{h} → {} objects ({} on gray), {:.1} ms", dets.len(), gray_dets.len(), det.last_ms());
                for d in &dets {
                    eprintln!("    {:<12} {:.2}  at ({:.2},{:.2}) {:.2}x{:.2}", d.class_name, d.score, d.x, d.y, d.w, d.h);
                    assert!((0.0..=1.0).contains(&d.x) && (0.0..=1.0).contains(&d.y) && d.w > 0.0 && d.h > 0.0);
                }
                total += dets.len();
            }
            let mean = ms.iter().sum::<f32>() / ms.len() as f32;
            eprintln!("[{input}] mean {mean:.1} ms/frame over {} frames (min {:.1}, max {:.1})", ms.len(), ms.iter().cloned().fold(f32::MAX, f32::min), ms.iter().cloned().fold(0.0, f32::max));
        }
        assert!(total > 0, "expected at least one object across the test images");
    }

    #[cfg(feature = "native")]
    #[test]
    fn cache_path_is_sane() {
        let p = cache_path("https://example.com/models/yolo11n.onnx?x=1");
        assert_eq!(p.file_name().unwrap(), "yolo11n.onnx");
        assert!(p.ends_with("sonder/models/yolo11n.onnx"));
        assert!(cache_path("https://example.com/").file_name().unwrap().to_str().unwrap().starts_with("model-"));
        assert_eq!(cache_path("/tmp/x.onnx"), std::path::PathBuf::from("/tmp/x.onnx"));
        assert_eq!(cache_path("file:///tmp/y.onnx"), std::path::PathBuf::from("/tmp/y.onnx"));
    }
}
