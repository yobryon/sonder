# The DJ and the narrator

Two optional bits of scene intelligence (V3 §6). Both ride on the hot-reload path and the
caption strip; neither changes how a scene sounds.

## DJ — `sonder play --dj [dj.toml]`

A rule engine that watches the **world**, not the playing scene. For every source kind its
rules mention it runs a lightweight *watcher* source of its own (default parameters, or the
rule's `params`), independent of the scene, and evaluates the rules once a second. When a
rule fires and the current scene has held for `min_hold`, the DJ hot-reloads into the
rule's scene (same path as the picker / `LoadScene`), shows a caption (`DJ → Seismic: M5.8
near Tonga`, importance 0.9) and records a `DjSnapshot` (`rule`, `reason`,
`hold_remaining`, `candidate`) for the UIs. After the rule's `hold` with nothing else
firing it returns to `default`.

`--dj` alone uses the embedded rules in [`dj/default.toml`](../dj/default.toml):

```toml
[dj]
min_hold = 600          # a scene plays at least this long before the DJ may switch
cooldown = 1800         # the same rule waits this long before firing again
default = "social-firehose"

[[rule]]
name = "big quake"
when = "usgs.earthquakes: event quake with mag >= 5.5"
scene = "seismic"       # built-in name, or a path (also relative to this file)
hold = 900              # default: min_hold
priority = 10           # highest wins when several fire in the same second
params = { feed = "all_hour", min_magnitude = 4.0 }   # watcher source parameters
```

Rule DSL (two forms):

```
<kind>: event <name> [with <attr> <op> <value>]
<kind>: reading <channel> <op> <value> [for <seconds>]
```

`<op>` is `< <= > >= == !=`; values are numbers or `'quoted text'` (text with `==`/`!=`
only); `intensity` works as an event attribute; `event *` is any event; `for N` means the
reading must have held for N seconds. Recipe kinds work too (`kraken.trades: event trade
with usd > 1000000`).

Behaviour: a rule firing during the hold becomes the `candidate` and switches when the hold
ends (a higher-priority rule replaces it). A rule firing while its own scene plays extends
the hold instead of reloading. `SetDj { enabled }` (TUI `D`, the header pill's on/off in
the web UI) toggles it; picking a scene by hand (picker, editor, `--watch`) pauses the DJ
until it is turned on again. Scene changes count in `EngineHandle::reloads()`.

Code: `crates/sonder-engine/src/dj.rs` (pure engine: parse, `observe`, `tick`,
`snapshot`), wired in `live.rs`.

## Narrator — `sonder play --narrate [seconds]`

Every `interval` (default 180 s, minimum 20) the narrator sends the last few minutes of
captions, events, harmony, form and mood to Claude and shows the one-sentence reply as a
caption prefixed `◎ ` (importance 0.7). It is token-frugal: ≈400 input tokens of context
(`MAX_CONTEXT_CHARS`), `max_tokens` 80, effort `low`, system prompt cached, nothing sent
when nothing happened or the engine is paused. The request shape and default model
(`claude-opus-5-5`) are those of `sonder compose`. Needs `ANTHROPIC_API_KEY`; without it
the narrator logs once and stays off.

Code: `crates/sonder-engine/src/narrator.rs` — `build_prompt`, `request_body`,
`parse_narration`/`tidy` are pure and unit-tested with canned replies; `narrator_live` is an
`#[ignore]`d test against the real API.
