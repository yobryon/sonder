//! `sonder radio` — run a scene headless and stream it through ffmpeg to Icecast, an HLS
//! directory or a file. See docs/RADIO.md.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, ValueEnum};
use sonder_engine::{AudioBackendChoice, EngineOptions};
use tokio::io::AsyncWriteExt;

#[derive(Args, Clone)]
pub struct RadioArgs {
    /// Built-in scene name or path to a .toml
    pub scene: String,
    /// Where the stream goes: `icecast://source:PASSWORD@host:8000/mount`, a directory
    /// (HLS: `index.m3u8` + segments + a `radio.html` player) or a file (`show.ogg`).
    #[arg(long, short)]
    pub out: String,
    /// Encoding (HLS always uses AAC segments unless `mp3`).
    #[arg(long, value_enum, default_value_t = Format::Ogg)]
    pub format: Format,
    /// Audio bitrate passed to ffmpeg.
    #[arg(long, default_value = "128k")]
    pub bitrate: String,
    /// Icecast source password, if not already in the URL.
    #[arg(long)]
    pub icecast_password: Option<String>,
    /// Stop after this many seconds (0 = run until Ctrl-C).
    #[arg(long, default_value_t = 0.0)]
    pub seconds: f64,
    /// Stream name shown by Icecast (default: the scene name).
    #[arg(long)]
    pub name: Option<String>,
    /// RNG seed (default: scene's, else random).
    #[arg(long)]
    pub seed: Option<u64>,
    /// Write every ingested sample to this `.sonder` (or `.sonder.gz`) data log, for `sonder replay`.
    #[arg(long, value_name = "PATH")]
    pub record_data: Option<PathBuf>,
    #[command(flatten)]
    pub source: super::SourceOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Ogg,
    Mp3,
}

/// Where the encoded audio goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Icecast(String),
    Hls(PathBuf),
    File(PathBuf),
}

impl Target {
    /// Classify `--out`: an `icecast://` URL (password inserted if missing), a directory
    /// (exists as one, ends with `/`, or has no extension) or a file.
    pub fn parse(out: &str, password: Option<&str>) -> anyhow::Result<Self> {
        if let Some(rest) = out.strip_prefix("icecast://") {
            let url = match (rest.contains('@'), password) {
                (true, _) => out.to_string(),
                (false, Some(pw)) => format!("icecast://source:{pw}@{rest}"),
                (false, None) => anyhow::bail!("icecast URL has no credentials; pass --icecast-password or use icecast://source:PASSWORD@host:port/mount"),
            };
            if !rest.contains('/') {
                anyhow::bail!("icecast URL needs a mount point, e.g. icecast://host:8000/sonder.ogg");
            }
            return Ok(Target::Icecast(url));
        }
        let p = Path::new(out);
        let is_dir = p.is_dir() || out.ends_with('/') || out.ends_with('\\') || p.extension().is_none();
        Ok(if is_dir { Target::Hls(p.to_path_buf()) } else { Target::File(p.to_path_buf()) })
    }

    /// ffmpeg arguments after the input (codec, container, destination).
    pub fn ffmpeg_args(&self, format: Format, bitrate: &str, name: &str) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        let codec = |a: &mut Vec<String>, f: Format| match f {
            Format::Ogg => a.extend(["-c:a", "libvorbis"].map(String::from)),
            Format::Mp3 => a.extend(["-c:a", "libmp3lame"].map(String::from)),
        };
        match self {
            Target::Icecast(url) => {
                codec(&mut a, format);
                a.extend(["-b:a", bitrate].map(String::from));
                let (ct, container) = match format {
                    Format::Ogg => ("audio/ogg", "ogg"),
                    Format::Mp3 => ("audio/mpeg", "mp3"),
                };
                a.extend(["-content_type", ct, "-ice_name", name, "-ice_description", "Sonder: live data as music", "-f", container].map(String::from));
                a.push(url.clone());
            }
            Target::Hls(dir) => {
                match format {
                    Format::Mp3 => codec(&mut a, Format::Mp3),
                    Format::Ogg => a.extend(["-c:a", "aac"].map(String::from)),
                }
                a.extend(["-b:a", bitrate, "-f", "hls", "-hls_time", "6", "-hls_list_size", "10"].map(String::from));
                a.extend(["-hls_flags", "delete_segments+append_list+omit_endlist"].map(String::from));
                a.extend(["-hls_segment_filename", &dir.join("seg%05d.ts").to_string_lossy()].map(String::from));
                a.push(dir.join("index.m3u8").to_string_lossy().into_owned());
            }
            Target::File(path) => {
                codec(&mut a, format);
                a.extend(["-b:a", bitrate, "-f", if format == Format::Ogg { "ogg" } else { "mp3" }].map(String::from));
                a.push(path.to_string_lossy().into_owned());
            }
        }
        a
    }
}

/// Is ffmpeg on PATH?
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

pub fn require_ffmpeg() -> anyhow::Result<()> {
    if ffmpeg_available() {
        Ok(())
    } else {
        anyhow::bail!("ffmpeg not found on PATH; install it (apt install ffmpeg / brew install ffmpeg) to stream or encode")
    }
}

/// The player written next to an HLS stream.
pub const RADIO_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sonder Radio</title>
<style>
:root{color-scheme:dark}body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0b0d12;color:#e6e9f0;font:16px/1.5 system-ui,sans-serif}
main{text-align:center;padding:16px}h1{font-weight:500;letter-spacing:.08em;margin:0 0 .5em}p{color:#8b93a7;margin:.25em 0 1em}audio{width:min(480px,90vw)}
</style></head><body><main>
<h1>SONDER RADIO</h1><p>the world, happening, as music</p>
<audio id="a" controls autoplay></audio>
<script src="https://cdn.jsdelivr.net/npm/hls.js@1/dist/hls.min.js"></script>
<script>
const a=document.getElementById('a'),src='index.m3u8';
if(a.canPlayType('application/vnd.apple.mpegurl')){a.src=src}
else if(window.Hls&&Hls.isSupported()){const h=new Hls({liveSyncDurationCount:3});h.loadSource(src);h.attachMedia(a)}
else{a.replaceWith(Object.assign(document.createElement('p'),{textContent:'This browser cannot play HLS.'}))}
</script></main></body></html>
"#;

pub async fn run(args: RadioArgs) -> anyhow::Result<()> {
    require_ffmpeg()?;
    let target = Target::parse(&args.out, args.icecast_password.as_deref())?;
    if let Target::Hls(dir) = &target {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join("radio.html"), RADIO_HTML)?;
    } else if let Target::File(p) = &target {
        if let Some(parent) = p.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
    }

    let scene = sonder_engine::load_scene(&args.scene)?;
    let name = args.name.clone().unwrap_or_else(|| format!("Sonder: {}", scene.name));
    let opts = EngineOptions {
        backend: AudioBackendChoice::Null,
        sample_rate: 48_000,
        seed: args.seed,
        web_tap: true,
        source_overrides: args.source.clone().into_overrides()?,
        midi_port: None,
        replay: None,
        record_data: args.record_data.clone(),
    };
    let mut handle = sonder_engine::start(scene, opts).await?;
    let mut audio = handle.subscribe_audio().context("engine has no audio tap")?;
    let sr = handle.sample_rate;
    let ff_args = target.ffmpeg_args(args.format, &args.bitrate, &name);
    eprintln!("sonder radio: `{}` → {} ({:?} {})", args.scene, args.out, args.format, args.bitrate);
    if let Target::Hls(dir) = &target {
        eprintln!("sonder radio: player at {}", dir.join("radio.html").display());
    }

    let deadline = (args.seconds > 0.0).then(|| tokio::time::Instant::now() + Duration::from_secs_f64(args.seconds));
    let mut bytes: Vec<u8> = Vec::with_capacity(1 << 16);
    let result: anyhow::Result<()> = 'outer: loop {
        let mut child = spawn_ffmpeg(sr, &ff_args)?;
        let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
        let started = tokio::time::Instant::now();
        // Feed until ffmpeg dies, time is up or we're interrupted.
        let reason = loop {
            tokio::select! {
                chunk = audio.recv() => match chunk {
                    Ok(chunk) => {
                        bytes.clear();
                        for s in chunk.iter() {
                            bytes.extend_from_slice(&s.to_le_bytes());
                        }
                        if let Err(e) = stdin.write_all(&bytes).await {
                            break Feed::Broken(format!("write to ffmpeg failed: {e}"));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!("radio fell behind by {n} chunks"),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break Feed::EngineGone,
                },
                status = child.wait() => break Feed::Broken(format!("ffmpeg exited: {}", status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()))),
                r = &mut handle.done => break match r {
                    Ok(Ok(())) => Feed::EngineGone,
                    Ok(Err(e)) => Feed::Fatal(e),
                    Err(_) => Feed::Fatal(anyhow::anyhow!("engine control thread exited unexpectedly")),
                },
                _ = crate::stop_signal() => break Feed::Done,
                _ = sleep_until_opt(deadline) => break Feed::Done,
            }
        };
        match reason {
            Feed::Done | Feed::EngineGone => {
                drop(stdin);
                let _ = tokio::time::timeout(Duration::from_secs(15), child.wait()).await;
                break 'outer Ok(());
            }
            Feed::Fatal(e) => {
                drop(stdin);
                let _ = child.kill().await;
                break 'outer Err(e);
            }
            Feed::Broken(why) => {
                drop(stdin);
                let _ = child.kill().await;
                if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                    break 'outer Ok(());
                }
                // A quick death right after start usually means bad arguments / unreachable
                // server; back off a little more than for a mid-stream drop.
                let wait = if started.elapsed() < Duration::from_secs(5) { 5 } else { 2 };
                eprintln!("sonder radio: {why}; restarting ffmpeg in {wait}s");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
                    _ = crate::stop_signal() => break 'outer Ok(()),
                }
            }
        }
    };
    handle.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(3), &mut handle.done).await;
    result
}

enum Feed {
    Done,
    EngineGone,
    Broken(String),
    Fatal(anyhow::Error),
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn spawn_ffmpeg(sample_rate: u32, out_args: &[String]) -> anyhow::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "warning", "-nostdin", "-f", "f32le", "-ar", &sample_rate.to_string(), "-ac", "2", "-i", "pipe:0"]);
    cmd.args(out_args);
    cmd.stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).kill_on_drop(true);
    cmd.spawn().context("spawning ffmpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_classify_and_build_ffmpeg_args() {
        assert!(Target::parse("icecast://radio.example:8000/sonder.ogg", None).is_err());
        assert_eq!(
            Target::parse("icecast://radio.example:8000/sonder.ogg", Some("hackme")).unwrap(),
            Target::Icecast("icecast://source:hackme@radio.example:8000/sonder.ogg".into())
        );
        assert_eq!(
            Target::parse("icecast://src:pw@radio.example:8000/m", Some("ignored")).unwrap(),
            Target::Icecast("icecast://src:pw@radio.example:8000/m".into())
        );
        assert_eq!(Target::parse("./radio/", None).unwrap(), Target::Hls(PathBuf::from("./radio/")));
        assert_eq!(Target::parse("radio", None).unwrap(), Target::Hls(PathBuf::from("radio")));
        assert_eq!(Target::parse("out/show.mp3", None).unwrap(), Target::File(PathBuf::from("out/show.mp3")));

        let ice = Target::Icecast("icecast://s:p@h:8000/m".into()).ffmpeg_args(Format::Ogg, "96k", "Sonder");
        assert!(ice.windows(2).any(|w| w == ["-c:a", "libvorbis"]));
        assert!(ice.windows(2).any(|w| w == ["-content_type", "audio/ogg"]));
        assert_eq!(ice.last().unwrap(), "icecast://s:p@h:8000/m");
        let hls = Target::Hls(PathBuf::from("r")).ffmpeg_args(Format::Ogg, "128k", "x");
        assert!(hls.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(hls.windows(2).any(|w| w == ["-f", "hls"]));
        assert!(hls.last().unwrap().ends_with("index.m3u8"));
        let file = Target::File(PathBuf::from("a.mp3")).ffmpeg_args(Format::Mp3, "128k", "x");
        assert!(file.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        assert_eq!(file.last().unwrap(), "a.mp3");
    }
}
