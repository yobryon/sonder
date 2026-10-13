//! Realtime safety: after warm-up, `Synth::handle` and `Synth::render` must not
//! allocate. A counting global allocator is enabled only inside a scoped flag so
//! the test harness's own allocations are not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sonder_core::{Chorus, Drum, Effects, Instrument, LayerId, LayerParam, NoteId, Osc, SynthCommand, Table, Wave};
use sonder_dsp::{presets, Synth};

struct Counting;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn render_does_not_allocate_or_free() {
    let sr = 48_000.0;
    let layers = 8;
    let mut synth = Synth::new(sr, layers);
    // Warm-up: assign instruments and effects (these commands carry boxes we recycle).
    let names = presets::names();
    for l in 0..layers {
        let inst = presets::get(names[l % names.len()]).unwrap();
        synth.handle(SynthCommand::SetInstrument { layer: LayerId(l as u16), instrument: Box::new(inst) });
    }
    // Make sure the chorus insert and every wavetable are live on some layer.
    synth.handle(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get("pad.chorus").unwrap()) });
    synth.handle(SynthCommand::SetInstrument { layer: LayerId(1), instrument: Box::new(presets::get("keys.organ").unwrap()) });
    synth.handle(SynthCommand::SetEffects(Box::<Effects>::default()));
    let mut out = vec![0.0f32; 512 * 2];
    synth.render(&mut out);

    // Pre-build every command and instrument box outside the counted region.
    let total_blocks = (10.0 * sr / 512.0) as usize;
    let mut cmds: Vec<Vec<SynthCommand>> = Vec::with_capacity(total_blocks);
    let mut note_id = 0u64;
    for b in 0..total_blocks {
        let mut v = Vec::new();
        let layer = LayerId((b % layers) as u16);
        for k in 0..3 {
            note_id += 1;
            let midi = 36.0 + ((b * 7 + k * 5) % 48) as f32;
            v.push(SynthCommand::NoteOn {
                layer,
                note: NoteId(note_id),
                pitch_hz: 440.0 * 2f32.powf((midi - 69.0) / 12.0),
                velocity: 0.3 + 0.7 * ((b % 5) as f32 / 5.0),
                pan: (k as f32) / 3.0,
            });
        }
        if b > 4 {
            // Release notes from ~4 blocks ago, and some that never existed.
            v.push(SynthCommand::NoteOff { layer: LayerId(((b - 4) % layers) as u16), note: NoteId(note_id - 12) });
            v.push(SynthCommand::NoteOff { layer, note: NoteId(999_999_999) });
            v.push(SynthCommand::NotePitch { layer, note: NoteId(note_id - 13), pitch_hz: 330.0 });
        }
        let drums = [Drum::Kick, Drum::Snare, Drum::Hat, Drum::Click, Drum::Noise, Drum::Tom, Drum::Rim];
        v.push(SynthCommand::Drum { layer: LayerId(7), drum: drums[b % drums.len()], velocity: 0.8, pan: 0.5, tone: (b % 10) as f32 / 10.0 });
        v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::CutoffOctaves(((b % 20) as f32 - 10.0) / 10.0) });
        v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::Gain(0.5 + (b % 3) as f32 * 0.2) });
        v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::Pan((b % 7) as f32 / 7.0) });
        v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::ReverbSend(0.3) });
        v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::Texture((b % 4) as f32 / 4.0) });
        if b % 50 == 0 {
            v.push(SynthCommand::SetLayerParam { layer, param: LayerParam::Mute(b % 100 == 0) });
            v.push(SynthCommand::SetTempo { bpm: 60.0 + (b % 60) as f32 });
        }
        if b % 97 == 0 {
            v.push(SynthCommand::SetInstrument { layer, instrument: Box::new(presets::get(names[b % names.len()]).unwrap()) });
            v.push(SynthCommand::SetEffects(Box::<Effects>::default()));
        }
        if b % 61 == 0 {
            // Toggle chorus on/off and cycle through every wavetable (covers the
            // bypass transition and every mip level as pitches wander).
            let k = b / 61;
            let table = Table::ALL[k % Table::ALL.len()];
            let chorus = if k % 2 == 0 { Chorus { depth: 0.5, rate_hz: 2.0, mix: 0.5, voices: 2 + (k % 2) as u8 } } else { Chorus::default() };
            let inst = Instrument {
                osc: vec![Osc { wave: Wave::Wavetable, table, ..Osc::default() }, Osc { wave: Wave::Wavetable, table, octave: 2, level: 0.3, ..Osc::default() }],
                chorus,
                ..Instrument::default()
            };
            v.push(SynthCommand::SetInstrument { layer: LayerId(((b + 1) % layers) as u16), instrument: Box::new(inst) });
        }
        if b % 400 == 399 {
            v.push(SynthCommand::AllNotesOff { layer });
        }
        if b == total_blocks / 2 {
            v.push(SynthCommand::Panic);
        }
        cmds.push(v);
    }
    // Room to park recycled boxes without freeing them inside the counted region.
    let mut recycled: Vec<Box<Instrument>> = Vec::with_capacity(1024);
    let mut recycled_fx: Vec<Box<Effects>> = Vec::with_capacity(1024);
    let mut feedback_count = 0usize;

    ALLOCS.store(0, Ordering::SeqCst);
    FREES.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    for block in cmds.drain(..) {
        // `Vec::drain` of a Vec<SynthCommand> frees the inner Vec at the end of the
        // loop body; moving the commands out with into_iter keeps that out of the
        // synth's accounting (we subtract frees below anyway).
        for cmd in block {
            synth.handle(cmd);
        }
        synth.render(&mut out);
        synth.drain_feedback(&mut |_| feedback_count += 1);
        while let Some(b) = synth.take_recycled() {
            recycled.push(b);
        }
        while let Some(b) = synth.take_recycled_effects() {
            recycled_fx.push(b);
        }
    }
    COUNTING.store(false, Ordering::SeqCst);
    let allocs = ALLOCS.load(Ordering::SeqCst);
    let frees = FREES.load(Ordering::SeqCst);
    // The only frees in the counted region are the per-block command Vecs we
    // consumed ourselves (one per block). Anything beyond that came from the synth.
    assert_eq!(allocs, 0, "synth allocated {allocs} times during render");
    assert!(frees <= total_blocks, "synth freed memory: {frees} frees (expected ≤ {total_blocks} from test-owned Vecs)");
    assert!(frees > 0, "allocator counting is not active; the test is vacuous");
    assert!(feedback_count > 0);
    assert!(!recycled.is_empty());
    assert!(!recycled_fx.is_empty());
    assert_eq!(synth.leaked_boxes(), 0);
    assert!(out.iter().all(|s| s.is_finite()));
}
