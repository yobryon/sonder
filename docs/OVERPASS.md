# The Overpass kit — Sonder on a Raspberry Pi with a camera

The founding image, built for real: a camera above a road, a small computer, music
that breathes with the traffic. Everything here works on a Raspberry Pi 4/5 (64-bit
Raspberry Pi OS) and most small Linux boxes.

## 1. Build

```
sudo apt install -y build-essential pkg-config libasound2-dev ffmpeg
curl https://sh.rustup.rs -sSf | sh            # Rust toolchain
git clone https://github.com/yobryon/sonder && cd sonder
cargo build --release -p sonder                # ~10 min on a Pi 5
sudo mkdir -p /opt/sonder && sudo cp target/release/sonder /opt/sonder/
```

## 2. Camera

Two paths. Pick whichever your camera supports.

**Pi camera / USB webcam → MJPEG over stdin** (`camera.mjpeg`, url = "-"):

```
# Pi camera module (libcamera):
rpicam-vid -t 0 --codec mjpeg --width 640 --height 480 --framerate 15 -o - \
  | /opt/sonder/sonder serve night-freeway --source camera.mjpeg --param url=- --param mode=bright
# USB webcam:
ffmpeg -f v4l2 -framerate 15 -video_size 640x480 -i /dev/video0 -f mjpeg - \
  | /opt/sonder/sonder serve night-freeway --source camera.mjpeg --param url=-
```

**Network camera** (an MJPEG URL, or anything ffmpeg can open):

```
/opt/sonder/sonder serve night-freeway --source camera.mjpeg --param url=http://cam/mjpg/video.mjpg
ffmpeg -rtsp_transport tcp -i rtsp://user:pw@cam/stream -f mjpeg - | /opt/sonder/sonder serve night-freeway --source camera.mjpeg --param url=-
```

**Public DOT stills** (`camera.jpeg`, refreshed every 30–60 s — Caltrans has thousands,
see `sonder sources camera.jpeg`): no hardware at all.

Tuning: `mode=bright` for headlights at night (threshold 0.5–0.7), `mode=motion` by day
(threshold 0.2–0.3); `line=0.5` adds crossing events; `lanes=N` quantizes x into lanes for
pitch. Watch `blobs`/`motion` in the web UI's Sources panel and adjust until single cars
become single events.

**Day mode: `detect = true`.** Build with the detector (`cargo build --release -p sonder -p sonder-vision
--features sonder-vision/detect`; pure Rust, nothing to apt-install) and the camera runs a
small YOLO model (YOLO11n, 11 MB, downloaded once into `~/.cache/sonder/models/`) instead
of the threshold: every blob becomes a `car`, `truck`, `bus`, `motorcycle`, `bicycle`,
`person`, `bird`, … — events carry `class`, readings add `objects` and a count per class,
so a scene can give trucks the bass and cyclists the bells:

```
/opt/sonder/sonder serve night-freeway --source camera.mjpeg --param url=- \
  --param detect=true --param model_input=320 --param fps_cap=4 \
  --param classes='["car","truck","bus","motorcycle","person","bicycle"]'
```

```toml
on_event.crossing.pitch = "attr('lane') / 4"
on_event.crossing.velocity = "if(attr('class') == 'truck', 1.0, 0.6)"
```

A Pi 5 manages ~4 fps at `model_input = 320` (a desktop core does ~20 fps); `camera.jpeg`
stills are free at any size. At night the detector only sees headlit cars on bright
roads — keep `mode = bright` for the founding image and switch `detect` on by day (two
layers with two sources, or `sonder serve --watch` and a cron that edits the scene).

## 3. Scene

Copy a scene and point it at your camera so the service needs no flags:

```
/opt/sonder/sonder show night-freeway > /opt/sonder/scene.toml
# edit: source = { kind = "camera.mjpeg", url = "-", mode = "bright", lanes = 4 }
/opt/sonder/sonder validate /opt/sonder/scene.toml
```

For stdin cameras, wrap the pipe in a script and point `ExecStart` at it.

## 4. Run as a service, listen anywhere

```
sudo useradd -r -G audio,video sonder; sudo chown -R sonder /opt/sonder
sudo cp deploy/sonder.service /etc/systemd/system/
sudo systemctl enable --now sonder
```

- Speakers on the Pi: native audio via ALSA (`aplay -l` to check the device).
- Any phone/laptop on the network: `http://<pi>:7777/` → ▶ Listen (audio streams to the
  browser; no sound card needed on the Pi).
- A screen on the wall: Chromium in kiosk mode on `http://localhost:7777/?scene=...`:
  `chromium --kiosk --autoplay-policy=no-user-gesture-required http://localhost:7777/`.
- The internet: `sonder radio` (see docs/RADIO.md) streams it as a station.

## 5. Add the world

Stack more layers in the scene — earthquakes, lightning, the ISS, your own MQTT sensors
(`mqtt` kind) — the camera stays the lead voice; the conductor keeps them in one key.
