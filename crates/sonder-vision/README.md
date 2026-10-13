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
