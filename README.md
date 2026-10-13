# Sonder

> *sonder* (n.) — the realization that each random passerby is living a life as vivid
> and complex as your own.

**Sonder is a realtime generative music engine that listens to the world.** Point it
at a live, public data stream — a traffic camera, the earthquake feed, Wikipedia's edit
firehose, the solar wind, your own CPU — and it turns whatever that stream is doing
into an evolving piece of music. Stack several streams; a conductor keeps them in one
key, one tempo, one mood.

It began as an image: a camera above a freeway at night, headlights sliding across the
frame, each one a person going somewhere, an ambient soundscape breathing with the
traffic. That image is the first preset. The product is an instrument whose player is
the world.

```
$ sonder play night-freeway            # simulated traffic, works offline
$ sonder play night-freeway --source camera.mjpeg --param url=http://…/video.mjpg
$ sonder play planet-tonight           # earthquakes + aircraft + wikipedia + solar wind + moon
$ sonder play this-machine             # your CPU, memory, network, processes
$ sonder probe https://api.example.com/metrics.json   # make any JSON endpoint a scene
$ sonder render night-freeway --seconds 120 --out freeway.wav
```

Written in Rust. Pure-Rust synth (no samples, no plugins), lock-free realtime audio,
a terminal UI, and an embedded web UI that streams the audio to your browser (so it
works on headless boxes and WSL too).

- **Product design:** [`docs/PRODUCT_DESIGN.md`](docs/PRODUCT_DESIGN.md)
- **Architecture & crate contracts:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- **Scene authoring (expression language):** [`crates/sonder-expr/README.md`](crates/sonder-expr/README.md)
- **Pointing it at a camera:** [`crates/sonder-vision/README.md`](crates/sonder-vision/README.md)

## How it works

```
sources ──Sample──▶ features ──signals/events──▶ composer ──SynthCommand──▶ synth ──▶ speakers / browser / wav
```

1. **Sources** emit *readings* (continuous channels like `mean_speed`) and *events*
   (a car crossed, an edit landed, a quake happened).
2. **Features** turn those into 0..1 *signals* with a tiny expression language —
   `norm(smooth(rate(event('car'), 10), 3))` — with adaptive normalization so any feed
   is "loud for itself", never absolute.
3. The **composer** is a conductor (key, mode, chords, tempo, mood) plus one
   *generator* per layer by role: pad, drone, bass, lead, arp, pluck, bells, perc,
   texture. Signals drive generator *parameters* (density, register, brightness…);
   events trigger notes. **Every pitch is quantized to the shared harmony**, which is
   why twelve independent feeds never turn into noise.
4. The **synth** is a polyphonic subtractive/FM engine with synthesized drums,
   tempo-synced delay, a plate reverb and a master limiter.

## Scenes

A scene is a TOML file. Curated ones live in [`scenes/`](scenes/):

| Scene | Data | Needs network |
|---|---|---|
| `night-freeway` | simulated traffic (or a real MJPEG camera) | no |
| `this-machine` | CPU, memory, network, processes | no |
| `midnight-walk` | seeded random walks — auditions every role | no |
| `planet-tonight` | USGS quakes · OpenSky aircraft · Wikipedia · NOAA solar wind · moon | yes |
| `wikipedia-rain` | Wikipedia recent changes | yes |
| `markets-open` | Coinbase BTC-USD trades | yes |
| `weather-station` | Open-Meteo · NOAA tides · sun position | yes |
| `hacker-news` | HN firehose | yes |
| `orbit` | ISS position | yes |

## Building

```
cargo build --release
./target/release/sonder --help
```

Native audio uses `cpal` (ALSA on Linux: `apt install libasound2-dev`). Without a sound
device, `sonder play` falls back to a silent realtime clock and you listen in the
browser (`--web`, default on) or pipe PCM: `sonder play night-freeway --pcm-stdout | ffplay -f s16le -ar 48000 -ch_layout stereo -`.

## License

MIT.
