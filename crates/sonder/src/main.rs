//! `sonder` — listen to the world happening.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use sonder_core::{Scene, SourceSpec};
use sonder_engine::{AudioBackendChoice, EngineOptions};

#[derive(Parser)]
#[command(name = "sonder", version, about = "A realtime generative music engine that listens to the world.", long_about = None)]
struct Cli {
    /// Log filter (e.g. `info`, `sonder_sources=debug`). Logs go to stderr (or a file with --log-file).
    #[arg(long, global = true, default_value = "warn")]
    log: String,
    /// Write logs to this file instead of stderr (recommended with the TUI).
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
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
    #[command(flatten)]
    source: SourceOverride,
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
    init_logging(&cli.log, cli.log_file.as_deref(), matches!(cli.cmd, Cmd::Play(_)))?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match cli.cmd {
        Cmd::Play(args) => rt.block_on(play(args, true)),
        Cmd::Serve(args) => rt.block_on(play(args, false)),
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

async fn play(args: PlayArgs, tui: bool) -> anyhow::Result<()> {
    let scene = sonder_engine::load_scene(&args.scene)?;
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
    };
    let handle = sonder_engine::start(scene, opts).await?;
    if let Some(p) = &handle.midi_port {
        tracing::info!("midi out → {p}");
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
            _ = tokio::signal::ctrl_c() => {
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
