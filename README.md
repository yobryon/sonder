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
| `markets-live` | Kraken · Bybit · OKX trades (recipes) | yes |
| `social-firehose` | every Bluesky post · Wikipedia · HN | yes |
| `storm` | Blitzortung lightning · NOAA buoys · wind | yes |
| `machines` | the public MQTT broker's IoT firehose | yes |
| `seismic` | an IRIS seismometer (SeedLink) · USGS quakes | yes |
| `city-transit` | GTFS-Realtime transit · a Caltrans camera still | yes |
| `deep-ocean` | an Orcasound hydrophone via ffmpeg · tide | yes (stdin) |
| `this-browser` | mouse, clicks, scroll, keys (browser build) | no |

## Run it in your browser

The whole engine also compiles to WebAssembly and runs **with no server at all**: a Web
Worker hosts the engine, JS adapters fetch the live feeds (USGS, Wikipedia, Coinbase,
NOAA, Hacker News, GitHub, the ISS…) straight from your browser, and your webcam can stand
in for the traffic camera. Hosted build: **https://yobryon.github.io/sonder/** (deployed by
`.github/workflows/pages.yml` on every push to `main`).

Build and serve it locally:

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version "$(scripts/wasm-bindgen-version.sh)"
scripts/build-site.sh                 # → site/  (wasm + bindings + the static UI)
python3 -m http.server -d site        # open http://localhost:8000/
```

Query flags: `?scene=wikipedia-rain` picks a scene, `?seed=42` pins the seed,
`?mode=ws|worker` forces the transport (auto-detected: served by `sonder play --web` →
WebSocket, static host → Worker), `?mock=1` uses the JS mock engine (`engine-mock.js`)
for UI work without a wasm build. Sources that cannot exist in a browser
(`system.metrics`, `file-tail`, `stdin`) show as *failed* and their layers play on the
conductor alone; OpenSky may be blocked by CORS. JS unit tests: `node --test crates/sonder-web/js-tests/*.test.mjs`.

## Desktop app

The desktop app is the native engine (native audio, MIDI out, local sources such as your
CPU or a camera) with the web UI in a window, built with Tauri. Download the installer for
Linux (AppImage/deb), macOS (dmg) or Windows (msi/exe) from the
[Releases](https://github.com/yobryon/sonder/releases) page — they are built by
`.github/workflows/desktop.yml` on every `v*` tag. Menu: pick a built-in scene, open a scene
file, toggle native audio, open the UI in a browser.

To run it from source you only need Rust — the app is an ordinary cargo binary:

```
# Linux prerequisites (Windows and macOS need nothing beyond Rust; Windows uses the built-in WebView2):
sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libasound2-dev
cd crates/sonder-desktop
cargo run --release                                    # builds and opens the app
```

Installers (AppImage/deb, dmg, msi) additionally need the Tauri CLI, which is a separate
tool (`cargo tauri` is not part of cargo until you install it):

```
cargo install tauri-cli --version "^2" --locked        # ~10 min; or: npm i -g @tauri-apps/cli
cd crates/sonder-desktop && cargo tauri build          # → target/release/bundle/
```

The crate lives outside the workspace (see [`crates/sonder-desktop/README.md`](crates/sonder-desktop/README.md)).

## Install as an app

The hosted build is a PWA: open **https://yobryon.github.io/sonder/** and use the
**install** button in the header (Chrome/Edge: also the install icon in the address bar;
Safari: *Share → Add to Home Screen*). Installed, it opens in its own window, works offline
for the shell and the wasm engine (live feeds still need the network; offline scenes like
`night-freeway` and `midnight-walk` play anyway), shows the scene name and play/pause on
the lock screen / media keys (Media Session), and drops its UI refresh to 2 Hz while hidden
(the music keeps playing at full rate).

iOS Safari notes: audio needs a tap on **Listen** (AudioWorklet starts only after a user
gesture, and the ring/silent switch mutes Web Audio); in standalone (home-screen) mode a
page reload restarts the engine with a new seed, so pin one with `?seed=…`; the browser
freezes JavaScript when the app is backgrounded, so audio stops when you switch apps and
resumes when you come back. Background playback works on Android Chrome and desktop.

## v2: form, attribution, sharing, presence

- **Form**: `conductor.form` shapes pieces over minutes — tension arcs or A/B sections,
  motifs that recur and transform, melodic layers that *answer* salient events.
- **Legibility**: every event-triggered note knows its cause (hover a note in the piano
  roll: "0.13 BTC sell on OKX"); captions tell you what just happened; the **Live Earth**
  view plots every geo-tagged event on a world map with the day/night terminator.
- **Sound**: granular playback of live audio (`grain.ocean` — the hydrophone *is* the
  instrument), Karplus–Strong strings and modal bells, a glue compressor and LUFS rider.
- **Sharing**: a scene lives in a link (`#s=…`); an in-browser editor with live validation;
  `sonder compose "a calm piece from Tokyo weather and its trains"` writes a scene with
  Claude (also in the browser with your own key). `sonder play --watch` hot-reloads.
- **Presence**: `sonder radio <scene> --out icecast://…` streams a station
  ([docs/RADIO.md](docs/RADIO.md)); `sonder record` renders dated tracks and a daily
  workflow publishes them under `/records/`; the [Overpass kit](docs/OVERPASS.md) puts
  the founding image on a Raspberry Pi. Scope and status: [docs/V2.md](docs/V2.md).

## Sources

`sonder sources` lists every kind. Coded adapters: `synthetic.freeway`, `synthetic.walk`,
`system.metrics`, `clock.astro`, `camera.mjpeg`, `camera.jpeg`, `wikipedia.recent-changes`,
`usgs.earthquakes`, `opensky.flights`, `open-meteo.weather`, `noaa.space-weather`,
`noaa.tides`, `noaa.buoys`, `coinbase.trades`, `iss.position`, `hackernews.firehose`,
`github.events`, `blitzortung.lightning`, `mqtt`, `seedlink.seismic`, `transit.gtfs-rt`,
`audio.features`, and the generic `http-json` / `sse` / `websocket` / `file-tail` / `stdin`.

**Recipes** (`recipes/*.toml`) turn a generic transport plus an engine-side mapping into a
named kind with no code — `kraken.trades`, `bybit.trades`, `okx.trades`, `binance.trades`,
`bluesky.firehose`, `thingspeak.channel`, `uk.flood`, `uk.carbon` so far. A recipe is ~15
lines of TOML and runs identically natively and in the browser; see
[`recipes/README.md`](recipes/README.md) to add one.

## Building

```
cargo build --release
./target/release/sonder --help
```

Native audio uses `cpal` (ALSA on Linux: `apt install libasound2-dev`). Without a sound
device, `sonder play` falls back to a silent realtime clock and you listen in the
browser (`--web`, default on at http://127.0.0.1:7777/) or pipe PCM:
`sonder play night-freeway --pcm-stdout | ffplay -f s16le -ar 48000 -ch_layout stereo -`.

MIDI out: `sonder midi-ports`, then `sonder play <scene> --midi <port>` (or `--midi virtual`
to create a port a DAW can connect to). Melodic layers get one channel each; percussion
layers play GM drums on channel 10.

## Commands

| | |
|---|---|
| `sonder play [scene]` | live, with the terminal UI and web UI (`--open` opens a browser) |
| `sonder serve [scene]` | headless: web UI only |
| `sonder render <scene> --seconds N --out f.wav` | deterministic offline render |
| `sonder list` / `sonder show <scene>` | built-in scenes / print one to tweak |
| `sonder sources [kind]` / `sonder instruments` | what you can point it at / what it can sound like |
| `sonder probe <url> [-o scene.toml]` | poll a JSON endpoint and write a starter scene |
| `sonder validate <scene>` | check a scene without running it |

Keys (TUI and web): `1-9` mute · `Shift+1-9` solo · `space` pause · `[ ]` tempo · `- =` density ·
`k/K` transpose · `n/N` (TUI) or `m/M` (web) mode · `c` next chord · `w` open web UI · `Ctrl+S` save scene · `q` quit.

## License

MIT.
