//! A synth voice: two oscillators → stereo SVF → drive → amp, with amp/filter
//! envelopes, an LFO and glide. Voices snapshot the instrument into a plain
//! [`Patch`] at note-on so instrument changes never touch sounding notes.

use sonder_core::{Adsr, Filter, FilterKind, Instrument, LayerId, Lfo as LfoCfg, NoteId, Osc, Table, Wave};

use crate::env::Envelope;
use crate::filter::{Svf, SvfCoefs};
use crate::grain::{AudioRing, GrainPlayer};
use crate::lfo::Lfo;
use crate::math::{hz_to_midi, lerp, onepole_coef, pan_gains, saturate, Rng};
use crate::osc::{osc_ratio, OscParams, OscState};
use crate::physical::{KarplusStrong, ModalBank};
use crate::BLOCK;

/// Plain-data copy of the parts of an [`Instrument`] a voice needs. `Clone` of
/// this never allocates (no `Vec`/`String`).
#[derive(Debug, Clone)]
pub struct Patch {
    pub osc: [Osc; 2],
    pub n_osc: usize,
    pub filter: Filter,
    pub amp_env: Adsr,
    pub filter_env: Adsr,
    pub lfo: LfoCfg,
    pub glide: f32,
    pub gain: f32,
    pub pan: f32,
    pub width: f32,
    pub reverb_send: f32,
    pub delay_send: f32,
    pub polyphony: usize,
    pub velocity_to_amp: f32,
    pub drive: f32,
}

impl Patch {
    pub fn from_instrument(i: &Instrument) -> Self {
        let n_osc = i.osc.len().clamp(1, 2);
        let mut osc = [Osc::default(), Osc::default()];
        for (k, o) in i.osc.iter().take(2).enumerate() {
            osc[k] = o.clone();
        }
        if i.osc.is_empty() {
            osc[0] = Osc::default();
        }
        Self {
            osc,
            n_osc,
            filter: i.filter.clone(),
            amp_env: i.amp_env,
            filter_env: i.filter_env,
            lfo: i.lfo.clone(),
            glide: i.glide.max(0.0),
            gain: i.gain.clamp(0.0, 4.0),
            pan: i.pan.clamp(0.0, 1.0),
            width: i.width.clamp(0.0, 1.0),
            reverb_send: i.reverb_send.clamp(0.0, 1.0),
            delay_send: i.delay_send.clamp(0.0, 1.0),
            polyphony: (i.polyphony as usize).clamp(1, 64),
            velocity_to_amp: i.velocity_to_amp.clamp(0.0, 1.0),
            drive: i.drive.clamp(0.0, 1.0),
        }
    }
}

impl Default for Patch {
    fn default() -> Self {
        Self::from_instrument(&Instrument::default())
    }
}

/// Per-block layer modulation applied to voices.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayerMod {
    /// Cutoff offset in octaves.
    pub cutoff_octaves: f32,
    /// Resonance override (None = instrument's).
    pub resonance: Option<f32>,
    /// Texture macro 0..1 → extra detune/drive/noise.
    pub texture: f32,
}

#[derive(Debug, Clone)]
pub struct Voice {
    pub active: bool,
    pub layer: LayerId,
    pub note: NoteId,
    /// Monotonic start counter for oldest-voice stealing.
    pub age: u64,
    pub patch: Patch,
    osc: [OscState; 2],
    /// Physical-model and granular state per oscillator slot (used when the slot's
    /// wave is `String` / `Modal` / `Grain`).
    string: [KarplusStrong; 2],
    modal: [ModalBank; 2],
    grains: [GrainPlayer; 2],
    filt: [Svf; 2],
    coefs: SvfCoefs,
    coefs_ready: bool,
    amp_env: Envelope,
    filt_env: Envelope,
    lfo: Lfo,
    rng: Rng,
    velocity: f32,
    note_pan: f32,
    /// Current and target pitch in MIDI (glide runs in the log domain).
    midi_cur: f32,
    midi_target: f32,
    glide_coef: f32,
    /// Block-interpolated stereo gains (velocity × LFO amp × pan).
    gain_l: f32,
    gain_r: f32,
    sample_rate: f32,
    tables: &'static crate::wavetable::Tables,
}

impl Default for Voice {
    fn default() -> Self {
        Self::new(48_000.0)
    }
}

impl Voice {
    /// A silent voice whose physical-model buffers are sized for `sample_rate`.
    pub fn new(sample_rate: f32) -> Self {
        Self {
            active: false,
            layer: LayerId(0),
            note: NoteId(0),
            age: 0,
            patch: Patch::default(),
            osc: [OscState::default(); 2],
            string: [KarplusStrong::new(sample_rate), KarplusStrong::new(sample_rate)],
            modal: [ModalBank::new(sample_rate), ModalBank::new(sample_rate)],
            grains: [GrainPlayer::default(), GrainPlayer::default()],
            filt: [Svf::default(); 2],
            coefs: SvfCoefs::default(),
            coefs_ready: false,
            amp_env: Envelope::default(),
            filt_env: Envelope::default(),
            lfo: Lfo::default(),
            rng: Rng::new(1),
            velocity: 1.0,
            note_pan: 0.5,
            midi_cur: 60.0,
            midi_target: 60.0,
            glide_coef: 1.0,
            gain_l: 0.0,
            gain_r: 0.0,
            sample_rate,
            tables: crate::wavetable::tables(),
        }
    }
}

/// Inputs to [`Voice::start`].
pub struct NoteStart<'a> {
    pub layer: LayerId,
    pub note: NoteId,
    pub pitch_hz: f32,
    pub velocity: f32,
    pub pan: f32,
    pub patch: &'a Patch,
    /// Pitch to glide from (last note on the layer), if the patch glides.
    pub glide_from_midi: Option<f32>,
    /// Layer-wide free-running LFO phase (used when `retrigger` is false).
    pub lfo_phase: f32,
    pub age: u64,
    pub seed: u32,
}

impl Voice {
    pub fn start(&mut self, sr: f32, s: NoteStart<'_>) {
        self.sample_rate = sr;
        self.active = true;
        self.layer = s.layer;
        self.note = s.note;
        self.age = s.age;
        self.patch = s.patch.clone();
        self.velocity = s.velocity.clamp(0.0, 1.0);
        self.note_pan = s.pan.clamp(0.0, 1.0);
        self.rng = Rng::new(s.seed);
        let target = hz_to_midi(s.pitch_hz.clamp(1.0, 20_000.0));
        self.midi_target = target;
        self.midi_cur = match s.glide_from_midi {
            Some(from) if self.patch.glide > 0.0 => from,
            _ => target,
        };
        self.glide_coef = onepole_coef(self.patch.glide / 3.0, sr / BLOCK as f32);
        let base_hz = 440.0 * 2f32.powf((self.midi_cur - 69.0) / 12.0);
        for (k, o) in self.osc.iter_mut().enumerate() {
            let seed = s.seed.wrapping_add(k as u32 * 7919);
            o.reset(seed);
            let cfg = &self.patch.osc[k];
            let hz = base_hz * osc_ratio(cfg);
            match cfg.wave {
                Wave::Supersaw => o.randomize_phases(),
                Wave::String => self.string[k].trigger(&cfg.model, hz, seed),
                Wave::Modal => self.modal[k].trigger(&cfg.model, hz, seed),
                Wave::Grain => self.grains[k].reset(seed),
                _ => {}
            }
        }
        for f in &mut self.filt {
            f.reset();
        }
        self.coefs_ready = false;
        self.amp_env.configure(&self.patch.amp_env, sr);
        self.filt_env.configure(&self.patch.filter_env, sr);
        self.amp_env.trigger();
        self.filt_env.trigger();
        let phase = if self.patch.lfo.retrigger { 0.0 } else { s.lfo_phase };
        self.lfo.reset(phase, s.seed ^ 0x5555);
        self.gain_l = 0.0;
        self.gain_r = 0.0;
    }

    pub fn release(&mut self) {
        self.amp_env.release();
        self.filt_env.release();
    }

    /// Fast fade-out (voice stealing / panic).
    pub fn kill(&mut self, seconds: f32) {
        self.amp_env.fast_release(seconds, self.sample_rate);
        self.filt_env.release();
    }

    pub fn set_pitch(&mut self, hz: f32) {
        self.midi_target = hz_to_midi(hz.clamp(1.0, 20_000.0));
        if self.patch.glide <= 0.0 {
            self.midi_cur = self.midi_target;
        }
    }

    pub fn is_released(&self) -> bool {
        self.amp_env.is_released()
    }

    /// Current amp envelope level (for quietest-voice stealing).
    pub fn level(&self) -> f32 {
        self.amp_env.level
    }

    pub fn current_midi(&self) -> f32 {
        self.midi_cur
    }

    /// Render one block, *adding* into `out` (interleaved stereo, `BLOCK` frames).
    /// `ring` is the layer's live audio for `Wave::Grain`. Returns `false` when the
    /// voice has finished and should be freed.
    pub fn render(&mut self, out: &mut [f32], m: &LayerMod, ring: &AudioRing) -> bool {
        debug_assert!(out.len() >= BLOCK * 2);
        let sr = self.sample_rate;
        let dt_block = BLOCK as f32 / sr;

        // Glide (per block, log domain).
        self.midi_cur += self.glide_coef * (self.midi_target - self.midi_cur);

        // LFO once per block.
        let lfo_v = self.lfo.step(self.patch.lfo.shape, self.patch.lfo.rate_hz, dt_block);
        let lfo_cents = lfo_v * self.patch.lfo.to_pitch;
        let lfo_amp = 1.0 - self.patch.lfo.to_amp.clamp(0.0, 1.0) * 0.5 * (1.0 - lfo_v);
        let lfo_pan = lfo_v * self.patch.lfo.to_pan.clamp(0.0, 1.0) * 0.5;
        let lfo_pw = lfo_v * self.patch.lfo.to_pulse_width.clamp(0.0, 1.0) * 0.4;

        // Texture macro: more spread/detune, a little drive.
        let texture = m.texture.clamp(0.0, 1.0);
        let spread_mul = 1.0 + texture * 1.5;
        let drive = (self.patch.drive + texture * 0.35).clamp(0.0, 1.0);

        // Base frequency.
        let midi = self.midi_cur + lfo_cents / 100.0;
        let base_hz = 440.0 * 2f32.powf((midi - 69.0) / 12.0);

        // Filter cutoff (block-rate modulation, coefficients interpolated per sample).
        let f = &self.patch.filter;
        let resonance = m.resonance.unwrap_or(f.resonance);
        let fenv = self.filt_env.level;
        let octaves = f.env_amount * fenv
            + f.lfo_amount * lfo_v
            + f.key_track * (self.midi_cur - 60.0) / 12.0
            + f.velocity_amount * self.velocity
            + m.cutoff_octaves;
        let cutoff = f.cutoff * 2f32.powf(octaves.clamp(-10.0, 10.0));
        let new_coefs = SvfCoefs::new(cutoff, resonance, sr);
        let old_coefs = if self.coefs_ready { self.coefs } else { new_coefs };
        self.coefs = new_coefs;
        self.coefs_ready = true;
        let kind = f.kind;

        // Stereo gains for this block (ramped from the previous block).
        let pan = (self.patch.pan + (self.note_pan - 0.5) + lfo_pan).clamp(0.0, 1.0);
        let (pl, pr) = pan_gains(pan);
        let vel_amp = lerp(1.0, self.velocity * self.velocity, self.patch.velocity_to_amp);
        let g = vel_amp * lfo_amp * self.patch.gain;
        let target_l = g * pl;
        let target_r = g * pr;
        let start_l = self.gain_l;
        let start_r = self.gain_r;
        self.gain_l = target_l;
        self.gain_r = target_r;

        // Oscillator params.
        let n_osc = self.patch.n_osc;
        let mut params = [OscParams {
            freq_hz: base_hz,
            sample_rate: sr,
            pulse_width: 0.5,
            spread_cents: 12.0,
            width: self.patch.width,
            fm_index: 0.0,
            fm_ratio: 2.0,
            table: Table::Organ,
            tables: self.tables,
        }; 2];
        let mut levels = [0.0f32; 2];
        let mut waves = [Wave::Sine; 2];
        for k in 0..n_osc {
            let o = &self.patch.osc[k];
            let p = &mut params[k];
            p.freq_hz = (base_hz * osc_ratio(o) * (1.0 + texture * 0.002 * k as f32)).clamp(0.0, sr * 0.49);
            p.pulse_width = (o.pulse_width + lfo_pw).clamp(0.05, 0.95);
            p.spread_cents = o.spread * spread_mul;
            p.fm_ratio = o.fm_ratio;
            p.table = o.table;
            levels[k] = o.level.clamp(0.0, 1.0);
            waves[k] = o.wave;
            match o.wave {
                Wave::String => self.string[k].set_freq(p.freq_hz),
                Wave::Modal => self.modal[k].set_freq(p.freq_hz),
                Wave::Grain => self.grains[k].configure(&o.grain, p.freq_hz, ring, sr),
                _ => {}
            }
        }
        let noise_floor = texture * 0.04;

        let inv = 1.0 / BLOCK as f32;
        let mut alive = true;
        for i in 0..BLOCK {
            let t = i as f32 * inv;
            let amp = self.amp_env.tick();
            self.filt_env.tick();
            if self.amp_env.is_idle() {
                alive = false;
                break;
            }
            // FM index follows the amp envelope when requested.
            for (p, o) in params.iter_mut().zip(self.patch.osc.iter()).take(n_osc) {
                p.fm_index = o.fm_index * (1.0 - o.fm_index_env + o.fm_index_env * amp);
            }
            let mut l = 0.0;
            let mut r = 0.0;
            for k in 0..n_osc {
                let (ol, or) = match waves[k] {
                    Wave::String => {
                        let s = self.string[k].tick();
                        (s, s)
                    }
                    Wave::Modal => {
                        let s = self.modal[k].tick();
                        (s, s)
                    }
                    Wave::Grain => self.grains[k].tick(ring),
                    w => self.osc[k].render(w, &params[k]),
                };
                l += ol * levels[k];
                r += or * levels[k];
            }
            if noise_floor > 0.0 {
                let n = self.rng.bipolar() * noise_floor;
                l += n;
                r += n;
            }
            let c = SvfCoefs::lerp(old_coefs, new_coefs, t);
            l = self.filt[0].process(l, &c, kind);
            r = self.filt[1].process(r, &c, kind);
            if drive > 0.0 {
                l = saturate(l, drive);
                r = saturate(r, drive);
            }
            let gl = lerp(start_l, target_l, t) * amp;
            let gr = lerp(start_r, target_r, t) * amp;
            out[i * 2] += l * gl;
            out[i * 2 + 1] += r * gr;
        }
        if !alive {
            self.active = false;
        }
        alive
    }
}

/// Resolve `FilterKind` display for debugging/UI.
pub fn filter_kind_name(k: FilterKind) -> &'static str {
    match k {
        FilterKind::Off => "off",
        FilterKind::Lp => "lp",
        FilterKind::Hp => "hp",
        FilterKind::Bp => "bp",
    }
}
