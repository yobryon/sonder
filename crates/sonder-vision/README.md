# sonder-vision

Camera input for Sonder. Point it at any MJPEG stream (a public traffic cam, a
Raspberry Pi, your webcam through ffmpeg) and it turns headlights and movement
into `blob` / `crossing` events plus `motion`, `blobs`, `mean_speed`,
`mean_brightness` and `fps` readings. Pure Rust, no OpenCV, no unsafe.

Pipeline: MJPEG framing → JPEG decode → luma + integer downscale → threshold
(bright or background-difference) → connected components → blob merging →
nearest-neighbour tracking → events.

## Pointing Sonder at a camera

The source kind is `camera.mjpeg`. The only required parameter is `url`.

```sh
# A public MJPEG traffic cam (many DOT cameras and Axis/Hikvision boxes serve
# multipart/x-mixed-replace at paths like these):
sonder play night-freeway --source camera.mjpeg --param url=http://host/mjpg/video.mjpg
sonder play night-freeway --source camera.mjpeg --param url=http://host:8080/video
sonder play night-freeway --source camera.mjpeg --param url=http://host/axis-cgi/mjpg/video.cgi

# Anything ffmpeg can open (RTSP, HLS, a USB webcam, a video file) — pipe raw
# MJPEG on stdin with url="-":
ffmpeg -loglevel error -rtsp_transport tcp -i rtsp://user:pass@cam/stream1 \
       -vf fps=15,scale=640:-1 -q:v 5 -f mjpeg - \
  | sonder play night-freeway --source camera.mjpeg --param url=-

# Webcam on Linux / macOS
ffmpeg -loglevel error -f v4l2 -i /dev/video0 -vf fps=15 -f mjpeg - | sonder play night-freeway --source camera.mjpeg --param url=-
ffmpeg -loglevel error -f avfoundation -i "0" -vf fps=15 -f mjpeg - | sonder play night-freeway --source camera.mjpeg --param url=-
```

Or in a scene file:

```toml
[[layer]]
name = "traffic"
role = "pad"
source = { kind = "camera.mjpeg", url = "http://host/mjpg/video.mjpg", mode = "bright", line = 0.55, lanes = 4 }
signals.density = "norm(smooth(rate(event('blob'), 10), 3))"
map.energy = "density"

[[layer]]
name = "headlights"
role = "pluck"
source = "traffic"
on_event.blob.pitch = "attr('lane') / 4"
on_event.blob.velocity = "attr('brightness')"
```

Both framings are accepted automatically: `multipart/x-mixed-replace` (with or
without `Content-Length` part headers) and raw concatenated JPEGs (what
`ffmpeg -f mjpeg -` writes). Garbage, truncated frames and broken boundaries are
skipped by resyncing on the next JPEG start marker.

## Parameters

| param | default | meaning |
|---|---|---|
| `url` | required | `http(s)://…` MJPEG URL, or `-` for stdin |
| `mode` | `bright` | `bright` (headlights on a dark frame) or `motion` (frame difference against a running background) |
| `threshold` | `0.6` bright / `0.25` motion | 0..1. Bright: luma level, blended 50/50 with an adaptive `mean + 2.5·stddev`. Motion: minimum `|frame − background|` |
| `line` | `0.5` | normalized y (0 top, 1 bottom) of the crossing line; `false` disables `crossing` events |
| `min_blob` | `4` | minimum blob area in downscaled pixels |
| `downscale` | `4` | integer decode downscale (640×480 → 160×120). Bigger = faster, coarser |
| `fps_cap` | `15` | frames per second to analyse; extra frames are skipped |
| `reconnect` | `true` | exponential backoff reconnects (1 s → 60 s, jittered) |
| `lanes` | `4` | x is quantized into this many lanes for the `lane` attribute |
| `merge_dist` | `6` | blobs whose boxes are within this many pixels merge (a headlight pair becomes one car) |
| `max_blobs` | `64` | per-frame cap, largest first |
| `background_alpha` | `0.02` | motion mode: background learning rate per frame |
| `detect` | `false` | run an object detector instead of the threshold (see *Object detection*; needs the `detect` build feature) |
| `model_url` | ultralytics `yolo11n.onnx` | ONNX model URL (downloaded once into `~/.cache/sonder/models/`) or a local path |
| `model_input` | `320` | model input size in pixels (multiple of 32); 320 ≈ 4× faster than 640 |
| `classes` | all | keep only these classes, e.g. `["car","truck","bus","motorcycle","person","bicycle"]`; also the per-class count readings |
| `min_score` | `0.35` | minimum detection score |

`camera.jpeg` (a still polled every `interval` seconds) takes the same parameters minus
`line`, `fps_cap`, `reconnect`.

## What it emits

Events (intensity = `size × brightness`, clamped to 0..1):

- `blob` — a tracked object left the frame (or was lost for 3 frames).
- `crossing` — a tracked object crossed `line`; `dir` is `+1` downward, `-1` upward.

Both carry attrs `x, y` (0..1, exit/crossing position), `size` (0..1,
perceptual), `area` (frame fraction), `brightness` (0..1), `speed`
(frame-fractions per second; for `blob` it is the track's mean speed), `dir`,
`lane` (0..lanes-1), `duration` (seconds tracked), `id`.

Readings at 4 Hz: `motion` (0..1 active-pixel fraction, smoothed), `blobs`
(count), `mean_speed`, `mean_brightness`, `fps` (frames actually analysed).

`Viz` at 10 Hz: `{"blobs":[{"x","y","w","h","b","id"}], "line": y, "w": W, "h": H, "motion": m}`
in processed-frame pixels for `w`/`h`.

## Object detection (`detect = true`)

Build with the `detect` feature (`cargo build --release -p sonder -p sonder-vision
--features sonder-vision/detect` for the CLI, `--features detect` inside this crate) and the camera sources can run a small
YOLO-class ONNX model with [`tract-onnx`](https://crates.io/crates/tract-onnx) — pure
Rust, no system dependencies, the same code in the browser build. Frames are then
decoded to RGB, letterboxed to `model_input × model_input`, and the detections (COCO's
80 classes: `car`, `truck`, `bus`, `motorcycle`, `bicycle`, `person`, `bird`, `boat`, …)
feed the tracker instead of threshold blobs. Everything downstream is unchanged, plus:

- `blob` / `crossing` events carry `class` (text) and `class_id`; `brightness` is the
  detection score, `size`/`area` come from the box.
- readings add `objects` (tracked objects in frame), one count per class of the
  `classes` list (or `car, truck, bus, motorcycle, person, bicycle` when there is no
  list) and `detect_ms` (inference time of the last frame).
- viz blobs carry `c` (class name).

The default model is Ultralytics' YOLO11n COCO export (11 MB, fetched on first use from
`model_url` into `$XDG_CACHE_HOME/sonder/models` or `~/.cache/sonder/models`; pass a
local path to skip the download). Any YOLOv8/YOLO11 `yolo export format=onnx` model
works, static or dynamic shape; the output must be `[1, 4 + classes, anchors]`.
Postprocessing: best class per anchor ≥ `min_score`, class-aware NMS at IoU 0.5, then a
class-agnostic pass at 0.8 so a vehicle scored both `car` and `truck` is one object.
The tracker keeps a majority vote on each track's class, so one-frame flickers do not
relabel a vehicle.

Cost (release, one core of this dev box, YOLO11n): ~38 ms/frame at `model_input = 320`,
~175 ms at 640 — 20+ fps or 5–6 fps. A Raspberry Pi 5 is roughly 3–4× slower, so use
320 and `fps_cap = 4` there; `camera.jpeg` stills (one frame every 10–60 s) are free.
Night scenes: the detector finds headlit cars on well-lit freeways but misses dark ones;
`mode = "bright"` without `detect` remains the better night instrument, detection the
better day one.

```toml
source = { kind = "camera.jpeg", url = "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tvd33i80baybridgesastowerwest/tvd33i80baybridgesastowerwest.jpg",
           interval = 30, detect = true, classes = ["car", "truck", "bus", "motorcycle", "person"] }
```

```rust
use sonder_vision::{decode_rgb, Detector, DetectorConfig, Tracker, TrackerConfig};

let mut det = Detector::from_bytes(&std::fs::read("yolo11n.onnx")?, DetectorConfig::default())?;
let (rgb, w, h) = decode_rgb(&jpeg_bytes)?;
let dets = det.detect_rgb(&rgb, w, h)?;            // Vec<Detection { class_name, score, x, y, w, h }>
let mut tracker = Tracker::new(TrackerConfig::default());
let a = tracker.process_detections(&dets, w, h, t_seconds);
for b in &a.crossings {
    println!("{} crossed at x={:.2}", b.class.map(|c| c.name).unwrap_or("?"), b.x);
}
```

In the browser (`sonder-wasm` built with `--features detect`): JS fetches `model_url`,
calls `engine.load_model(bytes, 0)`, and pushes RGB frames with
`engine.push_frame_rgb(source, rgb, w, h)` instead of gray ones with `push_frame`.

## Bright vs motion mode

**`bright`** is for the founding image: a night camera where the only bright
things are headlights. It thresholds luma, so a static streetlamp is a permanent
blob (it is tracked once, then never exits — harmless, but raise `threshold` or
mask it with `line` placement if it drowns the readings). Headlight pairs are
merged by `merge_dist`.

**`motion`** works on any camera in any light: it keeps a running-average
background and reacts to what differs from it. Static bright regions disappear
into the background within ~`1/background_alpha` frames; anything that moves is
a blob. Use it for daytime traffic, harbours, pedestrians. Lower `threshold`
(0.15–0.3) for subtle motion; raise `background_alpha` if slow lighting changes
show up as motion; lower it if slow-moving objects fade into the background.

## Tuning

- **Threshold.** Watch the `blobs` reading. In bright mode, a floodlit scene
  may need `threshold = 0.8`; a dim one `0.4`. The adaptive component keeps the
  frame mean out of the mask, so moonlit roads mostly work at the default.
- **Line.** Put it where vehicles are well separated and fully in frame —
  usually a bit below the vanishing point, `0.55–0.7`. Crossing events are
  cleaner than exit events for counting; exit events carry a better mean speed.
- **Downscale / min_blob.** Tracking runs at `W/downscale`. Keep the processed
  frame around 160–320 px wide; a car should be at least ~2×`min_blob` pixels
  there. Too small a frame merges adjacent lanes; too large wastes CPU and makes
  noise speckle pass `min_blob`.
- **fps_cap.** Tracking needs maybe 10–15 fps for freeway speeds; capping lower
  saves CPU and is fine for slow scenes (harbours, clouds).
- **Headlight pairs splitting** into two cars → raise `merge_dist`. Two adjacent
  lanes merging into one blob → lower it.

Processing cost (release, 320×240): ~55 µs per frame in bright mode, ~150 µs in
motion mode, plus ~270 µs to decode and downscale a 320×240 JPEG. A 640×480
stream at 15 fps with `downscale = 4` uses a few percent of one core.

## Library use

```rust
use sonder_vision::{decode_gray, Tracker, TrackerConfig, Mode};

let (gray, w, h) = decode_gray(&jpeg_bytes, 4)?;
let mut tracker = Tracker::new(TrackerConfig { mode: Mode::Bright, line: Some(0.6), ..Default::default() });
let analysis = tracker.process(&gray, w, h, t_seconds);
for b in &analysis.exited {
    println!("car left at x={:.2} speed={:.2} lane-ish={:.0}", b.x, b.speed, (b.x * 4.0).floor());
}
```

`mjpeg::FrameStream` yields `Bytes` of complete JPEGs from a `reqwest` body or
any `AsyncRead`; `mjpeg::Parser` is the sans-IO core. `Decoder` and
`Tracker::process_into` reuse buffers for allocation-free steady state. The
`testgen` feature exposes the synthetic frame / JPEG / multipart generators used
by the tests.
