//! MIDI out: mirrors the composer's `SynthCommand`s to an external synth / DAW.
//!
//! Layer `i` plays on MIDI channel `i` (skipping channel 10, which percussion layers
//! use with GM drum notes). Pitches are rounded to the nearest semitone — the composer
//! already quantizes to the scale, so only `snap = "none"` notes lose precision.
//! Layer parameters become CCs: gain→7, pan→10, cutoff→74, resonance→71, reverb→91,
//! delay→94, texture→1.

use std::collections::HashMap;

use sonder_core::{Drum, LayerId, LayerParam, NoteId, SynthCommand};

pub use crate::sink::CommandSink;

/// GM percussion channel (1-based 10 → 0-based 9).
const DRUM_CHANNEL: u8 = 9;

/// A MIDI output connected to a port.
pub struct MidiOut {
    conn: midir::MidiOutputConnection,
    /// note → (channel, key)
    sounding: HashMap<NoteId, (u8, u8)>,
    /// Which layers are percussion (use the drum channel).
    perc_layers: Vec<bool>,
    port_name: String,
}

impl MidiOut {
    /// List available output port names.
    pub fn ports() -> anyhow::Result<Vec<String>> {
        let out = midir::MidiOutput::new("sonder")?;
        Ok(out.ports().iter().filter_map(|p| out.port_name(p).ok()).collect())
    }

    /// Open a port by (case-insensitive) substring match, or create a virtual port
    /// named "Sonder" when `port` is `"virtual"` (Linux/macOS).
    pub fn open(port: &str, perc_layers: Vec<bool>) -> anyhow::Result<Self> {
        let out = midir::MidiOutput::new("sonder")?;
        if port.eq_ignore_ascii_case("virtual") {
            #[cfg(unix)]
            {
                use midir::os::unix::VirtualOutput;
                let conn = out.create_virtual("Sonder").map_err(|e| anyhow::anyhow!("virtual midi port: {e}"))?;
                return Ok(Self { conn, sounding: HashMap::new(), perc_layers, port_name: "Sonder (virtual)".into() });
            }
            #[cfg(not(unix))]
            anyhow::bail!("virtual MIDI ports are not supported on this platform");
        }
        let ports = out.ports();
        let needle = port.to_ascii_lowercase();
        let found = ports.iter().find(|p| out.port_name(p).map(|n| n.to_ascii_lowercase().contains(&needle)).unwrap_or(false));
        let Some(p) = found else {
            let names: Vec<String> = ports.iter().filter_map(|p| out.port_name(p).ok()).collect();
            anyhow::bail!("no MIDI output port matching `{port}` (available: {}; or use `virtual`)", names.join(", "));
        };
        let port_name = out.port_name(p).unwrap_or_default();
        let conn = out.connect(p, "sonder").map_err(|e| anyhow::anyhow!("connecting midi: {e}"))?;
        Ok(Self { conn, sounding: HashMap::new(), perc_layers, port_name })
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    fn channel(&self, layer: LayerId) -> u8 {
        if self.perc_layers.get(layer.0 as usize).copied().unwrap_or(false) {
            return DRUM_CHANNEL;
        }
        // Spread melodic layers over 0..16 skipping the drum channel.
        let mut ch = layer.0 % 15;
        if ch >= DRUM_CHANNEL as u16 {
            ch += 1;
        }
        ch as u8
    }

    fn send(&mut self, msg: &[u8]) {
        if let Err(e) = self.conn.send(msg) {
            tracing::debug!("midi send: {e}");
        }
    }

    /// Silence everything (all-notes-off on every channel).
    pub fn panic(&mut self) {
        for ch in 0..16u8 {
            self.send(&[0xB0 | ch, 123, 0]);
        }
        self.sounding.clear();
    }
}

fn to7(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

fn hz_to_key(hz: f32) -> u8 {
    sonder_core::hz_to_midi(hz as f64).round().clamp(0.0, 127.0) as u8
}

fn drum_key(d: Drum) -> u8 {
    match d {
        Drum::Kick => 36,
        Drum::Snare => 38,
        Drum::Hat => 42,
        Drum::Click => 37,
        Drum::Noise => 39,
        Drum::Tom => 45,
        Drum::Rim => 37,
    }
}

impl CommandSink for MidiOut {
    fn on_command(&mut self, cmd: &SynthCommand) {
        match cmd {
            SynthCommand::NoteOn { layer, note, pitch_hz, velocity, .. } => {
                let ch = self.channel(*layer);
                let key = hz_to_key(*pitch_hz);
                self.send(&[0x90 | ch, key, to7(*velocity).max(1)]);
                self.sounding.insert(*note, (ch, key));
            }
            SynthCommand::NoteOff { note, .. } => {
                if let Some((ch, key)) = self.sounding.remove(note) {
                    self.send(&[0x80 | ch, key, 0]);
                }
            }
            SynthCommand::NotePitch { note, pitch_hz, .. } => {
                if let Some(&(ch, key)) = self.sounding.get(note) {
                    let new_key = hz_to_key(*pitch_hz);
                    if new_key != key {
                        self.send(&[0x90 | ch, new_key, 80]);
                        self.send(&[0x80 | ch, key, 0]);
                        self.sounding.insert(*note, (ch, new_key));
                    }
                }
            }
            SynthCommand::Drum { drum, velocity, .. } => {
                let key = drum_key(*drum);
                self.send(&[0x90 | DRUM_CHANNEL, key, to7(*velocity).max(1)]);
                self.send(&[0x80 | DRUM_CHANNEL, key, 0]);
            }
            SynthCommand::AllNotesOff { layer } => {
                let ch = self.channel(*layer);
                self.sounding.retain(|_, (c, _)| *c != ch);
                self.send(&[0xB0 | ch, 123, 0]);
            }
            SynthCommand::SetLayerParam { layer, param } => {
                let ch = self.channel(*layer);
                let (cc, v) = match param {
                    LayerParam::Gain(g) => (7, to7(*g)),
                    LayerParam::Pan(p) => (10, to7(*p)),
                    LayerParam::CutoffOctaves(o) => (74, to7((o + 2.0) / 4.0)),
                    LayerParam::Resonance(r) => (71, to7(*r)),
                    LayerParam::ReverbSend(s) => (91, to7(*s)),
                    LayerParam::DelaySend(s) => (94, to7(*s)),
                    LayerParam::Texture(t) => (1, to7(*t)),
                    LayerParam::Mute(m) => {
                        if *m {
                            self.send(&[0xB0 | ch, 123, 0]);
                        }
                        return;
                    }
                };
                self.send(&[0xB0 | ch, cc, v]);
            }
            SynthCommand::Panic => self.panic(),
            SynthCommand::SetInstrument { .. } | SynthCommand::SetTempo { .. } | SynthCommand::SetEffects(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversions() {
        assert_eq!(hz_to_key(440.0), 69);
        assert_eq!(hz_to_key(261.63), 60);
        assert_eq!(to7(1.0), 127);
        assert_eq!(to7(0.0), 0);
        assert_eq!(drum_key(Drum::Kick), 36);
    }
}
