# OSC out

`sonder play <scene> --osc host:port` (also `serve`, `replay`; `EngineOptions::osc_target`
from Rust) mirrors everything the composer and the engine do to a UDP OSC receiver —
TouchDesigner, Max/MSP, Pure Data, SuperCollider, Processing, a lighting desk, another
machine running visuals. Feature `osc` (default on, crate `rosc`).

Everything one control tick (20 ms) produces goes out as **one OSC bundle** (immediate
timetag), so a receiver sees a tick's note-ons, parameter moves and tempo together. A
bundle that would not fit a datagram (> 1400 bytes) is split. Snapshot-derived state
(signals, harmony, form, mood, transport) goes out every snapshot (25 Hz) as its own
bundle.

## Address space

Layers are numbered by their position in the scene (`<i>` = 0-based index; the first
`[[layer]]` is 0). Floats are OSC `f` (32-bit), ints `i`, strings `s`.

| address | arguments | when |
|---|---|---|
| `/sonder/note/on` | `layer:i midi:f velocity:f pan:f` | a note starts (`midi` is fractional for unsnapped pitches; velocity, pan 0..1) |
| `/sonder/note/off` | `layer:i midi:f` | that note is released (same `midi` as its note-on, so you never track ids) |
| `/sonder/note/pitch` | `layer:i midi:f` | a sounding note glides to a new pitch |
| `/sonder/drum` | `layer:i name:s velocity:f pan:f tone:f` | a hit; `name` ∈ `kick snare hat click noise tom rim` |
| `/sonder/layer/<i>/<param>` | `value:f` | a layer parameter moves; `param` ∈ `gain pan cutoff resonance reverb delay texture mute` (`cutoff` is in octaves around the instrument's cutoff, `mute` is 0/1, the rest 0..1) |
| `/sonder/tempo` | `bpm:f` | the transport's tempo changed enough to tell the synth |
| `/sonder/panic` | — | everything was stopped |
| `/sonder/signal/<i>/<name>` | `value:f` | every snapshot: each layer's named `signals.*` value (a signal that disappears on scene reload is sent once as 0) |
| `/sonder/harmony` | `key:s mode:s chord:s` | every snapshot, e.g. `"D" "lydian" "Dmaj7"` |
| `/sonder/form` | `section:s arc:f` | every snapshot: `intro build peak release …` and the 0..1 position along the tension arc |
| `/sonder/mood` | `energy:f tension:f brightness:f` | every snapshot, each 0..1 |
| `/sonder/transport` | `bpm:f bar:i beat:i phase:f` | every snapshot |

On shutdown the sink sends `/sonder/note/off` for every sounding note.

## Receiving

**TouchDesigner**: drop an *OSC In CHOP* (or *OSC In DAT* for the strings), set the port,
leave the address filter at `*`. Each address becomes a channel, so `/sonder/signal/0/flow`
is directly a CHOP channel; `/sonder/mood` arrives as three channels `mood1..3`. For
note-ons use the DAT and parse the message list per frame — notes are events, not values.

**Max/MSP / Pure Data**: `[udpreceive 9000]` → `[oscparse]` (Max) or `[oscparse]` from
Pd's built-in OSC objects → `[route sonder]` → `[route note drum layer signal harmony form
mood tempo transport]`. The bundle is unpacked by `oscparse`/`udpreceive`.

**SuperCollider**: `OSCdef(\notes, { |msg| var layer = msg[1], midi = msg[2], vel = msg[3];
Synth(\pluck, [\freq, midi.midicps, \amp, vel]) }, '/sonder/note/on');` and
`thisProcess.openUDPPort(9000)` (or run `sclang -u 9000`). Bundles with an immediate
timetag are dispatched at once.

**Something else**: anything that speaks OSC 1.0 over UDP. From Python,
`python-osc`'s `Dispatcher().map("/sonder/*", handler)`.

## Example

```
sonder play night-freeway --osc 127.0.0.1:9000
```

A tick's bundle might carry:

```
/sonder/note/on        1 71.0 0.62 0.73
/sonder/layer/0/cutoff 0.31
/sonder/drum           4 "hat" 0.4 0.55 0.5
```

and the next snapshot:

```
/sonder/signal/0/flow   0.42
/sonder/signal/0/speed  0.57
/sonder/harmony         "D" "lydian" "Dmaj7"
/sonder/form            "build" 0.3
/sonder/mood            0.52 0.21 0.66
/sonder/transport       62.0 17 2 0.4
```

OSC is one-way (out). To drive Sonder from outside, use the WebSocket command channel
(`docs/ARCHITECTURE.md`, `UserCommand`), a `human.midi` source, or a source adapter.
