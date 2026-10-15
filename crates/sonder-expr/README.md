# sonder-expr — the Sonder feature language

A tiny expression language that turns raw source data (readings and events) into
signals in 0..1 and into musical parameters. Expressions live in a scene's TOML:

```toml
[[layer]]
signals.density = "norm(smooth(rate(event('car'), 10s), 3s))"
signals.speed   = "norm(smooth(mean_speed, 4s))"
signals.spike   = "onset(density, 0.8)"          # becomes an event named "spike"
map.energy      = "density"
map.register    = "0.3 + 0.4 * speed"

on_event.car.pitch    = "attr('lane') / 4"
on_event.car.velocity = "0.4 + 0.6 * intensity"
on_event.car.chance   = "if(attr_text('kind') == 'truck', 1, 0.5)"
```

Everything is evaluated at the engine's control rate (50 Hz, `dt` ≈ 20 ms). Each
call-site of a *stateful* function (`smooth`, `rate`, `norm`, …) keeps its own state,
so `smooth(x, 2) - smooth(x, 10)` is two independent filters.

## Grammar

```
expr     := or
or       := and ( "||" and )*
and      := cmp ( "&&" cmp )*
cmp      := add ( ("<" | "<=" | ">" | ">=" | "==" | "!=") add )?
add      := mul ( ("+" | "-") mul )*
mul      := unary ( ("*" | "/" | "%") unary )*
unary    := ("-" | "!") unary | pow
pow      := atom ( "^" unary )?
atom     := number | string | ident | ident "(" args ")" | "(" expr ")"
number   := 1.5 | 2 | 10s | 250ms | 2m | 1h      -- unit suffixes convert to seconds
string   := 'single' | "double"
```

- Comparisons yield `1.0` / `0.0`. `&&`, `||`, `!` treat any non-zero value as true.
  Comparisons do not chain: write `a < b && b < c`.
- `^` is right-associative and binds tighter than unary minus (`-2^2 = -4`).
- `%` is a wrapping modulo (`-1 % 3 = 2`). Division or modulo by zero yields `0`.
  Any non-finite intermediate becomes `0` so the music never gets a NaN.
- `#` starts a comment to end of line.
- Identifiers may contain `.` (`mean.speed`), which is handy for JSON-path channels.

### Identifiers

A bare name resolves, in order, to:

1. a **signal** of the same layer (any other signal, as long as there is no cycle;
   evaluation order is derived from the references),
2. a **constant**: `time` (seconds since start), `dt`, `beat`, `bar`, `bpm`, `pi`,
   `tau`, `e`, and `intensity` inside `on_event`.

Unknown names are a compile error listing the known names (with a "did you mean" hint).
3. a **source channel** — shorthand for `field("name")`. A channel that collides with a
   constant name (`e`, `time`, …) must be written as `field("e")`.

When the engine knows the source's channels it restricts bare names to them.

### Strings

String literals are only valid as arguments to `field`, `event`, `attr`, `attr_text`,
`has`, `hash`, and in `attr_text("k") == "..."` / `!=` comparisons (which yield 1/0).

## Builtins

### Sources of truth

| Function | Meaning |
|---|---|
| `field("name")` / bare `name` | Latest reading of a channel. Holds its last value; `0` before the first reading. |
| `event("name")` | Impulse train: on a tick, the **sum of intensities** of matching events received since the previous tick, else `0`. `"*"` matches any event, `"blob*"` is a prefix match. |
| `intensity` | (on_event only) the event's intensity. |
| `attr("k")`, `attr("k", default)` | (on_event only) attribute as a number: numbers as-is, booleans 0/1, text parsed as a number if it parses, otherwise hashed to a stable 0..1. Missing → `default` (0). |
| `attr_text("k") == "lit"` | (on_event only) string equality → 1/0. Also `!=`, and `hash(attr_text("k"))`. |
| `has("k")` | (on_event only) 1 if the attribute is present. |
| `time`, `dt`, `beat`, `bar`, `bpm` | Clock and transport. |

### Pure functions

`abs(x)`, `min(a, b, …)`, `max(a, b, …)`, `clamp(x, lo, hi)`, `lerp(a, b, t)`,
`pow(x, y)`, `sqrt`, `exp`, `log`, `log2`, `log10`, `floor`, `ceil`, `round`, `fract`,
`sign`, `step(edge, x)` (1 when `x >= edge`), `smoothstep(e0, e1, x)`, `if(cond, a, b)`,
`sin`, `cos`, `tan`, `tanh`, `wrap(x)` (`x - floor(x)`), `norm(x, lo, hi)` (fixed-range
normalization, clamped), `hash(x)` (stable 0..1 from a string or a number — FNV-1a, same
across runs and machines).

`if` evaluates both branches so stateful nodes inside keep ticking.

### Stateful functions (per call-site state)

All time arguments are seconds (`250ms`, `10s`, `2m` work). Window sizes must be
constants; everything else may be an expression.

| Function | Semantics |
|---|---|
| `smooth(x, seconds)` | One-pole low-pass with time constant `seconds`, dt-aware. First tick snaps to `x`. `smooth(x, 0)` passes through. |
| `ema(x, alpha)` | Exponential moving average with per-tick weight `alpha` (0..1). |
| `derivative(x)` | Change per second, averaged over the last two ticks. `0` on the first tick. |
| `norm(x)` | **Adaptive normalization.** Tracks a running min and max that decay toward the current value with a ~2 minute half-life, and maps `x` to 0..1 within them. Outputs `0.5` while the range is (nearly) empty, including the very first tick. This is what makes any feed "loud for itself". |
| `rate(events, window)` | Events per second over a sliding window — counts **events**, not intensity (`event("car")` with 3 cars in a tick counts 3; any other expression counts 1 per non-zero tick). |
| `count(events, window)` | Number of events in the window. |
| `sum(x, window)` | Sliding-window sum of values (impulse intensities). |
| `lag(x, seconds)` | `x` delayed by `seconds` (holds the first value until the delay fills). |
| `hold(x, seconds)` | Peak hold: `max(x, previous · e^(−dt/seconds))` — jumps up instantly, decays exponentially with time constant `seconds`. |
| `maxw(x, window)` / `minw(x, window)` | Sliding-window maximum / minimum. |
| `onset(x, threshold)` | **Edge:** 1 on the tick `x` crosses up through `threshold`, else 0 (never on the first tick). |
| `every(seconds)` | **Edge:** 1 on the first tick and then once every `seconds`. |
| `change(x)`, `change(x, eps)` | **Edge:** 1 when `x` differs from the previous tick by more than `eps` (default 0). |
| `random()` | Uniform 0..1, new value every tick. Seeded from the scene seed and the call-site, so renders are reproducible. |
| `walk(step, seconds)` | Seeded random walk in 0..1: every `seconds` picks a new target within ±`step` (reflecting at the edges) and glides to it linearly. Starts at 0.5. |

Windows are stored as one entry per tick (at most 65 536 entries ≈ 21 minutes at
50 Hz), so memory is bounded; the window is measured in ticks using the first tick's `dt`.

### Edge detectors and events

A **signal whose top-level node** is `onset`, `every` or `change` fires events. On each
tick it fires, the engine emits a layer event named after the signal, so

```toml
signals.spike = "onset(norm(cpu), 0.8)"
on_event.spike.pitch = "0.9"
```

plays a note whenever CPU spikes relative to its own recent history. `onset(x, 0.5) * 2`
is *not* an edge signal (the top-level node is `*`).

## on_event expressions

Evaluated once per event, stateless. They may use `intensity`, `attr`, `attr_text`,
`has`, `time`, `beat`, every pure function, and the layer's signals by name (their value
at the current tick). Stateful functions, `field` and `event` are compile errors there —
compute them as signals first and reference the signal.

## Examples

```toml
# Traffic density relative to the last couple of minutes, smoothed over 3 s.
signals.density = "norm(smooth(rate(event('car'), 10s), 3s))"

# Is the source busier than usual *right now*? Compare a fast and a slow average.
signals.surge = "clamp(smooth(rate(event('*'), 5s), 1s) / (smooth(rate(event('*'), 5s), 60s) + 0.01) - 1, 0, 1)"

# A bell every 4 bars' worth of seconds at 60 bpm.
signals.bell = "every(16s)"

# One hit when traffic crosses into the top fifth of its own range.
signals.rush = "onset(density, 0.8)"

# Magnitude 2..7 → 0..1, with the loudest quake in the last 10 minutes held.
signals.weight = "hold(norm(event('quake'), 2, 7), 10m)"

# Slow drift that is different for every seed.
signals.drift = "walk(0.1, 8s)"
map.register  = "lerp(0.3, 0.7, drift)"

# Event mapping: bots play low and quiet, humans by edit size.
on_event.edit.pitch    = "if(attr('bot'), 0.1, norm(attr('bytes'), 0, 2000))"
on_event.edit.velocity = "if(attr('bot'), 0.2, 0.5 + 0.5 * intensity)"
on_event.edit.pan      = "hash(attr_text('wiki'))"
on_event.edit.chance   = "if(attr_text('wiki') == 'enwiki', 1, 0.4)"
```

## Rust API (summary)

- `compile(src, &Scope) -> Result<Program, ExprError>` — compile one expression;
  `Program::tick(&Tick)` evaluates it with its own data environment.
- `LayerPipeline::compile(&signals, &map, seed)` — a layer's signals (dependency
  ordered) and map; `push_reading`, `push_event`, then `tick(now, dt, beat, bar, bpm) ->
  Vec<Fired>`; `signals()`, `params()`, `field_names()`.
- `EventExpr::compile(src, &signal_names)` + `eval(&EventCtx)` for `on_event` values.
- `parse_grid("1/16") -> Ok(Some(0.25))` beats; `"1/8t"`, `"1/8."`, `"off"`.
- `ExprError { message, span }` implements `Display`; `render(src)` adds a caret line.

Determinism: nothing reads the wall clock; same inputs and seed produce identical outputs.


### Note on `norm` warm-up

`norm(x)` fades in from a neutral 0.5 over the first 30 seconds (`NORM_WARMUP`). Without this a
value that is simply rising from zero at startup would read as "maximum" for minutes, because it
is always the running max of itself. The adaptive envelope still tracks from the first sample;
only the output confidence ramps.

`attr_len("k")` — character length of a text attribute (event context only).
