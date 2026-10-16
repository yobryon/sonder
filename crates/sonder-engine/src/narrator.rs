//! The narrator: every few minutes, one sentence of radio-host narration from Claude about
//! what just happened, shown as a caption ("◎ …").
//!
//! Token-frugal by design: the context is the last few minutes of captions, events,
//! harmony and form, cut to [`MAX_CONTEXT_CHARS`] (≈400 tokens); the reply is capped at
//! [`MAX_TOKENS`] and trimmed to [`MAX_CHARS`]. The request shape is the one `sonder
//! compose` and the browser composer use (Messages API, `x-api-key`, system block with a
//! cache breakpoint, server-side refusal fallback on the models that take it). Needs
//! `ANTHROPIC_API_KEY`; without it the task logs once and stops.

use std::time::Duration;

use serde_json::{json, Value as Json};
use sonder_core::{EngineSnapshot, Seconds};

/// Seconds between narrations unless `--narrate <seconds>` says otherwise.
pub const DEFAULT_INTERVAL: f64 = 180.0;
/// Shortest interval we accept (the API is not free).
pub const MIN_INTERVAL: f64 = 20.0;
/// The model, same default as `sonder compose`.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// Context budget in characters (~4 chars per token → ~400 tokens).
pub const MAX_CONTEXT_CHARS: usize = 1600;
/// Output cap.
pub const MAX_TOKENS: u32 = 80;
/// A narration is at most this long.
pub const MAX_CHARS: usize = 140;
/// Caption importance.
pub const IMPORTANCE: f64 = 0.7;
/// Caption prefix.
pub const PREFIX: &str = "◎ ";

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Models that accept the server-side refusal fallback (`fallbacks: "default"`).
const FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-fable-5-1"];

pub const SYSTEM_PROMPT: &str = "You are the late-night host of Sonder, a radio station that turns live world data into generative music. \
You get a short log of what just happened (captions, events, the key and chord, the form section, the mood) and reply with ONE sentence of on-air narration: \
present tense, specific (name the place, the number, the thing), warm, at most 140 characters. \
No hashtags, no emoji, no quotation marks, no preamble, no second sentence — just the line.";

#[derive(Debug, Clone)]
pub struct NarratorOptions {
    pub interval: f64,
    pub model: String,
}

impl Default for NarratorOptions {
    fn default() -> Self {
        Self { interval: DEFAULT_INTERVAL, model: DEFAULT_MODEL.into() }
    }
}

/// `ANTHROPIC_API_KEY`, if set and non-blank.
pub fn api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

fn ago(now: Seconds, at: Seconds) -> String {
    let d = (now - at).max(0.0);
    if d < 60.0 {
        format!("{}s ago", d.round() as u64)
    } else {
        format!("{}m ago", (d / 60.0).floor() as u64)
    }
}

/// The user message: what happened since `since` (engine seconds), newest last, cut to
/// [`MAX_CONTEXT_CHARS`] by dropping the oldest lines first. `None` when nothing new
/// happened (no captions or events since `since`), so no request is made.
pub fn build_prompt(snap: &EngineSnapshot, since: Seconds) -> Option<String> {
    let now = snap.time;
    let mut captions: Vec<String> = snap
        .captions
        .iter()
        .filter(|c| c.at > since && !c.text.is_empty() && !c.text.starts_with(PREFIX))
        .map(|c| format!("- {}: {}", ago(now, c.at), c.text))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut events: Vec<String> = snap
        .events
        .iter()
        .filter(|e| e.at > since)
        .filter_map(|e| {
            let label = if e.label.is_empty() { e.name.clone() } else { e.label.clone() };
            seen.insert(label.clone()).then_some(format!("- {}: {}", ago(now, e.at), label))
        })
        .collect();
    if captions.is_empty() && events.is_empty() {
        return None;
    }
    captions.reverse();
    events.reverse();
    let head = {
        let h = &snap.harmony;
        let f = &snap.form;
        let mut s = format!("Scene: {}\nMusic: {} {}, chord {}, {} bpm", snap.scene_name, h.key_name, h.mode, h.chord_name, snap.transport.bpm.round());
        if !f.section.is_empty() {
            s.push_str(&format!(", section {}", f.section));
        }
        if !f.note.is_empty() {
            s.push_str(&format!(" ({})", f.note));
        }
        s.push_str(&format!(
            "\nMood: energy {:.1}, tension {:.1}, brightness {:.1}",
            snap.mood.energy, snap.mood.tension, snap.mood.brightness
        ));
        s
    };
    let footer = "\nOne sentence of narration:";
    let budget = MAX_CONTEXT_CHARS.saturating_sub(head.chars().count() + footer.chars().count() + 40);
    // Newest first, alternating captions and events so both are represented, until the
    // budget is spent; then restore chronological order.
    let mut picked_c: Vec<String> = Vec::new();
    let mut picked_e: Vec<String> = Vec::new();
    let mut used = 0usize;
    let (mut ci, mut ei) = (0usize, 0usize);
    loop {
        let mut progressed = false;
        if let Some(l) = captions.get(ci) {
            let n = l.chars().count() + 1;
            if used + n <= budget {
                picked_c.push(l.clone());
                used += n;
                ci += 1;
                progressed = true;
            } else {
                ci = captions.len();
            }
        }
        if let Some(l) = events.get(ei) {
            let n = l.chars().count() + 1;
            if used + n <= budget {
                picked_e.push(l.clone());
                used += n;
                ei += 1;
                progressed = true;
            } else {
                ei = events.len();
            }
        }
        if !progressed || (ci >= captions.len() && ei >= events.len()) {
            break;
        }
    }
    picked_c.reverse();
    picked_e.reverse();
    let mut out = head;
    if !picked_c.is_empty() {
        out.push_str("\nCaptions (oldest first):\n");
        out.push_str(&picked_c.join("\n"));
    }
    if !picked_e.is_empty() {
        out.push_str("\nEvents (oldest first):\n");
        out.push_str(&picked_e.join("\n"));
    }
    out.push_str(footer);
    Some(out)
}

/// The Messages API request body (same shape as `sonder compose`, smaller caps). Effort
/// `low` keeps the reply quick and cheap on the models that think by default.
pub fn request_body(model: &str, prompt: &str) -> Json {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "output_config": { "effort": "low" },
        "system": [{ "type": "text", "text": SYSTEM_PROMPT, "cache_control": { "type": "ephemeral" } }],
        "messages": [{ "role": "user", "content": prompt }],
    });
    if uses_fallback(model) {
        body["fallbacks"] = Json::String("default".into());
    }
    body
}

fn uses_fallback(model: &str) -> bool {
    FALLBACK_MODELS.contains(&model)
}

/// Turn a Messages API reply into the caption text (with [`PREFIX`]), or say why not.
pub fn parse_narration(v: &Json) -> Result<String, String> {
    if let Some(err) = v.get("error") {
        return Err(format!("{}: {}", err["type"].as_str().unwrap_or("error"), err["message"].as_str().unwrap_or("(no message)")));
    }
    let stop = v["stop_reason"].as_str().unwrap_or("");
    if stop == "refusal" {
        return Err(format!("the model declined{}", v["stop_details"]["explanation"].as_str().map(|e| format!(": {e}")).unwrap_or_default()));
    }
    let text = v["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    tidy(&text).map(|t| format!("{PREFIX}{t}")).ok_or_else(|| if stop == "max_tokens" { "empty reply (max_tokens)".to_string() } else { "empty reply".to_string() })
}

/// One clean line: first non-empty line, quotes and hashtags stripped, whitespace
/// collapsed, cut at a word boundary to [`MAX_CHARS`].
pub fn tidy(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_matches(|c: char| c == '"' || c == '“' || c == '”' || c == '\'' || c == '«' || c == '»');
    let words: Vec<&str> = line.split_whitespace().filter(|w| !(w.starts_with('#') && w.len() > 1)).collect();
    let mut out = String::new();
    for w in words {
        let candidate_len = out.chars().count() + if out.is_empty() { 0 } else { 1 } + w.chars().count();
        if candidate_len > MAX_CHARS - 1 && !out.is_empty() {
            out.push('…');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    let out = out.trim_end_matches([',', ';', ':', '-']).to_string();
    if out.chars().count() > MAX_CHARS {
        let cut: String = out.chars().take(MAX_CHARS - 1).collect();
        return Some(format!("{}…", cut.trim_end()));
    }
    (!out.is_empty()).then_some(out)
}

/// Ask once. Returns the caption text.
pub async fn narrate_once(client: &reqwest::Client, key: &str, model: &str, prompt: &str) -> anyhow::Result<String> {
    let body = request_body(model, prompt);
    let mut req = client
        .post(MESSAGES_URL)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body);
    if uses_fallback(model) {
        req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let resp = req.send().await.map_err(|e| anyhow::anyhow!("calling the Claude API: {e}"))?;
    let status = resp.status();
    let v: Json = resp.json().await.map_err(|e| anyhow::anyhow!("reading the Claude API response: {e}"))?;
    if !status.is_success() && v.get("error").is_none() {
        anyhow::bail!("Claude API {status}");
    }
    let usage = &v["usage"];
    tracing::debug!(
        input = usage["input_tokens"].as_u64().unwrap_or(0),
        cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
        output = usage["output_tokens"].as_u64().unwrap_or(0),
        "narrator tokens"
    );
    parse_narration(&v).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The narrator task: every `opts.interval` seconds, narrate what the snapshot shows
/// since the last narration and send `(text, importance)` down `captions`. Stops when
/// the key is missing (logged once), on shutdown, or when the engine goes away.
pub async fn run(
    opts: NarratorOptions,
    snapshot: crate::SnapshotRx,
    captions: tokio::sync::mpsc::Sender<(String, f64)>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let Some(key) = api_key() else {
        tracing::warn!("narrator: ANTHROPIC_API_KEY is not set — narration is off");
        return;
    };
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(60)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("narrator: http client: {e}");
            return;
        }
    };
    let interval = Duration::from_secs_f64(opts.interval.max(MIN_INTERVAL));
    let mut since = snapshot.borrow().time;
    tracing::info!(model = %opts.model, every = ?interval, "narrator on");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
        }
        let snap = snapshot.borrow().clone();
        if snap.paused {
            continue;
        }
        let Some(prompt) = build_prompt(&snap, since) else {
            tracing::debug!("narrator: nothing new to narrate");
            continue;
        };
        since = snap.time;
        match narrate_once(&client, &key, &opts.model, &prompt).await {
            Ok(line) => {
                tracing::info!(%line, "narrator");
                if captions.send((line, IMPORTANCE)).await.is_err() {
                    return;
                }
            }
            Err(e) => tracing::warn!("narrator: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{Caption, EventSnapshot, LayerId};

    fn snap() -> EngineSnapshot {
        let mut s = EngineSnapshot { scene_name: "Seismic".into(), time: 400.0, ..Default::default() };
        s.harmony.key_name = "D".into();
        s.harmony.mode = "phrygian".into();
        s.harmony.chord_name = "Dm".into();
        s.transport.bpm = 56.4;
        s.form.section = "build".into();
        s.mood.energy = 0.42;
        s.captions = vec![
            Caption { at: 100.0, layer: None, text: "old news".into(), importance: 0.5 },
            Caption { at: 250.0, layer: Some(LayerId(2)), text: "M5.8 near Tonga".into(), importance: 0.9 },
            Caption { at: 300.0, layer: None, text: "◎ an earlier narration".into(), importance: 0.7 },
            Caption { at: 390.0, layer: None, text: "now in D phrygian".into(), importance: 0.5 },
        ];
        s.events = vec![
            EventSnapshot { at: 260.0, layer: LayerId(1), name: "onset".into(), intensity: 0.3, attrs: Default::default(), label: "sound onset".into() },
            EventSnapshot { at: 261.0, layer: LayerId(1), name: "onset".into(), intensity: 0.3, attrs: Default::default(), label: "sound onset".into() },
            EventSnapshot { at: 262.0, layer: LayerId(1), name: "tremor".into(), intensity: 0.7, attrs: Default::default(), label: String::new() },
        ];
        s
    }

    #[test]
    fn prompt_has_music_state_and_only_new_items_oldest_first() {
        let p = build_prompt(&snap(), 200.0).unwrap();
        assert!(p.starts_with("Scene: Seismic\nMusic: D phrygian, chord Dm, 56 bpm, section build\nMood: energy 0.4"), "{p}");
        assert!(!p.contains("old news"), "captions before `since` are out");
        assert!(!p.contains("earlier narration"), "our own lines are not fed back");
        let tonga = p.find("M5.8 near Tonga").unwrap();
        let key = p.find("now in D phrygian").unwrap();
        assert!(tonga < key, "oldest first");
        assert!(p.contains("- 2m ago: M5.8 near Tonga"), "{p}");
        assert!(p.contains("- 10s ago: now in D phrygian"), "{p}");
        assert_eq!(p.matches("sound onset").count(), 1, "events are deduped by label");
        assert!(p.contains("- 2m ago: tremor"), "unlabelled events use their name");
        assert!(p.trim_end().ends_with("One sentence of narration:"));
        assert!(p.chars().count() <= MAX_CONTEXT_CHARS);
    }

    #[test]
    fn prompt_is_none_when_nothing_happened_and_is_cut_to_budget() {
        assert!(build_prompt(&snap(), 395.0).is_none());
        let mut s = snap();
        s.captions = (0..200).map(|i| Caption { at: 300.0 + i as f64 * 0.1, layer: None, text: format!("caption number {i} with some words in it"), importance: 0.5 }).collect();
        s.events = (0..200).map(|i| EventSnapshot { at: 300.0 + i as f64 * 0.1, layer: LayerId(0), name: "e".into(), intensity: 0.1, attrs: Default::default(), label: format!("event number {i}") }).collect();
        let p = build_prompt(&s, 0.0).unwrap();
        assert!(p.chars().count() <= MAX_CONTEXT_CHARS, "{}", p.len());
        assert!(p.contains("caption number 199") && p.contains("event number 199"), "newest kept");
        assert!(!p.contains("caption number 0 "), "oldest dropped first");
    }

    #[test]
    fn request_body_is_small_and_matches_the_compose_shape() {
        let b = request_body("claude-opus-5-5", "hi");
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["max_tokens"], MAX_TOKENS);
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"], "hi");
        assert_eq!(b["fallbacks"], "default");
        assert!(request_body("claude-haiku-4-5", "hi").get("fallbacks").is_none());
    }

    #[test]
    fn canned_replies_parse() {
        let ok = json!({ "content": [{ "type": "text", "text": "\"A 5.8 rolls out of the Tonga trench; the drone drops a fifth. #quake\"\nSecond line ignored." }], "stop_reason": "end_turn", "usage": { "input_tokens": 300, "output_tokens": 30 } });
        assert_eq!(parse_narration(&ok).unwrap(), "◎ A 5.8 rolls out of the Tonga trench; the drone drops a fifth.");
        let long = json!({ "content": [{ "type": "text", "text": "word ".repeat(60) }], "stop_reason": "max_tokens" });
        let t = parse_narration(&long).unwrap();
        assert!(t.chars().count() <= MAX_CHARS + PREFIX.chars().count(), "{t}");
        assert!(t.ends_with('…'));
        let empty = json!({ "content": [], "stop_reason": "max_tokens" });
        assert_eq!(parse_narration(&empty).unwrap_err(), "empty reply (max_tokens)");
        let refused = json!({ "content": [], "stop_reason": "refusal", "stop_details": { "type": "refusal", "explanation": "nope" } });
        assert_eq!(parse_narration(&refused).unwrap_err(), "the model declined: nope");
        let err = json!({ "type": "error", "error": { "type": "authentication_error", "message": "invalid x-api-key" } });
        assert_eq!(parse_narration(&err).unwrap_err(), "authentication_error: invalid x-api-key");
        let thinking_only = json!({ "content": [{ "type": "thinking", "thinking": "" }, { "type": "text", "text": "  Lightning stacks up over the Gulf tonight.  " }], "stop_reason": "end_turn" });
        assert_eq!(parse_narration(&thinking_only).unwrap(), "◎ Lightning stacks up over the Gulf tonight.");
    }

    #[test]
    fn tidy_strips_hashtags_quotes_and_cuts_on_words() {
        assert_eq!(tidy("  “Quiet night over the Pacific.”  ").unwrap(), "Quiet night over the Pacific.");
        assert_eq!(tidy("Storm #lightning rolls in, #storm").unwrap(), "Storm rolls in");
        assert_eq!(tidy("No. 1 hit").unwrap(), "No. 1 hit");
        assert_eq!(tidy("\n\n"), None);
        let cut = tidy(&"abcdefghij ".repeat(20)).unwrap();
        assert!(cut.chars().count() <= MAX_CHARS, "{}", cut.chars().count());
        assert!(cut.ends_with("abcdefghij…"), "{cut}");
        let one_word = tidy(&"x".repeat(300)).unwrap();
        assert_eq!(one_word.chars().count(), MAX_CHARS);
    }

    /// Hits the real API. `cargo test -p sonder-engine -- --ignored narrator_live` with
    /// `ANTHROPIC_API_KEY` set.
    #[tokio::test]
    #[ignore]
    async fn narrator_live() {
        let Some(key) = api_key() else {
            eprintln!("ANTHROPIC_API_KEY not set; skipping");
            return;
        };
        let client = reqwest::Client::new();
        let prompt = build_prompt(&snap(), 200.0).unwrap();
        let line = narrate_once(&client, &key, DEFAULT_MODEL, &prompt).await.unwrap();
        eprintln!("{line}");
        assert!(line.starts_with(PREFIX));
        assert!(line.chars().count() <= MAX_CHARS + PREFIX.chars().count());
    }
}
