//! Embedded web UI for Sonder: engine snapshots and rendered audio over
//! WebSocket, plus a zero-build vanilla-JS frontend with canvas visualizers.
//!
//! ```text
//! GET  /              embedded index.html
//! GET  /app.js        embedded ES module (UI)
//! GET  /app.css       embedded stylesheet
//! GET  /worklet.js    embedded AudioWorklet processor
//! GET  /transport.js  embedded transport module (app.js imports it; picks ws here)
//! GET  /*.js, /coastlines.json  the other UI modules app.js imports (see `assets::FILES`)
//! GET  /api/snapshot  latest `EngineSnapshot` as JSON
//! POST /api/validate  `{"toml": "..."}` → `{"errors": []}` (scene validation for the editor)
//! WS   /ws            server → snapshot JSON on every change (coalesced);
//!                     client → `UserCommand` JSON (`{"cmd":"toggle_mute","layer":0}`)
//! WS   /audio         server → one text header `{"sample_rate","channels","format":"s16le"}`
//!                     then binary frames of interleaved i16 stereo PCM (~20 ms each)
//! ```
//!
//! The server shuts down gracefully when the snapshot `watch` sender is
//! dropped (i.e. the engine ended).

#![forbid(unsafe_code)]

mod assets;
mod audio;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sonder_core::{EngineSnapshot, UserCommand};
use tokio::sync::{mpsc, watch};

pub use audio::AudioSource;

/// Everything the web server needs from the running engine.
pub struct WebHandle {
    /// Latest engine snapshot; the server pushes a JSON copy to `/ws` clients on every change.
    pub snapshot: watch::Receiver<Arc<EngineSnapshot>>,
    /// Commands from UI clients are forwarded here.
    pub commands: mpsc::Sender<UserCommand>,
    /// Rendered audio for `/audio`. `None` disables browser playback.
    pub audio: Option<Box<dyn AudioSource>>,
    /// Sample rate of the audio produced by [`WebHandle::audio`].
    pub sample_rate: u32,
}

/// Shared server state.
pub(crate) struct AppState {
    pub snapshot: watch::Receiver<Arc<EngineSnapshot>>,
    pub commands: mpsc::Sender<UserCommand>,
    pub audio: Option<audio::AudioHub>,
}

/// Build the axum router (exposed for tests and embedding).
fn router(state: Arc<AppState>) -> Router {
    assets::routes(Router::new().route("/", get(assets::index)))
        .route("/api/snapshot", get(api_snapshot))
        .route("/api/validate", post(api_validate))
        .route("/ws", get(ws::snapshot_ws))
        .route("/audio", get(audio::audio_ws))
        .with_state(state)
}

async fn api_snapshot(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let snap = state.snapshot.borrow().clone();
    Json(snap)
}

#[derive(Deserialize)]
struct ValidateRequest {
    toml: String,
}

#[derive(Serialize)]
struct ValidateResponse {
    errors: Vec<String>,
}

/// Validate a scene TOML for the in-browser editor; the same checks the wasm
/// `validate_scene` runs. Always 200: errors are data, not failures.
async fn api_validate(Json(req): Json<ValidateRequest>) -> Json<ValidateResponse> {
    Json(ValidateResponse { errors: validate_scene_toml(&req.toml) })
}

/// Parse + validate a scene TOML; empty when valid.
pub fn validate_scene_toml(toml_src: &str) -> Vec<String> {
    match sonder_engine::parse_scene(toml_src) {
        Err(e) => vec![format!("{e:#}")],
        Ok(scene) => sonder_engine::validate(&scene).err().unwrap_or_default(),
    }
}

/// Serve the UI on `addr` until the engine ends (snapshot sender dropped).
///
/// Binds immediately; the bound address is logged at `info`. Returns when the
/// snapshot watch channel closes and all in-flight connections have finished.
pub async fn serve(addr: SocketAddr, handle: WebHandle) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(listener, handle).await
}

/// Like [`serve`] but on an already-bound listener (lets callers pick port 0
/// and learn the real address first).
pub async fn serve_on(listener: tokio::net::TcpListener, handle: WebHandle) -> anyhow::Result<()> {
    let local = listener.local_addr()?;
    tracing::info!(%local, "sonder web ui listening");

    let (audio, pump) = match handle.audio {
        Some(src) => {
            let (hub, pump) = audio::AudioHub::start(src, handle.sample_rate);
            (Some(hub), Some(pump))
        }
        None => (None, None),
    };

    let mut shutdown_rx = handle.snapshot.clone();
    let state = Arc::new(AppState { snapshot: handle.snapshot, commands: handle.commands, audio });
    let app = router(state);

    let shutdown = async move {
        // `changed()` errors only when the sender is dropped: the engine is gone.
        while shutdown_rx.changed().await.is_ok() {}
        tracing::info!("engine ended; shutting down web ui");
    };

    let result = axum::serve(listener, app).with_graceful_shutdown(shutdown).await;
    if let Some(pump) = pump {
        pump.stop();
    }
    result.map_err(Into::into)
}

/// Best-effort "open this URL in the user's browser". Never fails; failures are
/// logged at `debug`. Tries `xdg-open` (and `wslview` / `cmd.exe /c start` on
/// WSL) on Linux, `open` on macOS, `cmd /c start` on Windows.
pub fn open_browser(url: &str) {
    use std::process::{Command, Stdio};

    fn try_run(cmd: &str, args: &[&str]) -> bool {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    }

    let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version").map(|v| v.to_ascii_lowercase().contains("microsoft")).unwrap_or(false);

    let ok = if cfg!(target_os = "macos") {
        try_run("open", &[url])
    } else if cfg!(target_os = "windows") {
        try_run("cmd", &["/c", "start", "", url])
    } else if is_wsl {
        try_run("wslview", &[url]) || try_run("cmd.exe", &["/c", "start", "", url]) || try_run("xdg-open", &[url])
    } else {
        try_run("xdg-open", &[url])
    };
    if !ok {
        tracing::debug!(url, "could not open a browser; open the URL manually");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_are_wired() {
        assert!(!assets::INDEX_HTML.is_empty());
        assert!(!assets::APP_JS.is_empty());
        assert!(!assets::APP_CSS.is_empty());
        assert!(!assets::WORKLET_JS.is_empty());
        assert!(!assets::TRANSPORT_JS.is_empty());
        assert!(assets::APP_JS.contains("./transport.js"));
        assert!(assets::INDEX_HTML.contains("/app.js"));
        assert!(assets::INDEX_HTML.contains("/app.css"));
        assert!(assets::APP_JS.contains("/worklet.js"));
        // Every module app.js / editor.js import must be served in ws mode.
        for m in ["scene-url.js", "captions.js", "roll.js", "geo.js", "editor.js", "timeline.js", "coastlines.json"] {
            assert!(assets::FILES.iter().any(|(p, _, _)| p.trim_start_matches('/') == m), "{m} not embedded");
        }
        for (_, body, _) in assets::FILES {
            assert!(!body.is_empty());
        }
    }

    #[test]
    fn validate_scene_toml_reports_errors() {
        assert!(!validate_scene_toml("not toml at all = [").is_empty());
        assert!(validate_scene_toml("name = \"x\"\n[[layer]]\nname = \"a\"\nrole = \"pad\"\ninstrument = \"pad.warm\"\n").is_empty());
    }

    /// Extract a top-level `function name(...) { ... }` from app.js by brace matching.
    fn js_function(name: &str) -> String {
        let start = assets::APP_JS.find(&format!("function {name}(")).unwrap_or_else(|| panic!("no fn {name}"));
        let body = &assets::APP_JS[start..];
        let open = body.find('{').unwrap();
        let mut depth = 0i32;
        for (i, c) in body[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return body[..open + i + 1].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces in {name}");
    }

    /// Two events in the same engine tick share `at`; both must be ingested, and a
    /// later snapshot repeating the window must add nothing. Runs the real JS under
    /// `node`; skipped (with a note) when node is not installed.
    #[test]
    fn js_event_dedupe_keeps_same_tick_events() {
        let script = format!(
            "{key}\n{f}\nconst seen = new Map();\n\
             const w1 = [{{at:1,layer:0,name:'car'}},{{at:1,layer:0,name:'car'}},{{at:1,layer:1,name:'car'}}];\n\
             const a = newEvents(w1, seen).length;\n\
             const w2 = w1.concat([{{at:2,layer:0,name:'car'}},{{at:2,layer:0,name:'car'}}]);\n\
             const b = newEvents(w2, seen).length;\n\
             const c = newEvents(w2, seen).length;\n\
             console.log(JSON.stringify([a,b,c]));",
            key = "const eventKey = (e) => `${e.at}|${e.layer}|${e.name}`;",
            f = js_function("newEvents"),
        );
        let out = match std::process::Command::new("node").arg("-e").arg(&script).output() {
            Ok(o) => o,
            Err(_) => {
                eprintln!("node not installed; skipping JS dedupe test");
                return;
            }
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[3,2,0]");
    }
}
