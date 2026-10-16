//! Engine + web-server lifecycle for the desktop app. No Tauri in here: this module is
//! plain Sonder, so it type-checks on any machine the workspace builds on (the Tauri glue in
//! `main.rs` needs webkit2gtk and is verified by CI).
//!
//! One `Desktop` owns a tokio runtime, the running [`EngineHandle`] and the local web
//! server's URL. "Restart" (used by the audio-backend toggle) stops the engine, which ends
//! the web server too (it shuts down when the snapshot channel closes), then starts both
//! again on a fresh port; the window is navigated to the new URL by the caller.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use sonder_core::{Scene, UserCommand};
use sonder_engine::{AudioBackendChoice, EngineHandle, EngineOptions};

/// A started engine and the web UI serving it.
pub struct Running {
    pub handle: EngineHandle,
    pub url: String,
    pub backend: AudioBackendChoice,
}

/// Everything the menu handlers touch; stored in Tauri's managed state.
pub struct Desktop {
    rt: tokio::runtime::Runtime,
    running: Mutex<Option<Running>>,
    /// The last scene chosen from the menu / opened from a file: what a restart plays.
    scene_toml: Mutex<String>,
    /// "Record data…": the `.sonder` data log every (re)start writes (`EngineOptions::
    /// record_data`); `None` = not recording. Survives backend toggles.
    record_path: Mutex<Option<PathBuf>>,
}

impl Desktop {
    pub fn new() -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("sonder-rt")
            .enable_all()
            .build()
            .context("tokio runtime")?;
        Ok(Self { rt, running: Mutex::new(None), scene_toml: Mutex::new(String::new()), record_path: Mutex::new(None) })
    }

    /// Start the engine on `scene_toml` with `backend` and serve the UI on a free localhost
    /// port. Returns the URL. Any previous engine is shut down first.
    pub fn launch(&self, scene_toml: &str, backend: AudioBackendChoice) -> anyhow::Result<String> {
        let scene = sonder_engine::parse_scene(scene_toml)?;
        self.stop();
        let record = self.record_path.lock().unwrap().clone();
        let running = self.rt.block_on(boot(scene, backend, record))?;
        let url = running.url.clone();
        *self.scene_toml.lock().unwrap() = scene_toml.to_string();
        *self.running.lock().unwrap() = Some(running);
        Ok(url)
    }

    /// Restart with another audio backend, keeping the last menu-chosen scene.
    pub fn restart(&self, backend: AudioBackendChoice) -> anyhow::Result<String> {
        let toml = self.scene_toml.lock().unwrap().clone();
        self.launch(&toml, backend)
    }

    /// Restart the engine recording its data log to `path` (docs/V3.md §1; the log starts
    /// with the engine, so a restart is needed). Returns the new URL.
    pub fn record_to(&self, path: PathBuf) -> anyhow::Result<String> {
        *self.record_path.lock().unwrap() = Some(path);
        let backend = self.backend().unwrap_or_default();
        self.restart(backend)
    }

    pub fn record_path(&self) -> Option<PathBuf> {
        self.record_path.lock().unwrap().clone()
    }

    pub fn url(&self) -> Option<String> {
        self.running.lock().unwrap().as_ref().map(|r| r.url.clone())
    }

    pub fn backend(&self) -> Option<AudioBackendChoice> {
        self.running.lock().unwrap().as_ref().map(|r| r.backend)
    }

    /// Send a command to the running engine (errors when no engine is up).
    pub fn send(&self, cmd: UserCommand) -> anyhow::Result<()> {
        let tx = {
            let guard = self.running.lock().unwrap();
            guard.as_ref().map(|r| r.handle.commands.clone()).context("the engine is not running")?
        };
        self.rt.block_on(tx.send(cmd)).map_err(|_| anyhow::anyhow!("the engine has stopped"))
    }

    /// Validate and hot-load a scene (built-in or from a file) into the running engine.
    pub fn load_scene_toml(&self, toml: String) -> anyhow::Result<()> {
        let errs = sonder_web::validate_scene_toml(&toml);
        if !errs.is_empty() {
            anyhow::bail!("scene is invalid:\n  {}", errs.join("\n  "));
        }
        *self.scene_toml.lock().unwrap() = toml.clone();
        self.send(UserCommand::LoadScene { toml })
    }

    /// Stop the engine (and with it the web server); waits briefly for a clean exit
    /// (MIDI note-offs, audio thread join).
    pub fn stop(&self) {
        let running = self.running.lock().unwrap().take();
        if let Some(mut r) = running {
            r.handle.shutdown();
            // Built inside the runtime: `timeout` needs the reactor at construction time.
            let _ = self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(3), &mut r.handle.done).await });
        }
    }
}

/// The built-in scene library as (name, title, toml).
pub fn builtin_scenes() -> Vec<(&'static str, String, &'static str)> {
    sonder_engine::scenes::BUILTIN_SCENE_SOURCES
        .iter()
        .map(|(name, src)| {
            let title = sonder_engine::parse_scene(src).map(|s: Scene| s.name).unwrap_or_else(|_| name.to_string());
            (*name, title, *src)
        })
        .collect()
}

async fn boot(scene: Scene, backend: AudioBackendChoice, record_data: Option<PathBuf>) -> anyhow::Result<Running> {
    let opts = EngineOptions { backend, web_tap: true, record_data, ..EngineOptions::default() };
    let handle = sonder_engine::start(scene, opts).await.context("starting the engine")?;
    tracing::info!(audio = %handle.audio_backend, "engine started");

    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .context("binding a local port for the web ui")?;
    let addr = listener.local_addr()?;
    let web = sonder_web::WebHandle {
        snapshot: handle.snapshot.clone(),
        commands: handle.commands.clone(),
        audio: handle.subscribe_audio().map(|rx| Box::new(BroadcastAudio { rx }) as Box<dyn sonder_web::AudioSource>),
        sample_rate: handle.sample_rate,
    };
    tokio::spawn(async move {
        if let Err(e) = sonder_web::serve_on(listener, web).await {
            tracing::error!("web ui: {e}");
        }
    });
    Ok(Running { handle, url: format!("http://{addr}/"), backend })
}

/// Adapts the engine's broadcast of f32 chunks to the web server's pull interface
/// (same as the CLI's).
struct BroadcastAudio {
    rx: tokio::sync::broadcast::Receiver<std::sync::Arc<Vec<f32>>>,
}

impl sonder_web::AudioSource for BroadcastAudio {
    fn pop_chunk(&mut self, out: &mut Vec<f32>) -> usize {
        let mut n = 0;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    out.extend_from_slice(&chunk);
                    n += chunk.len();
                    if n >= 4800 {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_scene_titles_parse() {
        let scenes = builtin_scenes();
        assert!(scenes.iter().any(|(n, t, _)| *n == "night-freeway" && t == "Night Freeway"));
    }

    /// Silent backend: start, serve, hot-load, restart on a new port, stop.
    #[test]
    fn launch_restart_stop() {
        let d = Desktop::new().unwrap();
        let (_, _, toml) = builtin_scenes().into_iter().find(|(n, _, _)| *n == "night-freeway").unwrap();
        let url = d.launch(toml, AudioBackendChoice::Null).unwrap();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert_eq!(d.backend(), Some(AudioBackendChoice::Null));
        assert!(d.load_scene_toml("name = 'x'\n[[layer]]\nname = 'a'\nrole = 'pad'\ninstrument = 'nope'".into()).is_err());
        d.load_scene_toml("name = \"Bare\"\n[[layer]]\nname = \"drone\"\nrole = \"pad\"\ninstrument = \"pad.glass\"\n".into()).unwrap();
        let url2 = d.restart(AudioBackendChoice::Null).unwrap();
        assert_ne!(url, url2);
        let log = std::env::temp_dir().join(format!("sonder-desktop-test-{}.sonder", std::process::id()));
        d.record_to(log.clone()).unwrap();
        assert_eq!(d.record_path(), Some(log.clone()));
        d.stop();
        assert!(log.is_file(), "data log was not created");
        let _ = std::fs::remove_file(&log);
        assert!(d.url().is_none());
        assert!(d.send(UserCommand::TogglePause).is_err());
    }
}
