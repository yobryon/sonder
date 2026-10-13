use sonder_core::{LayerId, NoteId, SynthCommand};
use sonder_dsp::{presets, Synth};
#[test]
#[ignore]
fn print_levels() {
    for name in presets::names() {
        let mut synth = Synth::new(48_000.0, 1);
        synth.handle(SynthCommand::SetInstrument { layer: LayerId(0), instrument: Box::new(presets::get(name).unwrap()) });
        for (i, hz) in [130.81f32, 196.0, 261.63, 329.63].iter().enumerate() {
            synth.handle(SynthCommand::NoteOn { layer: LayerId(0), note: NoteId(i as u64), pitch_hz: *hz, velocity: 0.8, pan: 0.5 });
        }
        let mut out = vec![0.0f32; 48_000 * 2 * 3];
        for c in out.chunks_mut(512) { synth.render(c); }
        let seg = |a: f32, b: f32| { let s = &out[(a*96000.0) as usize..(b*96000.0) as usize]; (s.iter().map(|x| x*x).sum::<f32>() / s.len() as f32).sqrt() };
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!("{name:16} rms@0.1s {:.3} rms@1s {:.3} rms@2.5s {:.3} peak {:.3}", seg(0.05,0.15), seg(0.9,1.1), seg(2.4,2.6), peak);
    }
}
