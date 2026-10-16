//! Built-in instrument library. Every preset is a plain [`Instrument`] value
//! with musical defaults; scenes can override any field inline.

use sonder_core::{Adsr, Chorus, Filter, FilterKind, Grain, Instrument, Lfo, LfoShape, Model, Osc, Role, Table, Wave};

/// `(name, description, constructor)` for every preset, in display order.
type Builder = fn() -> Instrument;

const PRESETS: &[(&str, &str, Builder)] = &[
    ("pad.glass", "Bright, slow glassy pad: detuned triangles with a gentle resonant sweep", pad_glass),
    ("pad.warm", "Soft analogue pad: wide supersaw under a low-pass, long release", pad_warm),
    ("pad.strings", "Ensemble strings: slow attack supersaw with vibrato and air", pad_strings),
    ("pad.choir", "Vowel-ish choir: pulse with slow PWM through a band-pass", pad_choir),
    ("pad.wavetable", "Wavetable pad: glass + metal tables, slow sweep, three-voice chorus", pad_wavetable),
    ("pad.chorus", "Chorused analogue pad: saw + triangle through a wide, slow chorus", pad_chorus),
    ("keys.organ", "Drawbar organ: organ table + sine sub, instant envelope, rotary-ish chorus", keys_organ),
    ("drone.sub", "Deep sub drone: sine + octave, slow filter breathing", drone_sub),
    ("drone.saw", "Thick saw drone: detuned saws, dark filter, texture-reactive", drone_saw),
    ("bass.sub", "Round sub bass: sine with a touch of triangle, fast envelope", bass_sub),
    ("bass.pluck", "Plucked synth bass: saw through a snappy filter envelope", bass_pluck),
    ("bass.fm", "FM bass: metallic attack that mellows into a sine", bass_fm),
    ("lead.soft", "Soft lead: triangle + sine, slight glide, singing release", lead_soft),
    ("lead.sine", "Pure sine lead with slow vibrato", lead_sine),
    ("lead.saw", "Classic saw lead with resonant low-pass and delay", lead_saw),
    ("lead.vox", "Vowel lead: vox wavetable with glide, vibrato and light chorus", lead_vox),
    ("arp.pulse", "Arpeggio pulse: short PWM blips with ping-pong delay", arp_pulse),
    ("arp.glass", "Arpeggio glass: FM sparkle with quick decay", arp_glass),
    ("pluck.nylon", "Nylon-string pluck: filtered triangle with fast decay", pluck_nylon),
    ("pluck.glass", "Glass pluck: bright triangle + octave sine, airy reverb", pluck_glass),
    ("pluck.marimba", "Marimba: sine body with a 4:1 FM knock", pluck_marimba),
    ("bells.fm", "FM bells: 3.5:1 ratio, long shimmering decay", bells_fm),
    ("bells.glass", "Glass bells: high sines, slow tremolo, wash of reverb", bells_glass),
    ("perc.kit", "Percussion layer: synthesized kick/snare/hat (uses Drum commands)", perc_kit),
    ("texture.wind", "Wind texture: pink noise through a wandering band-pass", texture_wind),
    ("texture.grain", "Grainy texture: sample-and-hold filtered noise", texture_grain),
    ("texture.noise", "Dark noise bed: low-passed white noise, very slow envelope", texture_noise),
    ("grain.ocean", "Granular wash of the layer's live audio: long sprayed grains, loosely pitched to the note", grain_ocean),
    ("grain.voice", "Granular live audio pitched to the note: short dense grains, tight spray", grain_voice),
    ("string.nylon", "Karplus–Strong nylon string: warm pluck, quick damping", string_nylon),
    ("string.steel", "Karplus–Strong steel string: bright pluck that rings, octave shimmer", string_steel),
    ("modal.bar", "Struck bar (marimba/vibes): harmonic-ish resonator bank, soft mallet", modal_bar),
    ("modal.bell", "Bell: inharmonic resonator bank with a long shimmering decay", modal_bell),
    ("modal.membrane", "Membrane / drum head: dark stretched partials, fast decay", modal_membrane),
];

/// Look up a preset by name.
pub fn get(name: &str) -> Option<Instrument> {
    PRESETS.iter().find(|(n, _, _)| *n == name).map(|(_, _, f)| f())
}

/// All preset names, in display order.
pub fn names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _, _)| *n).collect()
}

/// One-line description of a preset.
pub fn describe(name: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(n, _, _)| *n == name).map(|(_, d, _)| *d)
}

/// A sensible default preset for each role.
pub fn default_for_role(role: Role) -> &'static str {
    match role {
        Role::Pad => "pad.glass",
        Role::Drone => "drone.sub",
        Role::Bass => "bass.sub",
        Role::Lead => "lead.soft",
        Role::Arp => "arp.pulse",
        Role::Pluck => "string.nylon",
        Role::Bells => "modal.bell",
        Role::Perc => "perc.kit",
        Role::Texture => "texture.wind",
        Role::Human => "string.nylon",
    }
}

fn osc(wave: Wave) -> Osc {
    Osc { wave, ..Osc::default() }
}

fn adsr(attack: f32, decay: f32, sustain: f32, release: f32) -> Adsr {
    Adsr { attack, decay, sustain, release }
}

fn lp(cutoff: f32, resonance: f32, env_amount: f32) -> Filter {
    Filter { kind: FilterKind::Lp, cutoff, resonance, env_amount, ..Filter::default() }
}

fn base(desc: &str) -> Instrument {
    Instrument { description: desc.to_string(), ..Instrument::default() }
}

fn pad_glass() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.8, detune_cents: -6.0, ..osc(Wave::Triangle) },
            Osc { level: 0.5, octave: 1, detune_cents: 5.0, ..osc(Wave::Sine) },
        ],
        filter: Filter { key_track: 0.3, lfo_amount: 0.4, ..lp(1800.0, 0.25, 1.2) },
        amp_env: adsr(1.2, 1.0, 0.8, 2.5),
        filter_env: adsr(2.0, 3.0, 0.3, 2.0),
        lfo: Lfo { rate_hz: 0.08, to_pan: 0.2, ..Lfo::default() },
        gain: 0.3,
        width: 0.5,
        reverb_send: 0.55,
        delay_send: 0.1,
        polyphony: 10,
        velocity_to_amp: 0.4,
        ..base("Bright glassy pad")
    }
}

fn pad_warm() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.7, spread: 9.0, ..osc(Wave::Supersaw) },
            Osc { level: 0.35, octave: -1, ..osc(Wave::Triangle) },
        ],
        filter: Filter { key_track: 0.2, lfo_amount: 0.25, ..lp(900.0, 0.15, 1.0) },
        amp_env: adsr(1.5, 1.0, 0.85, 3.0),
        filter_env: adsr(2.5, 2.0, 0.4, 2.5),
        lfo: Lfo { rate_hz: 0.05, ..Lfo::default() },
        gain: 0.4,
        width: 0.8,
        reverb_send: 0.45,
        polyphony: 10,
        velocity_to_amp: 0.3,
        drive: 0.1,
        chorus: Chorus { depth: 0.3, rate_hz: 0.4, mix: 0.4, voices: 2 },
        ..base("Warm analogue pad")
    }
}

fn pad_strings() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.7, spread: 14.0, ..osc(Wave::Supersaw) },
            Osc { level: 0.3, octave: 1, spread: 10.0, ..osc(Wave::Supersaw) },
        ],
        filter: Filter { key_track: 0.4, ..lp(2600.0, 0.1, 0.8) },
        amp_env: adsr(0.9, 1.0, 0.9, 2.2),
        filter_env: adsr(1.5, 2.0, 0.5, 2.0),
        lfo: Lfo { rate_hz: 4.5, to_pitch: 5.0, ..Lfo::default() },
        gain: 0.45,
        width: 0.9,
        reverb_send: 0.5,
        polyphony: 10,
        velocity_to_amp: 0.4,
        chorus: Chorus { depth: 0.25, rate_hz: 0.5, mix: 0.35, voices: 3 },
        ..base("Ensemble strings")
    }
}

fn pad_choir() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.7, pulse_width: 0.3, ..osc(Wave::Pulse) },
            Osc { level: 0.4, detune_cents: 8.0, pulse_width: 0.6, ..osc(Wave::Pulse) },
        ],
        filter: Filter { kind: FilterKind::Bp, cutoff: 900.0, resonance: 0.45, env_amount: 0.6, lfo_amount: 0.5, key_track: 0.5, ..Filter::default() },
        amp_env: adsr(1.0, 1.0, 0.85, 2.0),
        filter_env: adsr(2.0, 2.0, 0.5, 2.0),
        lfo: Lfo { rate_hz: 0.12, to_pulse_width: 0.4, ..Lfo::default() },
        gain: 0.35,
        width: 0.4,
        reverb_send: 0.6,
        polyphony: 8,
        velocity_to_amp: 0.3,
        ..base("Choir-ish pad")
    }
}

fn wt(table: Table) -> Osc {
    Osc { wave: Wave::Wavetable, table, ..Osc::default() }
}

fn pad_wavetable() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.7, detune_cents: -5.0, ..wt(Table::Glass) },
            Osc { level: 0.3, octave: -1, detune_cents: 4.0, ..wt(Table::Metal) },
        ],
        filter: Filter { key_track: 0.3, lfo_amount: 0.3, ..lp(1400.0, 0.2, 1.0) },
        amp_env: adsr(1.4, 1.0, 0.85, 2.8),
        filter_env: adsr(2.5, 3.0, 0.4, 2.5),
        lfo: Lfo { rate_hz: 0.07, to_pan: 0.15, ..Lfo::default() },
        gain: 0.3,
        width: 0.4,
        reverb_send: 0.55,
        delay_send: 0.1,
        polyphony: 10,
        velocity_to_amp: 0.4,
        chorus: Chorus { depth: 0.4, rate_hz: 0.5, mix: 0.5, voices: 3 },
        ..base("Wavetable pad")
    }
}

fn pad_chorus() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.6, ..osc(Wave::Saw) },
            Osc { level: 0.5, octave: -1, detune_cents: 6.0, ..osc(Wave::Triangle) },
        ],
        filter: Filter { key_track: 0.3, lfo_amount: 0.3, ..lp(1100.0, 0.15, 0.9) },
        amp_env: adsr(1.0, 1.0, 0.85, 2.5),
        filter_env: adsr(2.0, 2.5, 0.4, 2.0),
        lfo: Lfo { rate_hz: 0.06, ..Lfo::default() },
        gain: 0.35,
        width: 0.5,
        reverb_send: 0.4,
        polyphony: 10,
        velocity_to_amp: 0.3,
        drive: 0.08,
        chorus: Chorus { depth: 0.6, rate_hz: 0.35, mix: 0.5, voices: 3 },
        ..base("Chorused analogue pad")
    }
}

fn keys_organ() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.85, ..wt(Table::Organ) }, Osc { level: 0.3, octave: -1, ..osc(Wave::Sine) }],
        filter: Filter { key_track: 0.5, ..lp(5000.0, 0.05, 0.0) },
        amp_env: adsr(0.004, 0.05, 1.0, 0.12),
        filter_env: adsr(0.004, 0.1, 1.0, 0.1),
        gain: 0.4,
        reverb_send: 0.25,
        polyphony: 8,
        velocity_to_amp: 0.25,
        drive: 0.12,
        chorus: Chorus { depth: 0.35, rate_hz: 1.2, mix: 0.45, voices: 2 },
        ..base("Drawbar organ")
    }
}

fn lead_vox() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, ..wt(Table::Vox) }, Osc { level: 0.25, octave: -1, ..wt(Table::Sine8) }],
        filter: Filter { key_track: 0.5, velocity_amount: 0.6, ..lp(2200.0, 0.2, 0.8) },
        amp_env: adsr(0.05, 0.4, 0.7, 0.8),
        filter_env: adsr(0.03, 0.5, 0.4, 0.6),
        lfo: Lfo { rate_hz: 4.8, to_pitch: 5.0, ..Lfo::default() },
        glide: 0.07,
        gain: 0.45,
        reverb_send: 0.4,
        delay_send: 0.25,
        polyphony: 3,
        velocity_to_amp: 0.6,
        chorus: Chorus { depth: 0.25, rate_hz: 0.7, mix: 0.4, voices: 2 },
        ..base("Vowel lead")
    }
}

fn drone_sub() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, ..osc(Wave::Sine) }, Osc { level: 0.35, octave: 1, detune_cents: 3.0, ..osc(Wave::Triangle) }],
        filter: Filter { lfo_amount: 0.8, ..lp(400.0, 0.2, 0.0) },
        amp_env: adsr(3.0, 1.0, 1.0, 4.0),
        filter_env: adsr(3.0, 3.0, 0.5, 3.0),
        lfo: Lfo { rate_hz: 0.04, ..Lfo::default() },
        gain: 0.35,
        reverb_send: 0.15,
        polyphony: 4,
        velocity_to_amp: 0.2,
        drive: 0.15,
        ..base("Deep sub drone")
    }
}

fn drone_saw() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.6, spread: 7.0, ..osc(Wave::Supersaw) }, Osc { level: 0.5, octave: -1, ..osc(Wave::Saw) }],
        filter: Filter { lfo_amount: 0.6, ..lp(500.0, 0.3, 0.3) },
        amp_env: adsr(2.5, 1.0, 1.0, 3.5),
        filter_env: adsr(4.0, 4.0, 0.5, 3.0),
        lfo: Lfo { rate_hz: 0.06, shape: LfoShape::Triangle, ..Lfo::default() },
        gain: 0.3,
        width: 0.7,
        reverb_send: 0.3,
        polyphony: 4,
        velocity_to_amp: 0.2,
        drive: 0.25,
        ..base("Thick saw drone")
    }
}

fn bass_sub() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 1.0, ..osc(Wave::Sine) }, Osc { level: 0.25, ..osc(Wave::Triangle) }],
        filter: lp(600.0, 0.1, 0.8),
        amp_env: adsr(0.005, 0.3, 0.7, 0.25),
        filter_env: adsr(0.005, 0.25, 0.2, 0.2),
        gain: 0.5,
        reverb_send: 0.05,
        polyphony: 3,
        velocity_to_amp: 0.5,
        drive: 0.2,
        ..base("Round sub bass")
    }
}

fn bass_pluck() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, ..osc(Wave::Saw) }, Osc { level: 0.4, octave: -1, pulse_width: 0.5, ..osc(Wave::Square) }],
        filter: Filter { velocity_amount: 1.0, key_track: 0.3, ..lp(300.0, 0.4, 2.8) },
        amp_env: adsr(0.003, 0.35, 0.3, 0.2),
        filter_env: adsr(0.002, 0.18, 0.0, 0.2),
        gain: 0.65,
        reverb_send: 0.1,
        delay_send: 0.1,
        polyphony: 3,
        velocity_to_amp: 0.7,
        drive: 0.3,
        ..base("Plucked synth bass")
    }
}

fn bass_fm() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, fm_ratio: 1.0, fm_index: 3.0, fm_index_env: 0.85, ..osc(Wave::Fm) }, Osc { level: 0.3, octave: -1, ..osc(Wave::Sine) }],
        filter: lp(1400.0, 0.1, 1.0),
        amp_env: adsr(0.003, 0.4, 0.5, 0.25),
        filter_env: adsr(0.002, 0.3, 0.0, 0.2),
        gain: 0.55,
        reverb_send: 0.08,
        polyphony: 3,
        velocity_to_amp: 0.7,
        drive: 0.15,
        ..base("FM bass")
    }
}

fn lead_soft() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.7, ..osc(Wave::Triangle) }, Osc { level: 0.4, octave: 1, detune_cents: 4.0, ..osc(Wave::Sine) }],
        filter: Filter { key_track: 0.5, velocity_amount: 0.6, ..lp(1600.0, 0.2, 1.2) },
        amp_env: adsr(0.04, 0.4, 0.6, 0.9),
        filter_env: adsr(0.02, 0.5, 0.3, 0.6),
        lfo: Lfo { rate_hz: 5.0, to_pitch: 4.0, ..Lfo::default() },
        glide: 0.08,
        gain: 0.5,
        reverb_send: 0.4,
        delay_send: 0.3,
        polyphony: 3,
        velocity_to_amp: 0.6,
        ..base("Soft lead")
    }
}

fn lead_sine() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 1.0, ..osc(Wave::Sine) }],
        filter: Filter { kind: FilterKind::Off, ..Filter::default() },
        amp_env: adsr(0.03, 0.3, 0.8, 0.6),
        lfo: Lfo { rate_hz: 4.2, to_pitch: 6.0, ..Lfo::default() },
        glide: 0.05,
        gain: 0.4,
        reverb_send: 0.35,
        delay_send: 0.25,
        polyphony: 2,
        velocity_to_amp: 0.6,
        ..base("Pure sine lead")
    }
}

fn lead_saw() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, ..osc(Wave::Saw) }, Osc { level: 0.5, detune_cents: 9.0, ..osc(Wave::Saw) }],
        filter: Filter { key_track: 0.4, velocity_amount: 0.8, ..lp(1200.0, 0.45, 2.0) },
        amp_env: adsr(0.01, 0.3, 0.7, 0.5),
        filter_env: adsr(0.005, 0.35, 0.2, 0.4),
        lfo: Lfo { rate_hz: 5.5, to_pitch: 5.0, ..Lfo::default() },
        glide: 0.04,
        gain: 0.45,
        reverb_send: 0.3,
        delay_send: 0.35,
        polyphony: 3,
        velocity_to_amp: 0.7,
        drive: 0.2,
        ..base("Classic saw lead")
    }
}

fn arp_pulse() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, pulse_width: 0.25, ..osc(Wave::Pulse) }, Osc { level: 0.3, octave: -1, ..osc(Wave::Square) }],
        filter: Filter { key_track: 0.5, velocity_amount: 1.0, ..lp(1000.0, 0.35, 2.5) },
        amp_env: adsr(0.002, 0.18, 0.0, 0.25),
        filter_env: adsr(0.001, 0.12, 0.0, 0.2),
        lfo: Lfo { rate_hz: 0.2, to_pulse_width: 0.3, ..Lfo::default() },
        gain: 0.45,
        reverb_send: 0.25,
        delay_send: 0.45,
        polyphony: 6,
        velocity_to_amp: 0.7,
        ..base("Arpeggio pulse")
    }
}

fn arp_glass() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, fm_ratio: 2.0, fm_index: 1.8, fm_index_env: 0.9, ..osc(Wave::Fm) }, Osc { level: 0.3, octave: 1, ..osc(Wave::Sine) }],
        filter: lp(6000.0, 0.1, 0.0),
        amp_env: adsr(0.002, 0.35, 0.0, 0.5),
        gain: 0.5,
        width: 0.3,
        reverb_send: 0.4,
        delay_send: 0.4,
        polyphony: 8,
        velocity_to_amp: 0.7,
        ..base("Arpeggio glass")
    }
}

fn pluck_nylon() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, ..osc(Wave::Triangle) }, Osc { level: 0.3, octave: 1, ..osc(Wave::Sine) }],
        filter: Filter { key_track: 0.6, velocity_amount: 1.2, ..lp(900.0, 0.2, 2.2) },
        amp_env: adsr(0.002, 0.5, 0.0, 0.6),
        filter_env: adsr(0.001, 0.15, 0.0, 0.3),
        gain: 0.5,
        reverb_send: 0.35,
        delay_send: 0.1,
        polyphony: 12,
        velocity_to_amp: 0.8,
        drive: 0.1,
        ..base("Nylon pluck")
    }
}

fn pluck_glass() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.7, ..osc(Wave::Triangle) }, Osc { level: 0.5, octave: 1, detune_cents: 3.0, ..osc(Wave::Sine) }],
        filter: Filter { key_track: 0.7, velocity_amount: 0.8, ..lp(2400.0, 0.25, 1.4) },
        amp_env: adsr(0.002, 0.7, 0.0, 1.0),
        filter_env: adsr(0.001, 0.3, 0.0, 0.5),
        gain: 0.5,
        width: 0.3,
        reverb_send: 0.55,
        delay_send: 0.2,
        polyphony: 12,
        velocity_to_amp: 0.7,
        ..base("Glass pluck")
    }
}

fn pluck_marimba() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, ..osc(Wave::Sine) }, Osc { level: 0.45, fm_ratio: 4.0, fm_index: 1.2, fm_index_env: 1.0, ..osc(Wave::Fm) }],
        filter: Filter { key_track: 0.5, ..lp(5000.0, 0.1, 0.0) },
        amp_env: adsr(0.001, 0.35, 0.0, 0.4),
        gain: 0.55,
        reverb_send: 0.3,
        polyphony: 10,
        velocity_to_amp: 0.8,
        ..base("Marimba")
    }
}

fn bells_fm() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, fm_ratio: 3.5, fm_index: 1.6, fm_index_env: 0.7, ..osc(Wave::Fm) }, Osc { level: 0.25, octave: 2, ..osc(Wave::Sine) }],
        filter: Filter { kind: FilterKind::Hp, cutoff: 300.0, ..Filter::default() },
        amp_env: adsr(0.002, 2.5, 0.0, 3.0),
        lfo: Lfo { rate_hz: 0.3, to_pan: 0.3, ..Lfo::default() },
        gain: 0.4,
        reverb_send: 0.6,
        delay_send: 0.25,
        polyphony: 12,
        velocity_to_amp: 0.7,
        ..base("FM bells")
    }
}

fn bells_glass() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.7, octave: 1, ..osc(Wave::Sine) }, Osc { level: 0.4, octave: 2, semitones: 7.0, detune_cents: 4.0, ..osc(Wave::Sine) }],
        filter: Filter { kind: FilterKind::Off, ..Filter::default() },
        amp_env: adsr(0.005, 3.0, 0.0, 3.5),
        lfo: Lfo { rate_hz: 3.0, to_amp: 0.25, ..Lfo::default() },
        gain: 0.4,
        width: 0.4,
        reverb_send: 0.7,
        delay_send: 0.3,
        polyphony: 12,
        velocity_to_amp: 0.6,
        ..base("Glass bells")
    }
}

fn perc_kit() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.6, ..osc(Wave::Noise) }],
        filter: Filter { kind: FilterKind::Bp, cutoff: 3000.0, resonance: 0.3, ..Filter::default() },
        amp_env: adsr(0.001, 0.08, 0.0, 0.05),
        gain: 0.8,
        reverb_send: 0.2,
        polyphony: 8,
        velocity_to_amp: 0.9,
        ..base("Percussion kit")
    }
}

fn texture_wind() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, ..osc(Wave::Pink) }],
        filter: Filter { kind: FilterKind::Bp, cutoff: 700.0, resonance: 0.55, lfo_amount: 1.5, ..Filter::default() },
        amp_env: adsr(4.0, 1.0, 1.0, 5.0),
        lfo: Lfo { rate_hz: 0.07, shape: LfoShape::Triangle, to_pan: 0.5, ..Lfo::default() },
        gain: 0.5,
        reverb_send: 0.5,
        polyphony: 2,
        velocity_to_amp: 0.3,
        ..base("Wind texture")
    }
}

fn texture_grain() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.8, ..osc(Wave::Noise) }, Osc { level: 0.3, ..osc(Wave::Pink) }],
        filter: Filter { kind: FilterKind::Bp, cutoff: 1500.0, resonance: 0.75, lfo_amount: 2.0, ..Filter::default() },
        amp_env: adsr(2.0, 1.0, 1.0, 3.0),
        lfo: Lfo { rate_hz: 6.0, shape: LfoShape::Random, to_amp: 0.4, to_pan: 0.6, ..Lfo::default() },
        gain: 0.4,
        reverb_send: 0.45,
        delay_send: 0.3,
        polyphony: 2,
        velocity_to_amp: 0.3,
        ..base("Grainy texture")
    }
}

fn texture_noise() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, ..osc(Wave::Noise) }],
        filter: Filter { lfo_amount: 0.5, ..lp(500.0, 0.2, 0.0) },
        amp_env: adsr(6.0, 1.0, 1.0, 6.0),
        lfo: Lfo { rate_hz: 0.03, ..Lfo::default() },
        gain: 1.2,
        reverb_send: 0.3,
        polyphony: 2,
        velocity_to_amp: 0.2,
        ..base("Dark noise bed")
    }
}

fn grain_ocean() -> Instrument {
    Instrument {
        osc: vec![Osc {
            level: 0.9,
            grain: Grain { ring_seconds: 6.0, size_ms: 180.0, density: 24.0, spray: 3.0, pitch_follow: 0.6, jitter_cents: 25.0, width: 0.9 },
            ..osc(Wave::Grain)
        }],
        filter: Filter { lfo_amount: 0.4, ..lp(3500.0, 0.15, 0.0) },
        amp_env: adsr(1.5, 1.0, 1.0, 3.0),
        lfo: Lfo { rate_hz: 0.05, shape: LfoShape::Triangle, ..Lfo::default() },
        gain: 0.6,
        width: 0.9,
        reverb_send: 0.5,
        delay_send: 0.15,
        polyphony: 4,
        velocity_to_amp: 0.4,
        ..base("Granular ocean")
    }
}

fn grain_voice() -> Instrument {
    Instrument {
        osc: vec![Osc {
            level: 1.0,
            grain: Grain { ring_seconds: 2.0, size_ms: 30.0, density: 90.0, spray: 0.08, pitch_follow: 1.0, jitter_cents: 4.0, width: 0.3 },
            ..osc(Wave::Grain)
        }],
        filter: Filter { key_track: 0.3, ..lp(6000.0, 0.1, 0.0) },
        amp_env: adsr(0.03, 0.2, 0.9, 0.4),
        gain: 0.7,
        width: 0.3,
        reverb_send: 0.3,
        polyphony: 6,
        velocity_to_amp: 0.5,
        ..base("Granular voice")
    }
}

fn string_nylon() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, model: Model { damping: 0.45, brightness: 0.45, position: 0.22, ..Model::default() }, ..osc(Wave::String) }],
        filter: Filter { key_track: 0.5, velocity_amount: 0.8, ..lp(2800.0, 0.1, 0.0) },
        amp_env: adsr(0.001, 0.5, 1.0, 0.5),
        gain: 0.55,
        reverb_send: 0.35,
        delay_send: 0.1,
        polyphony: 12,
        velocity_to_amp: 0.8,
        ..base("Nylon string")
    }
}

fn string_steel() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.85, model: Model { damping: 0.15, brightness: 0.9, position: 0.13, ..Model::default() }, ..osc(Wave::String) },
            Osc { level: 0.25, octave: 1, model: Model { damping: 0.35, brightness: 0.8, position: 0.2, ..Model::default() }, ..osc(Wave::String) },
        ],
        filter: Filter { key_track: 0.4, ..lp(7000.0, 0.05, 0.0) },
        amp_env: adsr(0.001, 0.8, 1.0, 0.8),
        gain: 0.5,
        width: 0.3,
        reverb_send: 0.4,
        delay_send: 0.2,
        polyphony: 12,
        velocity_to_amp: 0.75,
        ..base("Steel string")
    }
}

fn modal_bar() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 0.9, model: Model { damping: 0.35, brightness: 0.4, position: 0.3, stiffness: 0.15, modes: 8 }, ..osc(Wave::Modal) }],
        filter: Filter { key_track: 0.5, ..lp(6000.0, 0.05, 0.0) },
        amp_env: adsr(0.001, 0.5, 1.0, 0.6),
        gain: 0.6,
        reverb_send: 0.3,
        polyphony: 10,
        velocity_to_amp: 0.8,
        ..base("Struck bar")
    }
}

fn modal_bell() -> Instrument {
    Instrument {
        osc: vec![
            Osc { level: 0.8, model: Model { damping: 0.08, brightness: 0.75, position: 0.17, stiffness: 1.0, modes: 12 }, ..osc(Wave::Modal) },
            Osc { level: 0.2, octave: 2, model: Model { damping: 0.3, brightness: 0.6, position: 0.25, stiffness: 1.0, modes: 6 }, ..osc(Wave::Modal) },
        ],
        filter: Filter { kind: FilterKind::Hp, cutoff: 200.0, ..Filter::default() },
        amp_env: adsr(0.001, 3.0, 1.0, 3.0),
        lfo: Lfo { rate_hz: 0.3, to_pan: 0.3, ..Lfo::default() },
        gain: 0.45,
        width: 0.4,
        reverb_send: 0.6,
        delay_send: 0.25,
        polyphony: 12,
        velocity_to_amp: 0.7,
        ..base("Bell")
    }
}

fn modal_membrane() -> Instrument {
    Instrument {
        osc: vec![Osc { level: 1.0, model: Model { damping: 0.7, brightness: 0.25, position: 0.4, stiffness: 0.6, modes: 10 }, ..osc(Wave::Modal) }],
        filter: lp(2500.0, 0.2, 0.0),
        amp_env: adsr(0.001, 0.4, 1.0, 0.3),
        gain: 0.7,
        reverb_send: 0.25,
        polyphony: 8,
        velocity_to_amp: 0.9,
        ..base("Membrane")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_is_consistent() {
        let n = names();
        assert!(n.len() >= 20);
        for name in &n {
            assert!(get(name).is_some());
            assert!(describe(name).is_some());
        }
        for role in Role::ALL {
            assert!(get(default_for_role(role)).is_some(), "{role:?}");
        }
        assert!(get("nope").is_none());
    }

    #[test]
    fn new_presets_use_the_new_features() {
        let wavetable_users: Vec<_> = names().into_iter().filter(|n| get(n).unwrap().osc.iter().any(|o| o.wave == Wave::Wavetable)).collect();
        assert!(wavetable_users.contains(&"pad.wavetable"));
        assert!(wavetable_users.contains(&"keys.organ"));
        assert!(wavetable_users.contains(&"lead.vox"));
        let chorus_users: Vec<_> = names().into_iter().filter(|n| get(n).unwrap().chorus.depth > 0.0).collect();
        assert!(chorus_users.contains(&"pad.chorus"));
        assert!(chorus_users.contains(&"pad.warm"));
        for n in chorus_users {
            let c = get(n).unwrap().chorus;
            assert!((0.1..=5.0).contains(&c.rate_hz) && (2..=3).contains(&c.voices), "{n}");
        }
        for n in ["pad.wavetable", "pad.chorus", "keys.organ", "lead.vox"] {
            assert!(describe(n).is_some());
        }
    }

    #[test]
    fn v2_presets_use_the_new_waves() {
        let uses = |name: &str, w: Wave| get(name).unwrap().osc.iter().any(|o| o.wave == w);
        assert!(uses("grain.ocean", Wave::Grain) && uses("grain.voice", Wave::Grain));
        assert!(uses("string.nylon", Wave::String) && uses("string.steel", Wave::String));
        for n in ["modal.bar", "modal.bell", "modal.membrane"] {
            assert!(uses(n, Wave::Modal), "{n}");
            assert!(get(n).unwrap().osc.iter().all(|o| (1..=32).contains(&o.model.modes)), "{n}");
        }
        assert!(get("grain.voice").unwrap().osc[0].grain.density > get("grain.ocean").unwrap().osc[0].grain.density);
        assert!(default_for_role(Role::Pluck).starts_with("string."));
        assert!(default_for_role(Role::Bells).starts_with("modal."));
    }
}
