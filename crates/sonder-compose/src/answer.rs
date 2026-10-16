//! Call-and-response: a salient event on one layer is answered a beat later by
//! a melodic layer with a short figure that lands on a chord tone near the
//! call's register. Answers carry `reason = "answer"` and the call's cause.

use std::collections::BTreeMap;

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sonder_core::{LayerEvent, LayerId, NoteCause, Role, Seconds};
use sonder_theory::Quantizer;

use crate::conductor::Harmony;
use crate::generators::register_window;
use crate::notes::{cause_of, NoteSink};
use crate::transport::Transport;

/// `NoteSnapshot.reason` for answer notes.
pub const ANSWER_REASON: &str = "answer";
/// Intensity at or above which an event is a call on its own.
pub const SALIENT_INTENSITY: f64 = 0.7;
/// An event name not heard for this long on its layer is a call when it returns.
pub const NOVELTY_S: f64 = 30.0;
/// Spacing of the notes within an answer figure, in beats.
const FIGURE_STEP: f64 = 0.25;
/// Length of the figure's last note, in beats.
const FINAL_BEATS: f64 = 1.5;

/// An answer waiting for its beat.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub layer: LayerId,
    pub at_beat: f64,
    /// Register of the call, 0..1 within the answering layer's window.
    pub pitch01: f64,
    pub intensity: f64,
    pub notes: usize,
    /// Approach the final chord tone from below (else above).
    pub from_below: bool,
    pub cause: NoteCause,
    /// Echo a human phrase: land near this midi note, reproducing this interval
    /// (semitones) into it. Overrides `pitch01` / `notes` / `from_below`.
    pub echo: Option<Echo>,
}

/// What an answer to a human phrase echoes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Echo {
    /// Where the phrase ended.
    pub midi: f64,
    /// The phrase's last interval, in semitones (second-to-last → last note).
    pub interval: i32,
}

#[derive(Debug)]
pub struct Answers {
    rng: ChaCha8Rng,
    /// Last time each (layer, event name) was heard.
    last_seen: BTreeMap<(LayerId, String), Seconds>,
    pending: Vec<Pending>,
    /// Bar of the latest answer scheduled per layer (one per layer per bar).
    last_bar: BTreeMap<LayerId, u64>,
    /// Beat until which each answering layer should keep its own melody quiet.
    rest_until: BTreeMap<LayerId, f64>,
    /// Event name of the latest call, for the form note.
    last_call: String,
}

impl Answers {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed ^ 0x2c6f_a1d9_8b3e_5571),
            last_seen: BTreeMap::new(),
            pending: Vec::new(),
            last_bar: BTreeMap::new(),
            rest_until: BTreeMap::new(),
            last_call: String::new(),
        }
    }

    /// Is this event a call? Loud (intensity ≥ 0.7) or the first of its name
    /// on its layer in 30 s. Always records the sighting.
    pub fn is_salient(&mut self, ev: &LayerEvent) -> bool {
        let key = (ev.layer, ev.name.clone());
        let novel = match self.last_seen.get(&key) {
            Some(t) => ev.at - t >= NOVELTY_S,
            None => true,
        };
        self.last_seen.insert(key, ev.at);
        let intensity = if ev.intensity.is_finite() {
            ev.intensity
        } else {
            0.0
        };
        intensity >= SALIENT_INTENSITY || novel
    }

    /// Schedule an answer to `ev` on `layer` one beat later (on the half-beat
    /// grid). Returns false when that layer already answers this bar.
    pub fn schedule(
        &mut self,
        ev: &LayerEvent,
        pitch01: f64,
        layer: LayerId,
        transport: &Transport,
    ) -> bool {
        let at_beat = Transport::next_grid(transport.beats() + 1.0, 0.5);
        let bar = (at_beat / transport.beats_per_bar().max(1) as f64).floor() as u64;
        if self.last_bar.get(&layer) == Some(&bar) {
            return false;
        }
        self.last_bar.insert(layer, bar);
        let intensity = if ev.intensity.is_finite() {
            ev.intensity.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let notes = 1 + self.rng.random_range(0..3);
        let from_below = self.rng.random::<f64>() < 0.6;
        let end = at_beat + (notes - 1) as f64 * FIGURE_STEP + FINAL_BEATS;
        let r = self.rest_until.entry(layer).or_insert(0.0);
        *r = r.max(end);
        self.last_call = ev.name.clone();
        self.pending.push(Pending {
            layer,
            at_beat,
            pitch01: if pitch01.is_finite() {
                pitch01.clamp(0.0, 1.0)
            } else {
                0.5
            },
            intensity,
            notes,
            from_below,
            cause: cause_of(ev),
            echo: None,
        });
        true
    }

    /// Schedule an answer that echoes a human phrase: a beat later, a two-note
    /// figure whose last interval is the phrase's, landing on a chord tone near
    /// where the phrase ended. Same one-per-layer-per-bar rule as [`Answers::schedule`].
    pub fn schedule_echo(
        &mut self,
        ev: &LayerEvent,
        echo: Echo,
        layer: LayerId,
        transport: &Transport,
    ) -> bool {
        let at_beat = Transport::next_grid(transport.beats() + 1.0, 0.5);
        let bar = (at_beat / transport.beats_per_bar().max(1) as f64).floor() as u64;
        if self.last_bar.get(&layer) == Some(&bar) {
            return false;
        }
        self.last_bar.insert(layer, bar);
        let intensity = if ev.intensity.is_finite() {
            ev.intensity.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let end = at_beat + 2.0 * FIGURE_STEP + FINAL_BEATS;
        let r = self.rest_until.entry(layer).or_insert(0.0);
        *r = r.max(end);
        self.last_call = ev.name.clone();
        self.pending.push(Pending {
            layer,
            at_beat,
            pitch01: 0.5,
            intensity,
            notes: 2,
            from_below: echo.interval >= 0,
            cause: cause_of(ev),
            echo: Some(if echo.midi.is_finite() { echo } else { Echo { midi: 60.0, ..echo } }),
        });
        true
    }

    /// Beat until which `layer`'s own melody should rest for an answer.
    pub fn rest_until(&self, layer: LayerId) -> f64 {
        self.rest_until.get(&layer).copied().unwrap_or(0.0)
    }

    /// Latest call's event name (empty if none yet).
    pub fn last_call(&self) -> &str {
        &self.last_call
    }

    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Take the answers for `layer` due within the next half beat.
    pub fn take_due(&mut self, layer: LayerId, beats: f64) -> Vec<Pending> {
        let mut due = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].layer == layer && self.pending[i].at_beat <= beats + 0.5 {
                due.push(self.pending.remove(i));
            } else {
                i += 1;
            }
        }
        due
    }

    /// Drop everything scheduled for a layer (it was muted).
    pub fn forget_layer(&mut self, layer: LayerId) {
        self.pending.retain(|p| p.layer != layer);
        self.rest_until.remove(&layer);
    }

    /// Play an answer figure: `notes - 1` scale steps approaching a chord tone
    /// near the call's register, then the chord tone itself.
    pub fn play(
        p: &Pending,
        role: Role,
        register: f64,
        harmony: &Harmony,
        transport: &Transport,
        sink: &mut NoteSink<'_>,
    ) {
        let (lo, hi) = register_window(role, register);
        let target = match p.echo {
            Some(e) => e.midi.clamp(lo as f64, hi as f64),
            None => lo as f64 + (hi - lo) as f64 * p.pitch01,
        };
        let pcs = if harmony.chord_pcs.is_empty() {
            &harmony.scale_pcs
        } else {
            &harmony.chord_pcs
        };
        let final_midi = Quantizer::notes_in_window(pcs, lo, hi)
            .into_iter()
            .min_by_key(|&m| (((m as f64 - target).abs() * 100.0) as i64, m))
            .unwrap_or_else(|| Quantizer::nearest_in(pcs, target));
        let spb = transport.seconds_per_beat();
        let vel = 0.35 + 0.5 * p.intensity;
        if let Some(e) = p.echo {
            // Two notes: the phrase's last interval, in the scale, into the chord tone.
            let first = Quantizer::nearest_in(&harmony.scale_pcs, (final_midi - e.interval) as f64);
            let first = if first == final_midi && e.interval != 0 {
                Quantizer::nearest_in(&harmony.scale_pcs, (final_midi - e.interval.signum() * 2) as f64)
            } else {
                first
            };
            sink.play_as(
                first as f64,
                vel * 0.8,
                0.5,
                Some(2.0 * FIGURE_STEP * 0.9 * spb),
                transport.delay_to(p.at_beat),
                ANSWER_REASON,
                Some(p.cause.clone()),
            );
            sink.play_as(
                final_midi as f64,
                vel,
                0.5,
                Some(FINAL_BEATS * spb),
                transport.delay_to(p.at_beat + 2.0 * FIGURE_STEP),
                ANSWER_REASON,
                Some(p.cause.clone()),
            );
            return;
        }
        let n = p.notes.clamp(1, 3);
        let final_idx = harmony.scale.index_of_midi(final_midi);
        for k in 0..n {
            let steps_away = (n - 1 - k) as i32;
            let midi = if steps_away == 0 {
                final_midi
            } else {
                match final_idx {
                    Some(fi) => harmony.scale.midi_from_index(if p.from_below {
                        fi - steps_away
                    } else {
                        fi + steps_away
                    }),
                    None => {
                        final_midi
                            + if p.from_below {
                                -steps_away
                            } else {
                                steps_away
                            }
                    }
                }
            };
            let beat = p.at_beat + k as f64 * FIGURE_STEP;
            let (dur, v) = if steps_away == 0 {
                (FINAL_BEATS * spb, vel)
            } else {
                (FIGURE_STEP * 0.9 * spb, vel * 0.8)
            };
            sink.play_as(
                midi as f64,
                v,
                0.5,
                Some(dur),
                transport.delay_to(beat),
                ANSWER_REASON,
                Some(p.cause.clone()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::ConductorConfig;

    fn ev(layer: u16, name: &str, at: f64, intensity: f64) -> LayerEvent {
        LayerEvent {
            layer: LayerId(layer),
            name: name.into(),
            at,
            intensity,
            attrs: BTreeMap::new(),
        }
    }

    #[test]
    fn salience_is_loudness_or_novelty() {
        let mut a = Answers::new(1);
        assert!(a.is_salient(&ev(0, "car", 0.0, 0.1)), "first of its name");
        assert!(!a.is_salient(&ev(0, "car", 1.0, 0.1)));
        assert!(a.is_salient(&ev(0, "car", 2.0, 0.9)), "loud");
        assert!(
            a.is_salient(&ev(1, "car", 3.0, 0.1)),
            "new on another layer"
        );
        assert!(!a.is_salient(&ev(0, "car", 20.0, 0.2)));
        assert!(a.is_salient(&ev(0, "car", 51.0, 0.2)), "quiet for 30 s");
        assert!(!a.is_salient(&ev(0, "car", f64::NAN, f64::NAN)));
    }

    #[test]
    fn one_answer_per_layer_per_bar_one_beat_later() {
        let mut a = Answers::new(1);
        let mut t = Transport::new(&ConductorConfig {
            tempo: 60.0,
            tempo_range: [60.0, 60.0],
            ..Default::default()
        });
        t.tick(1.3, 0.5); // beat 1.3
        assert!(a.schedule(&ev(0, "car", 1.3, 0.9), 0.5, LayerId(2), &t));
        assert!(
            !a.schedule(&ev(0, "car", 1.3, 0.9), 0.5, LayerId(2), &t),
            "rate limited"
        );
        assert!(
            a.schedule(&ev(0, "car", 1.3, 0.9), 0.5, LayerId(3), &t),
            "another layer may answer"
        );
        let p = &a.pending()[0];
        assert!((p.at_beat - 2.5).abs() < 1e-9, "{}", p.at_beat);
        assert_eq!(p.cause.event, "car");
        assert!(a.rest_until(LayerId(2)) > 2.5);
        assert_eq!(a.last_call(), "car");
        assert!(a.take_due(LayerId(2), 1.5).is_empty());
        let due = a.take_due(LayerId(2), 2.1);
        assert_eq!(due.len(), 1);
        assert_eq!(a.pending().len(), 1);
        a.forget_layer(LayerId(3));
        assert!(a.pending().is_empty());
    }
}
