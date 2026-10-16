//! `sonder` — listen to the world happening.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use sonder_core::{Scene, SourceSpec};
use sonder_engine::{AudioBackendChoice, EngineOptions};

mod radio;
mod record;

#[derive(Parser)]
#[command(name = "sonder", version, about = "A realtime generative music engine that listens to the world.", long_about = None)]
struct Cli {
    /// Log filter (e.g. `info`, `sonder_sources=debug`). Logs go to stderr (or a file with --log-file).
    #[arg(long, global = true, default_value = "warn")]
    log: String,
    /// Write logs to this file instead of stderr (recommended with the TUI).
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    /// Subcommand; with none, `play` starts the default scene.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Play a scene live, with the terminal UI and the web UI.
    Play(PlayArgs),
    /// Run headless: no TUI, just the web UI (and audio).
    Serve(PlayArgs),
    /// Render a scene offline to a WAV file (deterministic for offline sources).
    Render {
        /// Built-in scene name or path to a .toml
        scene: String,
        #[arg(long, default_value_t = 60.0)]
        seconds: f64,
        #[arg(long, short, default_value = "sonder.wav")]
        out: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[command(flatten)]
        source: SourceOverride,
    },
    /// List built-in scenes.
    List,
    /// List source kinds and their parameters.
    Sources {
        /// Show details for one kind.
        kind: Option<String>,
    },
    /// List instrument presets.
    Instruments,
    /// Poll a JSON endpoint, show what's in it, and write a starter scene.
    Probe {
        url: String,
        #[arg(long, default_value_t = 3)]
        polls: usize,
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Write the starter scene here (default: print to stdout).
        #[arg(long, short)]
        out: Option<PathBuf>,
    },
    /// Validate a scene file.
    Validate { scene: String },
    /// Print a built-in scene's TOML (to copy and tweak).
    Show { scene: String },
    /// List MIDI output ports (for --midi).
    MidiPorts,
    /// Stream a scene 24/7 through ffmpeg to Icecast, an HLS directory or a file (docs/RADIO.md).
    Radio(radio::RadioArgs),
    /// Render today's dated track (seed = date) into an album directory with an index page.
    Record(record::RecordArgs),
    /// Play a recorded data log (time travel): the scene it was recorded with, or another.
    Replay(ReplayArgs),
    /// Compose a scene from a prompt with Claude (needs ANTHROPIC_API_KEY).
    Compose(ComposeArgs),
    /// Print the composer catalog (source kinds, recipes, presets, system prompt).
    Catalog {
        /// JSON (what `scripts/build-site.sh` writes to site/catalog.json); default: the system prompt as text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Clone)]
struct PlayArgs {
    /// Built-in scene name or path to a .toml
    #[arg(default_value = "night-freeway")]
    scene: String,
    /// Web UI port (0 = disabled).
    #[arg(long, default_value_t = 7777)]
    web: u16,
    /// Bind address for the web UI.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Open the web UI in a browser at start.
    #[arg(long)]
    open: bool,
    /// Don't open a native audio device (listen via the web UI).
    #[arg(long)]
    no_audio: bool,
    /// Write s16le stereo PCM to stdout instead of a device (pipe to ffplay/aplay/paplay).
    #[arg(long)]
    pcm_stdout: bool,
    /// Preferred sample rate.
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
    /// RNG seed (default: scene's, else random).
    #[arg(long)]
    seed: Option<u64>,
    /// Also send MIDI to this output port (substring match), or `virtual` to create one.
    #[arg(long, value_name = "PORT")]
    midi: Option<String>,
    /// Reload the scene in place whenever its file changes on disk (polled every second).
    #[arg(long)]
    watch: bool,
    /// Write every ingested sample to this `.sonder` (or `.sonder.gz`) data log, for `sonder replay`.
    #[arg(long, value_name = "PATH")]
    record_data: Option<PathBuf>,
    #[command(flatten)]
    source: SourceOverride,
}

#[derive(Args, Clone)]
struct ReplayArgs {
    /// A `.sonder` / `.sonder.gz` data log (from `--record-data` or `sonder record`).
    #[arg(value_name = "LOG")]
    log_path: PathBuf,
    /// Scene to play the log through (default: the scene named in the log's header, if built in).
    #[arg(long)]
    scene: Option<String>,
    /// Playback speed (1 = real time).
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Start this many seconds into the log.
    #[arg(long)]
    from: Option<f64>,
    /// Stop at this many seconds into the log.
    #[arg(long)]
    to: Option<f64>,
    /// Start over when the log ends.
    #[arg(long = "loop")]
    looping: bool,
    /// Web UI port (0 = disabled).
    #[arg(long, default_value_t = 7777)]
    web: u16,
    /// Bind address for the web UI.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Open the web UI in a browser at start.
    #[arg(long)]
    open: bool,
    /// Don't open a native audio device (listen via the web UI).
    #[arg(long)]
    no_audio: bool,
    /// Write s16le stereo PCM to stdout instead of a device.
    #[arg(long)]
    pcm_stdout: bool,
    /// Run headless (no TUI).
    #[arg(long)]
    headless: bool,
    /// RNG seed (default: the seed recorded in the log, so the music comes back too).
    #[arg(long)]
    seed: Option<u64>,
    /// Also send MIDI to this output port (substring match), or `virtual`.
    #[arg(long, value_name = "PORT")]
    midi: Option<String>,
}

#[derive(Args, Clone, Default)]
struct SourceOverride {
    /// Replace the scene's (first) data source with this kind, e.g. `camera.mjpeg`.
    #[arg(long)]
    source: Option<String>,
    /// Which layer's source to replace (default: the first layer that has one).
    #[arg(long, requires = "source")]
    layer: Option<String>,
    /// Parameter for --source, `key=value` (repeatable). Values parse as JSON when possible.
    #[arg(long = "param", short = 'p', value_name = "KEY=VALUE")]
    params: Vec<String>,
}

impl SourceOverride {
    fn into_overrides(self) -> anyhow::Result<Vec<(String, SourceSpec)>> {
        let Some(kind) = self.source else {
            if !self.params.is_empty() {
                anyhow::bail!("--param requires --source");
            }
            return Ok(Vec::new());
        };
        let mut spec = SourceSpec::new(kind);
        for p in self.params {
            let (k, v) = p.split_once('=').with_context(|| format!("--param `{p}` must be KEY=VALUE"))?;
            let value = serde_json::from_str::<serde_json::Value>(v).unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
            spec.params.insert(k.to_string(), value);
        }
        Ok(vec![(self.layer.unwrap_or_default(), spec)])
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Bare `sonder` = play the default scene.
    let cmd = match cli.cmd {
        Some(c) => c,
        None => Cli::parse_from(["sonder", "play"]).cmd.expect("play subcommand"),
    };
    // The TUI needs a real terminal; without one (piped, launched from a GUI, CI) `play`
    // becomes `serve` so the web UI still works instead of failing on raw mode.
    let has_tty = std::io::IsTerminal::is_terminal(&std::io::stdin()) && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let cmd = match cmd {
        Cmd::Play(args) if !has_tty => {
            eprintln!("sonder: no terminal for the TUI — running headless; use the web UI");
            Cmd::Serve(args)
        }
        other => other,
    };
    init_logging(&cli.log, cli.log_file.as_deref(), matches!(cmd, Cmd::Play(_)))?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match cmd {
        Cmd::Play(args) => rt.block_on(async {
            let scene = sonder_engine::load_scene(&args.scene)?;
            play(scene, args, true, None).await
        }),
        Cmd::Serve(args) => rt.block_on(async {
            let scene = sonder_engine::load_scene(&args.scene)?;
            play(scene, args, false, None).await
        }),
        Cmd::Replay(args) => rt.block_on(replay(args)),
        Cmd::Render { scene, seconds, out, seed, source } => {
            let mut scene = sonder_engine::load_scene(&scene)?;
            sonder_engine::live::apply_overrides(&mut scene, &source.into_overrides()?)?;
            eprintln!("rendering {seconds}s of `{}` (seed {seed}) → {}", scene.name, out.display());
            let stats = sonder_engine::render_to_wav(&scene, seconds, seed, &out)?;
            eprintln!(
                "done: {} frames @ {} Hz, peak {:.3}, rms {:.3}, {} notes, {} events",
                stats.frames, stats.sample_rate, stats.peak, stats.rms, stats.notes, stats.events
            );
            Ok(())
        }
        Cmd::List => {
            println!("{:<18} {:<8} {:<7} DESCRIPTION", "SCENE", "LAYERS", "NET");
            for (name, scene) in sonder_engine::builtin_scenes() {
                let net = if scene_needs_network(&scene) { "yes" } else { "no" };
                println!("{:<18} {:<8} {:<7} {}", name, scene.layers.len(), net, scene.description);
            }
            Ok(())
        }
        Cmd::Sources { kind } => {
            let reg = sonder_engine::live::registry();
            match kind {
                None => {
                    println!("{:<26} {:<4} SUMMARY", "KIND", "NET");
                    for info in reg.catalog() {
                        println!("{:<26} {:<4} {}", info.kind, if info.network { "yes" } else { "no" }, info.summary);
                    }
                    println!("\nrecipes (generic transport + engine-side mapping; see recipes/README.md):");
                    for r in sonder_engine::recipes::all() {
                        println!("{:<26} {:<4} {}", r.recipe.kind, if r.recipe.network { "yes" } else { "no" }, r.recipe.summary);
                    }
                    println!("\n`sonder sources <kind>` for parameters, channels and events.");
                }
                Some(k) if sonder_engine::recipes::is_recipe(&k) => {
                    let r = sonder_engine::recipes::get(&k).context("recipe vanished")?;
                    println!("{} (recipe)\n  {}\n", r.recipe.kind, r.recipe.summary);
                    println!("parameters:");
                    for (name, p) in &r.params {
                        let d = p.default.as_ref().map(|d| format!(" (default {d})")).unwrap_or_else(|| " (required)".into());
                        println!("  {:<16} {}{}", name, p.summary, d);
                    }
                    let expanded = sonder_engine::recipes::expand(&SourceSpec::new(&k)).map_err(anyhow::Error::msg)?;
                    println!("expands to:        {} {}", expanded.kind, serde_json::to_string(&expanded.params).unwrap_or_default());
                }
                Some(k) => {
                    let info = reg.info(&k).with_context(|| format!("unknown source kind `{k}`"))?;
                    println!("{}\n  {}\n", info.kind, info.summary);
                    println!("parameters:");
                    for p in &info.params {
                        let d = p.default.map(|d| format!(" (default {d})")).unwrap_or_else(|| " (required)".into());
                        println!("  {:<16} {}{}", p.name, p.summary, d);
                    }
                    println!("channels (field):  {}", info.channels.join(", "));
                    println!("events (event):    {}", info.events.join(", "));
                }
            }
            Ok(())
        }
        Cmd::Instruments => {
            for name in sonder_dsp::presets::names() {
                println!("{:<16} {}", name, sonder_dsp::presets::describe(name).unwrap_or(""));
            }
            Ok(())
        }
        Cmd::Probe { url, polls, interval, out } => rt.block_on(async move {
            eprintln!("probing {url} ({polls}× every {interval}s)…");
            let report = sonder_sources::probe::probe_json(&url, polls, std::time::Duration::from_secs_f64(interval)).await?;
            for f in &report.fields {
                let vals: Vec<String> = f.values.iter().map(|v| format!("{v:.4}")).collect();
                println!("  {:<32} {:<36} {}", f.path, vals.join(" → "), if f.changed { f.suggestion.as_str() } else { "(constant)" });
            }
            if let Some(s) = &report.events_suggestion {
                println!("  events: {s}");
            }
            let scene = sonder_sources::probe::starter_scene(&report, &url);
            let text = toml::to_string_pretty(&scene)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, text)?;
                    eprintln!("wrote starter scene to {}  —  sonder play {}", p.display(), p.display());
                }
                None => println!("\n{text}"),
            }
            Ok(())
        }),
        Cmd::Validate { scene } => {
            let s = sonder_engine::load_scene(&scene)?;
            match sonder_engine::validate(&s) {
                Ok(()) => {
                    println!("ok: `{}` — {} layers", s.name, s.layers.len());
                    Ok(())
                }
                Err(errs) => {
                    for e in &errs {
                        eprintln!("error: {e}");
                    }
                    std::process::exit(1)
                }
            }
        }
        #[cfg(not(feature = "midi"))]
        Cmd::MidiPorts => anyhow::bail!("this build has no MIDI support (feature `midi`)"),
        #[cfg(feature = "midi")]
        Cmd::MidiPorts => {
            let ports = sonder_engine::midi::MidiOut::ports()?;
            if ports.is_empty() {
                println!("no MIDI output ports (use `--midi virtual` to create one)");
            }
            for p in ports {
                println!("{p}");
            }
            Ok(())
        }
        Cmd::Compose(args) => rt.block_on(compose(args)),
        Cmd::Catalog { json } => {
            if json {
                println!("{}", catalog_json_native());
            } else {
                print!("{}", system_prompt_native());
            }
            Ok(())
        }
        Cmd::Radio(args) => rt.block_on(radio::run(args)),
        Cmd::Record(args) => record::run(args),
        Cmd::Show { scene } => {
            let (_, src) = sonder_engine::scenes::BUILTIN_SCENE_SOURCES
                .iter()
                .find(|(n, _)| *n == scene)
                .with_context(|| format!("no built-in scene `{scene}`"))?;
            print!("{src}");
            Ok(())
        }
    }
}

fn scene_needs_network(scene: &Scene) -> bool {
    let reg = sonder_engine::live::registry();
    scene.layers.iter().any(|l| match &l.source {
        sonder_core::LayerSource::Spec(s) => reg.info(&s.kind).is_some_and(|i| i.network),
        _ => false,
    })
}

/// `sonder replay`: resolve the scene (the log's own, or `--scene`) and play the log into it.
async fn replay(args: ReplayArgs) -> anyhow::Result<()> {
    let log = sonder_engine::datalog::DataLog::open(&args.log_path).with_context(|| format!("opening data log {}", args.log_path.display()))?;
    let header = log.header().clone();
    let scene = match &args.scene {
        Some(s) => sonder_engine::load_scene(s)?,
        None => builtin_scene_named(&header.scene).with_context(|| {
            format!(
                "the log was recorded with scene `{}`, which is not a built-in scene; pass --scene <name|path> to choose one (the log carries sources: {})",
                header.scene,
                log.source_names().join(", ")
            )
        })?,
    };
    let fed: Vec<String> = {
        let names = log.source_names();
        scene.layers.iter().filter(|l| matches!(l.source, sonder_core::LayerSource::Spec(_)) && names.contains(&l.name)).map(|l| l.name.clone()).collect()
    };
    if fed.is_empty() {
        eprintln!(
            "sonder replay: warning: the log's sources ({}) match none of scene `{}`'s ({}); its layers will play on the conductor alone",
            log.source_names().join(", "),
            scene.name,
            scene.layers.iter().filter(|l| matches!(l.source, sonder_core::LayerSource::Spec(_))).map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    if !log.audio_dropped().is_empty() {
        eprintln!("sonder replay: note: audio from {} was not recorded (logs hold data, not sound)", log.audio_dropped().iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    }
    let title = args.log_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| args.log_path.display().to_string());
    eprintln!(
        "sonder replay: {title} — {} ({}), {} of data, {} samples → scene `{}`{}",
        header.scene,
        header.created,
        mmss(log.length_seconds()),
        log.entries().len(),
        scene.name,
        if args.speed != 1.0 { format!(" at {}×", args.speed) } else { String::new() }
    );
    let config = sonder_engine::replay::ReplayConfig {
        speed: args.speed,
        from: args.from.unwrap_or(0.0),
        to: args.to,
        looping: args.looping,
        rename: Default::default(),
    };
    let replay = sonder_engine::live::ReplayOptions { log: std::sync::Arc::new(log), title, config };
    let play_args = PlayArgs {
        scene: args.scene.clone().unwrap_or_else(|| header.scene.clone()),
        web: args.web,
        bind: args.bind,
        open: args.open,
        no_audio: args.no_audio,
        pcm_stdout: args.pcm_stdout,
        sample_rate: 48_000,
        seed: Some(args.seed.unwrap_or(header.seed)),
        midi: args.midi,
        watch: false,
        record_data: None,
        source: SourceOverride::default(),
    };
    play(scene, play_args, !args.headless, Some(replay)).await
}

/// A built-in scene by slug (`night-freeway`) or title (`Night Freeway`), case-insensitive.
fn builtin_scene_named(name: &str) -> Option<Scene> {
    let want = name.trim().to_ascii_lowercase();
    sonder_engine::builtin_scenes()
        .into_iter()
        .find(|(slug, scene)| slug.eq_ignore_ascii_case(&want) || scene.name.to_ascii_lowercase() == want)
        .map(|(_, s)| s)
}

fn mmss(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

async fn play(scene: Scene, args: PlayArgs, tui: bool, replay: Option<sonder_engine::live::ReplayOptions>) -> anyhow::Result<()> {
    let backend = if args.pcm_stdout {
        AudioBackendChoice::PcmStdout
    } else if args.no_audio {
        AudioBackendChoice::Null
    } else {
        AudioBackendChoice::Auto
    };
    let opts = EngineOptions {
        backend,
        sample_rate: args.sample_rate,
        seed: args.seed,
        web_tap: args.web != 0,
        source_overrides: args.source.into_overrides()?,
        midi_port: args.midi.clone(),
        replay,
        record_data: args.record_data.clone(),
    };
    let handle = sonder_engine::start(scene, opts).await?;
    if let Some(p) = &handle.midi_port {
        tracing::info!("midi out → {p}");
    }
    if let Some(p) = &args.record_data {
        eprintln!("sonder: recording data to {} (play it back with `sonder replay {}`)", p.display(), p.display());
    }
    if args.watch {
        let path = scene_file_path(&args.scene).context("--watch needs a scene file, not a built-in name")?;
        tokio::spawn(watch_scene_file(path, handle.commands.clone()));
    }
    let web_url = if args.web != 0 {
        let addr: SocketAddr = format!("{}:{}", args.bind, args.web).parse().context("bad --bind/--web")?;
        let web_handle = sonder_web::WebHandle {
            snapshot: handle.snapshot.clone(),
            commands: handle.commands.clone(),
            audio: handle.subscribe_audio().map(|rx| Box::new(BroadcastAudio { rx }) as Box<dyn sonder_web::AudioSource>),
            sample_rate: handle.sample_rate,
        };
        tokio::spawn(async move {
            if let Err(e) = sonder_web::serve(addr, web_handle).await {
                tracing::error!("web ui: {e}");
            }
        });
        let url = format!("http://{addr}/");
        if args.open {
            sonder_web::open_browser(&url);
        }
        Some(url)
    } else {
        None
    };

    let mut handle = handle;
    if tui {
        let opts = sonder_tui::TuiOptions { web_url: web_url.clone(), ..Default::default() };
        let tui_run = sonder_tui::run(handle.snapshot.clone(), handle.commands.clone(), opts);
        tokio::pin!(tui_run);
        // The engine can end on its own (quit command, control-thread failure); don't leave
        // the TUI showing a frozen snapshot when it does.
        let res = tokio::select! {
            r = &mut tui_run => r,
            r = &mut handle.done => {
                handle.shutdown();
                match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(anyhow::anyhow!("engine stopped unexpectedly")),
                }
            }
        };
        handle.shutdown();
        // Let the engine finish (MIDI note-offs, audio thread join) before the process exits.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut handle.done).await;
        res?;
    } else {
        eprintln!("sonder: playing `{}` — audio: {}", args.scene, handle.audio_backend);
        if let Some(u) = &web_url {
            eprintln!("sonder: web ui at {u}");
        }
        eprintln!("sonder: Ctrl-C to stop");
        tokio::select! {
            _ = stop_signal() => {
                handle.shutdown();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut handle.done).await;
            },
            r = &mut handle.done => {
                // A dropped sender means the control thread died (panicked) without reporting.
                r.map_err(|_| anyhow::anyhow!("engine control thread exited unexpectedly"))??;
            },
        }
    }
    Ok(())
}

/// The on-disk file a scene argument refers to, if it is one (`load_scene` semantics).
fn scene_file_path(name_or_path: &str) -> Option<PathBuf> {
    if sonder_engine::scenes::BUILTIN_SCENE_SOURCES.iter().any(|(n, _)| *n == name_or_path) {
        return None;
    }
    let p = PathBuf::from(name_or_path);
    [p.clone(), p.with_extension("toml")].into_iter().find(|p| p.is_file())
}

/// Poll a scene file's mtime once a second and hot-reload it when it changes.
async fn watch_scene_file(path: PathBuf, commands: tokio::sync::mpsc::Sender<sonder_core::UserCommand>) {
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut last = mtime(&path);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let now = mtime(&path);
        if now.is_some() && now != last {
            last = now;
            // Editors often write in two steps; give the file a moment to settle.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            match std::fs::read_to_string(&path) {
                Ok(toml) => {
                    tracing::info!(path = %path.display(), "scene file changed; reloading");
                    if commands.send(sonder_core::UserCommand::LoadScene { toml }).await.is_err() {
                        return;
                    }
                }
                Err(e) => tracing::warn!(path = %path.display(), "cannot read changed scene: {e}"),
            }
        }
    }
}

/// Adapts the engine's broadcast of f32 chunks to the web server's pull interface.
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

fn init_logging(filter: &str, file: Option<&std::path::Path>, tui: bool) -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("warn"));
    match file {
        Some(p) => {
            let f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::sync::Mutex::new(f)).with_ansi(false).init();
        }
        None if tui => {
            // The TUI owns the terminal; default to a log file next to it.
            let f = std::fs::OpenOptions::new().create(true).append(true).open("sonder.log")?;
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::sync::Mutex::new(f)).with_ansi(false).init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    Ok(())
}

// ── compose: LLM scene composer ──────────────────────────────────────────────────────

use sonder_engine::compose_prompt::{self, ParamLike, SourceInfoLike};

/// The model `sonder compose` uses unless `--model` says otherwise.
const DEFAULT_COMPOSE_MODEL: &str = "claude-opus-5-5";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Models that accept the server-side refusal fallback (`fallbacks: "default"`).
const FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-fable-5-1"];

#[derive(Args, Clone)]
struct ComposeArgs {
    /// What to compose, e.g. "a slow, dark piece from live earthquakes and the solar wind".
    prompt: String,
    /// Write the scene here (default: print to stdout).
    #[arg(long, short)]
    out: Option<PathBuf>,
    /// Claude model id.
    #[arg(long, default_value = DEFAULT_COMPOSE_MODEL)]
    model: String,
    /// Start playing the composed scene (TUI + web UI) once it validates.
    #[arg(long)]
    play: bool,
    /// How many times to send validation errors back for a fix.
    #[arg(long, default_value_t = 3)]
    retries: u32,
    /// Don't request the server-side refusal fallback (enabled by default on models that support it).
    #[arg(long)]
    no_fallback: bool,
}

/// The live source registry in prompt form.
fn source_catalog() -> Vec<SourceInfoLike> {
    sonder_engine::live::registry()
        .catalog()
        .into_iter()
        .map(|i| SourceInfoLike {
            kind: i.kind.to_string(),
            summary: i.summary.to_string(),
            params: i.params.iter().map(|p| ParamLike::new(p.name, p.summary, p.default.map(str::to_string))).collect(),
            channels: i.channels.iter().map(|c| c.to_string()).collect(),
            events: i.events.iter().map(|e| e.to_string()).collect(),
            network: i.network,
        })
        .collect()
}

fn catalog_json_native() -> String {
    compose_prompt::catalog_json(&source_catalog())
}

fn system_prompt_native() -> String {
    compose_prompt::system_prompt(&source_catalog(), &compose_prompt::recipes_catalog(), &compose_prompt::presets_catalog())
}

/// Pull the scene out of a model reply: the first fenced ```toml block, else the first fenced
/// block of any language, else the whole text if it looks like a scene.
fn extract_toml_block(text: &str) -> Option<String> {
    let mut best: Option<(bool, String)> = None;
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let nl = after.find('\n')?;
        let lang = after[..nl].trim().to_ascii_lowercase();
        let body = &after[nl + 1..];
        let Some(end) = body.find("```") else { break };
        let inner = body[..end].trim_end().to_string() + "\n";
        let is_toml = lang == "toml";
        if is_toml {
            return Some(inner);
        }
        if best.is_none() {
            best = Some((false, inner));
        }
        rest = &body[end + 3..];
    }
    if let Some((_, b)) = best {
        return Some(b);
    }
    let t = text.trim();
    (t.contains("[[layer]]") && t.contains("[conductor]")).then(|| format!("{t}\n"))
}

/// The follow-up user message that asks for a fix.
fn retry_message(errs: &[String]) -> String {
    let mut m = String::from("The scene did not validate. Fix every error below and return the WHOLE corrected scene as a single ```toml block, nothing else.\n\nErrors:\n");
    for e in errs {
        m.push_str("- ");
        m.push_str(e);
        m.push('\n');
    }
    m
}

/// Parse + full validation, with parse errors in the same `Vec<String>` shape.
fn validate_toml(toml_src: &str) -> Result<Scene, Vec<String>> {
    let scene = sonder_engine::parse_scene(toml_src).map_err(|e| vec![format!("{e:#}")])?;
    sonder_engine::validate(&scene)?;
    Ok(scene)
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
}

/// (input, output) USD per million tokens, first-party API list prices.
fn price_per_mtok(model: &str) -> Option<(f64, f64)> {
    Some(match model {
        "claude-opus-5-5" => (4.0, 20.0),
        "claude-fable-5-1" | "claude-fable-5" | "claude-mythos-5-1" => (10.0, 50.0),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" => (5.0, 25.0),
        "claude-sonnet-5-5" | "claude-sonnet-5" => (2.0, 10.0),
        "claude-sonnet-4-6" => (3.0, 15.0),
        "claude-haiku-4-5" => (1.0, 5.0),
        _ => return None,
    })
}

/// Cost estimate in USD. Cache writes bill at 1.25× input, cache reads at ~0.1× (0.05× on the
/// 5.x models; this errs high).
fn cost_estimate(model: &str, u: &Usage) -> Option<f64> {
    let (i, o) = price_per_mtok(model)?;
    let m = 1e-6;
    Some(u.input as f64 * i * m + u.cache_write as f64 * i * 1.25 * m + u.cache_read as f64 * i * 0.1 * m + u.output as f64 * o * m)
}

/// A parsed Messages API reply.
struct Reply {
    text: String,
    stop_reason: String,
    usage: Usage,
    /// `stop_details.explanation` on a refusal.
    refusal: Option<String>,
}

fn parse_reply(v: &serde_json::Value) -> Reply {
    let text = v["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""))
        .unwrap_or_default();
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Reply {
        text,
        stop_reason: v["stop_reason"].as_str().unwrap_or("").to_string(),
        usage: Usage { input: n("input_tokens"), output: n("output_tokens"), cache_read: n("cache_read_input_tokens"), cache_write: n("cache_creation_input_tokens") },
        refusal: v["stop_details"]["explanation"].as_str().map(str::to_string),
    }
}

fn request_body(model: &str, system: &str, messages: &[serde_json::Value], fallback: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": 16000,
        "system": [{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }],
        "messages": messages,
    });
    if fallback {
        body["fallbacks"] = serde_json::Value::String("default".into());
    }
    body
}

async fn call_messages(client: &reqwest::Client, key: &str, body: &serde_json::Value, fallback: bool) -> anyhow::Result<Reply> {
    let mut req = client
        .post(MESSAGES_URL)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(body);
    if fallback {
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let resp = req.send().await.context("calling the Claude API")?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.context("reading the Claude API response")?;
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or("(no message)");
        let kind = v["error"]["type"].as_str().unwrap_or("error");
        anyhow::bail!("Claude API {status}: {kind}: {msg}");
    }
    Ok(parse_reply(&v))
}

async fn compose(args: ComposeArgs) -> anyhow::Result<()> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .context("ANTHROPIC_API_KEY is not set (export it, or create a key at console.anthropic.com)")?;
    let fallback = !args.no_fallback && FALLBACK_MODELS.contains(&args.model.as_str());
    let system = system_prompt_native();
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(600)).build()?;
    let mut messages = vec![serde_json::json!({ "role": "user", "content": args.prompt })];
    let mut total = Usage::default();
    let mut scene_toml: Option<String> = None;
    let mut last_errs: Vec<String> = Vec::new();

    eprintln!(
        "sonder compose: {} ({} chars of context{})",
        args.model,
        system.len(),
        if fallback { ", refusal fallback on" } else { "" }
    );
    for attempt in 0..=args.retries {
        if attempt > 0 {
            eprintln!("sonder compose: attempt {}/{} — sending {} validation error(s) back", attempt + 1, args.retries + 1, last_errs.len());
        }
        let body = request_body(&args.model, &system, &messages, fallback);
        let reply = call_messages(&client, &key, &body, fallback).await?;
        total.add(reply.usage);
        if reply.stop_reason == "refusal" {
            anyhow::bail!("the model declined this request{}", reply.refusal.map(|e| format!(": {e}")).unwrap_or_default());
        }
        if reply.stop_reason == "max_tokens" {
            eprintln!("sonder compose: warning: reply hit max_tokens; the scene may be cut off");
        }
        let errs = match extract_toml_block(&reply.text) {
            None => vec!["reply contained no ```toml block".to_string()],
            Some(toml_src) => match validate_toml(&toml_src) {
                Ok(_) => {
                    scene_toml = Some(toml_src);
                    break;
                }
                Err(errs) => errs,
            },
        };
        for e in &errs {
            eprintln!("  invalid: {e}");
        }
        messages.push(serde_json::json!({ "role": "assistant", "content": reply.text }));
        messages.push(serde_json::json!({ "role": "user", "content": retry_message(&errs) }));
        last_errs = errs;
    }

    let cost = cost_estimate(&args.model, &total).map(|c| format!(" ≈ ${c:.4}")).unwrap_or_else(|| " (no price table for this model)".into());
    eprintln!(
        "sonder compose: tokens in {} (cache write {}, read {}), out {}{}",
        total.input, total.cache_write, total.cache_read, total.output, cost
    );
    let Some(toml_src) = scene_toml else {
        anyhow::bail!("no valid scene after {} attempt(s); last errors:\n  {}", args.retries + 1, last_errs.join("\n  "));
    };

    let path = match &args.out {
        Some(p) => {
            std::fs::write(p, &toml_src).with_context(|| format!("writing {}", p.display()))?;
            eprintln!("sonder compose: wrote {}  —  sonder play {}", p.display(), p.display());
            Some(p.clone())
        }
        None if args.play => {
            let p = std::env::temp_dir().join(format!("sonder-compose-{}.toml", std::process::id()));
            std::fs::write(&p, &toml_src)?;
            eprintln!("sonder compose: scene saved to {}", p.display());
            Some(p)
        }
        None => {
            print!("{toml_src}");
            None
        }
    };
    if args.play {
        let scene = path.expect("a path when playing").to_string_lossy().into_owned();
        let loaded = sonder_engine::load_scene(&scene)?;
        play(
            loaded,
            PlayArgs {
                scene,
                web: 7777,
                bind: "127.0.0.1".into(),
                open: false,
                no_audio: false,
                pcm_stdout: false,
                sample_rate: 48000,
                seed: None,
                midi: None,
                watch: false,
                record_data: None,
                source: SourceOverride::default(),
            },
            true,
            None,
        )
        .await?;
    }
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (systemd stop, `timeout`), so data logs flush cleanly.
pub(crate) async fn stop_signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(t) => t,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}


#[cfg(test)]
mod compose_tests {
    use super::*;

    #[test]
    fn extracts_toml_block_variants() {
        let reply = "Here you go:\n\n```toml\nname = \"x\"\n[conductor]\nkey = \"D\"\n```\n\nEnjoy.";
        assert_eq!(extract_toml_block(reply).unwrap(), "name = \"x\"\n[conductor]\nkey = \"D\"\n");
        // A toml block wins over an earlier unlabelled one.
        let two = "```\nnot it\n```\ntext\n```toml\nname = \"y\"\n```";
        assert_eq!(extract_toml_block(two).unwrap(), "name = \"y\"\n");
        // Unlabelled fence is accepted when there is nothing better.
        assert_eq!(extract_toml_block("```\nname = \"z\"\n```").unwrap(), "name = \"z\"\n");
        // Bare TOML that looks like a scene is accepted; prose is not.
        assert!(extract_toml_block("[conductor]\nkey = \"D\"\n[[layer]]\nname = \"a\"").is_some());
        assert!(extract_toml_block("Sorry, I cannot do that.").is_none());
        assert!(extract_toml_block("```toml\nunterminated").is_none());
    }

    #[test]
    fn retry_message_lists_errors() {
        let m = retry_message(&["layer `a`: unknown instrument `pad.x`".into(), "conductor.mode `zz` is not a known mode".into()]);
        assert!(m.starts_with("The scene did not validate."));
        assert!(m.contains("- layer `a`: unknown instrument `pad.x`\n"));
        assert!(m.contains("- conductor.mode `zz` is not a known mode\n"));
        assert!(m.contains("```toml"));
    }

    #[test]
    fn canned_reply_parses_and_validates() {
        let canned = serde_json::json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": "", "signature": "x"},
                {"type": "text", "text": "```toml\nname = \"Test\"\n[conductor]\nkey = \"D\"\nmode = \"dorian\"\ntempo = 70\n\n[[layer]]\nname = \"bed\"\nrole = \"pad\"\ninstrument = \"pad.warm\"\nsource = { kind = \"synthetic.freeway\" }\nsignals.flow = \"norm(smooth(rate(event('*'), 20), 4))\"\nmap.energy = \"flow\"\n```"}
            ],
            "stop_reason": "end_turn", "stop_details": null,
            "usage": {"input_tokens": 120, "output_tokens": 80, "cache_creation_input_tokens": 9000, "cache_read_input_tokens": 0}
        });
        let r = parse_reply(&canned);
        assert_eq!(r.stop_reason, "end_turn");
        assert_eq!(r.usage, Usage { input: 120, output: 80, cache_read: 0, cache_write: 9000 });
        assert!(r.refusal.is_none());
        let toml_src = extract_toml_block(&r.text).unwrap();
        let scene = validate_toml(&toml_src).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(scene.layers.len(), 1);
        // A bad scene produces errors we would send back.
        let errs = validate_toml("[[layer]]\nname = \"a\"\ninstrument = \"nope\"\n").unwrap_err();
        assert!(errs.iter().any(|e| e.contains("unknown instrument")));
        // Cost: 120·4 + 9000·4·1.25 + 80·20 per million.
        let c = cost_estimate("claude-opus-5-5", &r.usage).unwrap();
        assert!((c - (120.0 * 4.0 + 9000.0 * 5.0 + 80.0 * 20.0) * 1e-6).abs() < 1e-12);
        assert!(cost_estimate("claude-unknown", &r.usage).is_none());
    }

    #[test]
    fn refusal_reply_is_detected() {
        let v = serde_json::json!({
            "content": [], "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "cyber", "explanation": "nope"},
            "usage": {"input_tokens": 1, "output_tokens": 0}
        });
        let r = parse_reply(&v);
        assert_eq!(r.stop_reason, "refusal");
        assert_eq!(r.refusal.as_deref(), Some("nope"));
    }

    #[test]
    fn request_body_shape() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let b = request_body("claude-opus-5-5", "SYS", &msgs, true);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["system"][0]["text"], "SYS");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("thinking").is_none(), "thinking is left to the model's adaptive default");
        let b = request_body("claude-haiku-4-5", "SYS", &msgs, false);
        assert!(b.get("fallbacks").is_none());
    }

    #[test]
    fn catalog_json_has_live_registry() {
        let v: serde_json::Value = serde_json::from_str(&catalog_json_native()).unwrap();
        let kinds: Vec<&str> = v["sources"].as_array().unwrap().iter().filter_map(|s| s["kind"].as_str()).collect();
        assert!(kinds.contains(&"synthetic.freeway") && kinds.contains(&"usgs.earthquakes"), "{kinds:?}");
        assert!(v["system_prompt"].as_str().unwrap().contains("usgs.earthquakes"));
    }

    /// Real call. `cargo test -p sonder -- --ignored` with ANTHROPIC_API_KEY set.
    #[test]
    #[ignore]
    fn live_compose_once() {
        if std::env::var("ANTHROPIC_API_KEY").map(|k| k.is_empty()).unwrap_or(true) {
            eprintln!("ANTHROPIC_API_KEY not set; skipping");
            return;
        }
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let out = std::env::temp_dir().join("sonder-live-compose-test.toml");
        rt.block_on(compose(ComposeArgs {
            prompt: "a calm two-layer piece from simulated freeway traffic; one pad, one pluck".into(),
            out: Some(out.clone()),
            model: DEFAULT_COMPOSE_MODEL.into(),
            play: false,
            retries: 2,
            no_fallback: false,
        }))
        .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        validate_toml(&text).unwrap();
    }
}

