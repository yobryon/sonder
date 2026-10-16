# sonder-desktop

The Sonder desktop app: a [Tauri 2](https://tauri.app) window around the **native engine**
(native audio via cpal, MIDI out, local sources such as `system.metrics`, cameras, `stdin`)
showing the **same web UI** the CLI serves. On launch it

1. starts `sonder_engine::start` (audio backend `Auto`, web tap on),
2. serves the embedded UI with `sonder_web::serve_on` on `127.0.0.1:<free port>`,
3. navigates the window there — the UI runs in `ws` mode, so browser playback (the
   **Listen** button) works too, and "Open in browser" shows the same page in your browser.

Menu bar (and a tray icon): **Scene** — every built-in scene, *Open scene file…* (a `.toml`,
hot-loaded with `load_scene`); **Engine** — *Record data…*, *Native audio* (toggle: restarts
the engine on the silent backend or back; the web UI moves to a new port and the window
follows), *Open in browser*, *Quit*.

`Record data…` picks a `.sonder` path and restarts the engine recording every ingested
sample to it (`EngineOptions::record_data`, docs/V3.md §1); play it back with
`sonder replay <log>` or drop it on the web UI.

## Building

This crate is **excluded from the root workspace** (`exclude` in the root `Cargo.toml`):
Tauri needs system libraries (webkit2gtk, appindicator) that `cargo build --workspace`
must not require. It references the sibling crates by path and has its own `Cargo.lock`.

```
# Linux build deps (Debian/Ubuntu); macOS and Windows need nothing extra
sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libasound2-dev pkg-config

cd crates/sonder-desktop
cargo check                      # typecheck
cargo tauri dev                  # run (cargo install tauri-cli --version "^2", or npm i -g @tauri-apps/cli)
cargo tauri build                # installers under target/release/bundle/
```

Release builds for Linux (AppImage, deb), macOS (dmg) and Windows (msi, nsis) are made by
`.github/workflows/desktop.yml` on every `v*` tag (and on demand) and attached to the GitHub
release; `cargo check` + the engine-lifecycle test run there on every push.

`src/engine.rs` has no Tauri in it (engine + server lifecycle, `Desktop` state) so it can be
type-checked on a machine without webkit2gtk by including it in a scratch crate; `src/main.rs`
is the Tauri glue (menu, tray, dialogs, window) and is verified by CI.

Icons: `python3 scripts/make-icons.py` (Pillow) regenerates `icons/*` and the PWA icons.
