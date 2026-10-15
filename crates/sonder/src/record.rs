//! `sonder record` — render a dated, deterministic track (seed = the date) as WAV (+ MP3
//! when ffmpeg is around), write `<date>.json` with the day's captions and stats, and
//! rebuild the `index.html` album page listing every record in the directory.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};

#[derive(Args, Clone)]
pub struct RecordArgs {
    /// Built-in scene name or path to a .toml
    #[arg(long, default_value = "night-freeway")]
    pub scene: String,
    /// Length of the track.
    #[arg(long, default_value_t = 10.0)]
    pub minutes: f64,
    /// Album directory (one `<date>.json` + audio per day, plus `index.html`).
    #[arg(long, short, default_value = "site/records")]
    pub out: PathBuf,
    /// Date to record as, YYYY-MM-DD (default: today, UTC). Also the seed (YYYYMMDD).
    #[arg(long)]
    pub date: Option<String>,
    /// Extra files already in the album directory to list under this day (e.g. a live
    /// capture from `sonder radio --out <dir>/<date>-live.mp3`). Repeatable.
    #[arg(long = "attach", value_name = "FILE")]
    pub attachments: Vec<PathBuf>,
    /// Delete the WAV once the MP3 exists (keeps the album small).
    #[arg(long)]
    pub no_wav: bool,
    /// Only rebuild index.html from the JSON files already there.
    #[arg(long)]
    pub index_only: bool,
}

/// One day's record, as written to `<date>.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Record {
    pub date: String,
    pub scene: String,
    pub scene_name: String,
    pub description: String,
    pub seed: u64,
    pub minutes: f64,
    /// Audio files relative to the album directory.
    #[serde(default)]
    pub files: Vec<Attachment>,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub captions: Vec<CaptionOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Attachment {
    pub file: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Stats {
    pub peak: f32,
    pub rms: f32,
    pub notes: usize,
    pub events: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptionOut {
    pub at: f64,
    pub text: String,
    pub importance: f64,
}

/// Today's date (UTC) as YYYY-MM-DD.
pub fn today_utc() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-10-15` → 20261015 (the day's seed).
pub fn seed_for(date: &str) -> anyhow::Result<u64> {
    let digits: String = date.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 8 {
        anyhow::bail!("--date must be YYYY-MM-DD, got `{date}`");
    }
    Ok(digits.parse()?)
}

pub fn run(args: RecordArgs) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out).with_context(|| format!("creating {}", args.out.display()))?;
    if !args.index_only {
        let date = args.date.clone().unwrap_or_else(today_utc);
        let seed = seed_for(&date)?;
        let scene = sonder_engine::load_scene(&args.scene)?;
        let seconds = (args.minutes * 60.0).max(1.0);
        let wav = args.out.join(format!("{date}.wav"));
        eprintln!("sonder record: {date} — {:.1} min of `{}` (seed {seed}) → {}", args.minutes, scene.name, wav.display());
        let stats = sonder_engine::render_to_wav(&scene, seconds, seed, &wav)?;
        let mut files = Vec::new();
        let mp3 = args.out.join(format!("{date}.mp3"));
        if super::radio::ffmpeg_available() {
            let status = std::process::Command::new("ffmpeg")
                .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(&wav)
                .args(["-c:a", "libmp3lame", "-b:a", "160k"])
                .arg(&mp3)
                .status()
                .context("running ffmpeg")?;
            if status.success() {
                files.push(Attachment { file: format!("{date}.mp3"), label: format!("{} (offline render)", scene.name) });
                if args.no_wav {
                    let _ = std::fs::remove_file(&wav);
                }
            } else {
                eprintln!("sonder record: ffmpeg failed ({status}); keeping the WAV only");
            }
        } else {
            eprintln!("sonder record: ffmpeg not found; WAV only");
        }
        if wav.exists() {
            files.push(Attachment { file: format!("{date}.wav"), label: format!("{} (WAV)", scene.name) });
        }
        for a in &args.attachments {
            let name = a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !args.out.join(&name).exists() {
                eprintln!("sonder record: attachment {} is not in {}; skipping", a.display(), args.out.display());
                continue;
            }
            let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&name);
            let label = stem.strip_prefix(&format!("{date}-")).unwrap_or(stem).replace(['-', '_'], " ");
            files.push(Attachment { file: name, label });
        }
        let record = Record {
            date: date.clone(),
            scene: args.scene.clone(),
            scene_name: scene.name.clone(),
            description: scene.description.clone(),
            seed,
            minutes: args.minutes,
            files,
            stats: Stats { peak: stats.peak, rms: stats.rms, notes: stats.notes, events: stats.events },
            captions: stats.captions.iter().map(|c| CaptionOut { at: c.at, text: c.text.clone(), importance: c.importance }).collect(),
        };
        std::fs::write(args.out.join(format!("{date}.json")), serde_json::to_string_pretty(&record)?)?;
        eprintln!("sonder record: {} notes, {} events, {} captions", record.stats.notes, record.stats.events, record.captions.len());
    }
    let records = load_records(&args.out)?;
    std::fs::write(args.out.join("index.html"), index_html(&records))?;
    eprintln!("sonder record: index.html lists {} record(s)", records.len());
    Ok(())
}

/// Every `<date>.json` in `dir`, newest first.
pub fn load_records(dir: &Path) -> anyhow::Result<Vec<Record>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = entry?.path();
        if p.extension().is_some_and(|e| e == "json") {
            match std::fs::read_to_string(&p).map_err(anyhow::Error::from).and_then(|t| Ok(serde_json::from_str::<Record>(&t)?)) {
                Ok(r) if !r.date.is_empty() => out.push(r),
                Ok(_) => {}
                Err(e) => eprintln!("sonder record: skipping {}: {e}", p.display()),
            }
        }
    }
    out.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(out)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn mmss(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{}:{:02}", t / 60, t % 60)
}

/// The album page: vanilla HTML, dark.
pub fn index_html(records: &[Record]) -> String {
    let mut body = String::new();
    if records.is_empty() {
        body.push_str("<p class=\"muted\">No records yet.</p>");
    }
    for r in records {
        let mut players = String::new();
        for f in &r.files {
            let mime = if f.file.ends_with(".mp3") { "audio/mpeg" } else if f.file.ends_with(".ogg") { "audio/ogg" } else { "audio/wav" };
            players.push_str(&format!(
                "<figure><figcaption>{}</figcaption><audio controls preload=\"none\"><source src=\"{}\" type=\"{mime}\"></audio> <a href=\"{}\" download>download</a></figure>",
                esc(&f.label),
                esc(&f.file),
                esc(&f.file)
            ));
        }
        // The dozen most important captions, in time order.
        let mut top: Vec<&CaptionOut> = r.captions.iter().collect();
        top.sort_by(|a, b| b.importance.partial_cmp(&a.importance).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(12);
        top.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
        let caps: String = top
            .iter()
            .map(|c| format!("<li><time>{}</time> {}</li>", mmss(c.at), esc(&c.text)))
            .collect();
        body.push_str(&format!(
            "<article id=\"{d}\"><header><h2><a href=\"#{d}\">{d}</a></h2><p>{name} · {min:.0} min · seed {seed} · {notes} notes · {events} events</p><p class=\"muted\">{desc}</p></header>{players}{caps}</article>\n",
            d = esc(&r.date),
            name = esc(&r.scene_name),
            min = r.minutes,
            seed = r.seed,
            notes = r.stats.notes,
            events = r.stats.events,
            desc = esc(&r.description),
            caps = if caps.is_empty() { String::new() } else { format!("<ol class=\"captions\">{caps}</ol>") },
        ));
    }
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Sonder — Records</title>
<style>
:root{{color-scheme:dark;--bg:#0b0d12;--fg:#e6e9f0;--muted:#8b93a7;--line:#1d2230;--accent:#8fd3ff}}
body{{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,sans-serif}}
main{{max-width:760px;margin:0 auto;padding:32px 16px 64px}}
h1{{font-weight:500;letter-spacing:.12em;margin:0}}.sub{{color:var(--muted);margin:.25em 0 2em}}
article{{border-top:1px solid var(--line);padding:24px 0}}h2{{font-weight:500;margin:0;font-size:1.25rem}}h2 a{{color:var(--fg);text-decoration:none}}
header p{{margin:.2em 0;color:var(--muted)}}.muted{{color:var(--muted)}}
figure{{margin:12px 0}}figcaption{{font-size:.9rem;color:var(--muted);margin-bottom:4px}}audio{{width:100%;max-width:520px;vertical-align:middle}}
figure a{{font-size:.85rem;color:var(--accent);margin-left:8px}}
.captions{{list-style:none;padding:0;margin:12px 0 0;columns:2;column-gap:24px;font-size:.92rem}}
.captions li{{break-inside:avoid;padding:2px 0}}.captions time{{color:var(--accent);font-variant-numeric:tabular-nums;margin-right:.5em}}
@media (max-width:560px){{.captions{{columns:1}}}}
</style></head><body><main>
<h1>SONDER</h1><p class="sub">one record a day — a deterministic render seeded by the date, and a live capture — <a href="../" style="color:var(--accent)">listen live</a></p>
{body}</main></body></html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_seeds() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_741), (2026, 10, 15));
        assert_eq!(seed_for("2026-10-15").unwrap(), 20_261_015);
        assert!(seed_for("yesterday").is_err());
        assert_eq!(today_utc().len(), 10);
    }

    #[test]
    fn index_lists_records_newest_first_and_escapes() {
        let recs = vec![
            Record { date: "2026-01-02".into(), scene_name: "A <b>".into(), files: vec![Attachment { file: "2026-01-02.mp3".into(), label: "x".into() }], ..Default::default() },
            Record { date: "2026-01-03".into(), scene_name: "B".into(), captions: vec![CaptionOut { at: 65.0, text: "M4.1 off Honshu".into(), importance: 0.9 }], ..Default::default() },
        ];
        let html = index_html(&recs);
        assert!(html.find("2026-01-03").unwrap() < html.find("2026-01-02").unwrap() || recs[0].date < recs[1].date);
        assert!(html.contains("A &lt;b&gt;"));
        assert!(html.contains("<time>1:05</time> M4.1 off Honshu"));
        assert!(html.contains("src=\"2026-01-02.mp3\" type=\"audio/mpeg\""));
    }

    #[test]
    fn record_renders_json_and_index() {
        let dir = std::env::temp_dir().join(format!("sonder-record-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let args = RecordArgs {
            scene: "night-freeway".into(),
            minutes: 0.05,
            out: dir.clone(),
            date: Some("2026-10-15".into()),
            attachments: vec![],
            no_wav: false,
            index_only: false,
        };
        run(args).unwrap();
        let rec: Record = serde_json::from_str(&std::fs::read_to_string(dir.join("2026-10-15.json")).unwrap()).unwrap();
        assert_eq!(rec.seed, 20_261_015);
        assert_eq!(rec.scene_name, "Night Freeway");
        assert!(dir.join("2026-10-15.wav").exists());
        assert!(rec.files.iter().any(|f| f.file.ends_with(".wav")));
        let index = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(index.contains("2026-10-15"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
