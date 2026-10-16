//! OSC out: mirrors the composer's commands and the engine's state to a UDP target
//! (TouchDesigner, Max/MSP, SuperCollider, Processing, a lighting desk…).
//!
//! Address space (see `docs/OSC.md`):
//!
//! | address | arguments |
//! |---|---|
//! | `/sonder/note/on` | `layer:i midi:f velocity:f pan:f` |
//! | `/sonder/note/off` | `layer:i midi:f` |
//! | `/sonder/drum` | `layer:i name:s velocity:f pan:f tone:f` |
//! | `/sonder/layer/<i>/<param>` | `value:f` (`gain pan cutoff resonance reverb delay texture mute`) |
//! | `/sonder/tempo` | `bpm:f` |
//! | `/sonder/signal/<layer>/<name>` | `value:f` (every snapshot) |
//! | `/sonder/harmony` | `key:s mode:s chord:s` |
//! | `/sonder/form` | `section:s arc:f` |
//! | `/sonder/mood` | `energy:f tension:f brightness:f` |
//!
//! Everything a control tick produces goes out in one OSC bundle (split only when a
//! bundle would exceed a safe UDP payload). Note-offs carry the pitch of the note-on they
//! release, so a receiver never needs to track `NoteId`s.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

use rosc::{OscBundle, OscMessage, OscPacket, OscTime, OscType};
use sonder_core::{Drum, EngineSnapshot, LayerParam, NoteId, SynthCommand};

pub use crate::sink::CommandSink;

/// Keep each datagram comfortably under the Ethernet MTU.
const MAX_BUNDLE_BYTES: usize = 1400;

/// A UDP OSC sender.
pub struct OscOut {
    sock: UdpSocket,
    target: SocketAddr,
    pending: Vec<OscMessage>,
    /// note → (layer, midi pitch it was started with).
    sounding: HashMap<NoteId, (u16, f32)>,
    /// Signals already published once (so a signal that disappears is sent as 0 once).
    last_signals: Vec<Vec<String>>,
}

impl OscOut {
    /// Resolve `host:port` and bind a socket for it.
    pub fn connect(target: &str) -> anyhow::Result<Self> {
        let target = target
            .to_socket_addrs()
            .map_err(|e| anyhow::anyhow!("--osc `{target}`: {e}"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("--osc `{target}` resolves to no address"))?;
        let bind: SocketAddr = if target.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
        let sock = UdpSocket::bind(bind)?;
        Ok(Self { sock, target, pending: Vec::new(), sounding: HashMap::new(), last_signals: Vec::new() })
    }

    pub fn target(&self) -> SocketAddr {
        self.target
    }

    fn push(&mut self, addr: impl Into<String>, args: Vec<OscType>) {
        self.pending.push(OscMessage { addr: addr.into(), args });
    }

    /// Send everything pending as one bundle (or several if it would not fit a datagram).
    fn send_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let msgs = std::mem::take(&mut self.pending);
        let mut content: Vec<OscPacket> = Vec::new();
        let mut size = 16; // bundle header + timetag
        for m in msgs {
            let len = rosc::encoder::encode(&OscPacket::Message(m.clone())).map(|b| b.len() + 4).unwrap_or(0);
            if !content.is_empty() && size + len > MAX_BUNDLE_BYTES {
                self.send_bundle(std::mem::take(&mut content));
                size = 16;
            }
            size += len;
            content.push(OscPacket::Message(m));
        }
        self.send_bundle(content);
    }

    fn send_bundle(&self, content: Vec<OscPacket>) {
        if content.is_empty() {
            return;
        }
        let bundle = OscPacket::Bundle(OscBundle { timetag: OscTime { seconds: 0, fractional: 1 }, content });
        match rosc::encoder::encode(&bundle) {
            Ok(bytes) => {
                if let Err(e) = self.sock.send_to(&bytes, self.target) {
                    tracing::debug!("osc send: {e}");
                }
            }
            Err(e) => tracing::debug!("osc encode: {e}"),
        }
    }
}

fn drum_name(d: Drum) -> &'static str {
    match d {
        Drum::Kick => "kick",
        Drum::Snare => "snare",
        Drum::Hat => "hat",
        Drum::Click => "click",
        Drum::Noise => "noise",
        Drum::Tom => "tom",
        Drum::Rim => "rim",
    }
}

fn param_name_value(p: &LayerParam) -> (&'static str, f32) {
    match p {
        LayerParam::Gain(v) => ("gain", *v),
        LayerParam::Pan(v) => ("pan", *v),
        LayerParam::CutoffOctaves(v) => ("cutoff", *v),
        LayerParam::Resonance(v) => ("resonance", *v),
        LayerParam::ReverbSend(v) => ("reverb", *v),
        LayerParam::DelaySend(v) => ("delay", *v),
        LayerParam::Texture(v) => ("texture", *v),
        LayerParam::Mute(m) => ("mute", if *m { 1.0 } else { 0.0 }),
    }
}

fn midi_of(hz: f32) -> f32 {
    sonder_core::hz_to_midi(hz as f64) as f32
}

impl CommandSink for OscOut {
    fn on_command(&mut self, cmd: &SynthCommand) {
        match cmd {
            SynthCommand::NoteOn { layer, note, pitch_hz, velocity, pan } => {
                let midi = midi_of(*pitch_hz);
                self.sounding.insert(*note, (layer.0, midi));
                self.push("/sonder/note/on", vec![OscType::Int(layer.0 as i32), OscType::Float(midi), OscType::Float(*velocity), OscType::Float(*pan)]);
            }
            SynthCommand::NoteOff { layer, note } => {
                if let Some((_, midi)) = self.sounding.remove(note) {
                    self.push("/sonder/note/off", vec![OscType::Int(layer.0 as i32), OscType::Float(midi)]);
                }
            }
            SynthCommand::NotePitch { layer, note, pitch_hz } => {
                if let Some((_, midi)) = self.sounding.get_mut(note) {
                    *midi = midi_of(*pitch_hz);
                    let midi = *midi;
                    self.push("/sonder/note/pitch", vec![OscType::Int(layer.0 as i32), OscType::Float(midi)]);
                }
            }
            SynthCommand::Drum { layer, drum, velocity, pan, tone } => {
                self.push(
                    "/sonder/drum",
                    vec![OscType::Int(layer.0 as i32), OscType::String(drum_name(*drum).into()), OscType::Float(*velocity), OscType::Float(*pan), OscType::Float(*tone)],
                );
            }
            SynthCommand::AllNotesOff { layer } => {
                let mut off: Vec<f32> = Vec::new();
                self.sounding.retain(|_, (l, midi)| {
                    let mine = *l == layer.0;
                    if mine {
                        off.push(*midi);
                    }
                    !mine
                });
                for midi in off {
                    self.push("/sonder/note/off", vec![OscType::Int(layer.0 as i32), OscType::Float(midi)]);
                }
            }
            SynthCommand::SetLayerParam { layer, param } => {
                let (name, v) = param_name_value(param);
                self.push(format!("/sonder/layer/{}/{name}", layer.0), vec![OscType::Float(v)]);
            }
            SynthCommand::SetTempo { bpm } => self.push("/sonder/tempo", vec![OscType::Float(*bpm)]),
            SynthCommand::Panic => {
                self.sounding.clear();
                self.push("/sonder/panic", vec![]);
            }
            SynthCommand::SetInstrument { .. } | SynthCommand::SetEffects(_) | SynthCommand::AudioChunk { .. } => {}
        }
    }

    fn flush(&mut self) {
        self.send_pending();
    }

    fn on_snapshot(&mut self, snap: &EngineSnapshot) {
        if self.last_signals.len() < snap.layers.len() {
            self.last_signals.resize(snap.layers.len(), Vec::new());
        }
        for (i, layer) in snap.layers.iter().enumerate() {
            // Signals that vanished (scene reload) go out once as 0.
            let gone: Vec<String> = self.last_signals[i].iter().filter(|n| !layer.signals.contains_key(*n)).cloned().collect();
            for name in gone {
                self.push(format!("/sonder/signal/{i}/{name}"), vec![OscType::Float(0.0)]);
            }
            for (name, v) in &layer.signals {
                self.push(format!("/sonder/signal/{i}/{name}"), vec![OscType::Float(*v as f32)]);
            }
            self.last_signals[i] = layer.signals.keys().cloned().collect();
        }
        let h = &snap.harmony;
        self.push("/sonder/harmony", vec![OscType::String(h.key_name.clone()), OscType::String(h.mode.clone()), OscType::String(h.chord_name.clone())]);
        self.push("/sonder/form", vec![OscType::String(snap.form.section.clone()), OscType::Float(snap.form.arc as f32)]);
        self.push("/sonder/mood", vec![OscType::Float(snap.mood.energy as f32), OscType::Float(snap.mood.tension as f32), OscType::Float(snap.mood.brightness as f32)]);
        self.push("/sonder/transport", vec![OscType::Float(snap.transport.bpm as f32), OscType::Int(snap.transport.bar as i32), OscType::Int(snap.transport.beat as i32), OscType::Float(snap.transport.phase as f32)]);
        self.send_pending();
    }
}

impl Drop for OscOut {
    /// Release every sounding note so a receiver is never left droning.
    fn drop(&mut self) {
        let sounding: Vec<(u16, f32)> = self.sounding.drain().map(|(_, v)| v).collect();
        for (layer, midi) in sounding {
            self.push("/sonder/note/off", vec![OscType::Int(layer as i32), OscType::Float(midi)]);
        }
        self.send_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::{HarmonySnapshot, LayerId, LayerSnapshot, Mood};
    use std::time::Duration;

    fn receiver() -> (UdpSocket, SocketAddr) {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let addr = rx.local_addr().unwrap();
        (rx, addr)
    }

    fn recv_messages(rx: &UdpSocket) -> Vec<OscMessage> {
        let mut buf = vec![0u8; 65_536];
        let n = rx.recv(&mut buf).expect("a datagram");
        let (_, packet) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        let mut out = Vec::new();
        fn walk(p: OscPacket, out: &mut Vec<OscMessage>) {
            match p {
                OscPacket::Message(m) => out.push(m),
                OscPacket::Bundle(b) => b.content.into_iter().for_each(|c| walk(c, out)),
            }
        }
        walk(packet, &mut out);
        out
    }

    #[test]
    fn commands_become_one_bundle_per_tick() {
        let (rx, addr) = receiver();
        let mut osc = OscOut::connect(&addr.to_string()).unwrap();
        osc.on_command(&SynthCommand::NoteOn { layer: LayerId(2), note: NoteId(7), pitch_hz: 440.0, velocity: 0.8, pan: 0.25 });
        osc.on_command(&SynthCommand::Drum { layer: LayerId(1), drum: Drum::Snare, velocity: 0.9, pan: 0.5, tone: 0.3 });
        osc.on_command(&SynthCommand::SetLayerParam { layer: LayerId(0), param: LayerParam::CutoffOctaves(0.5) });
        osc.on_command(&SynthCommand::SetTempo { bpm: 92.0 });
        osc.on_command(&SynthCommand::NoteOff { layer: LayerId(2), note: NoteId(7) });
        osc.flush();
        let msgs = recv_messages(&rx);
        let addrs: Vec<&str> = msgs.iter().map(|m| m.addr.as_str()).collect();
        assert_eq!(addrs, ["/sonder/note/on", "/sonder/drum", "/sonder/layer/0/cutoff", "/sonder/tempo", "/sonder/note/off"]);
        assert_eq!(msgs[0].args, vec![OscType::Int(2), OscType::Float(69.0), OscType::Float(0.8), OscType::Float(0.25)]);
        assert_eq!(msgs[1].args[1], OscType::String("snare".into()));
        assert_eq!(msgs[4].args, vec![OscType::Int(2), OscType::Float(69.0)], "note-off carries the note-on's pitch");
        // Nothing pending → nothing sent.
        osc.flush();
        assert!(rx.recv(&mut [0u8; 16]).is_err());
    }

    #[test]
    fn snapshot_publishes_signals_harmony_form_and_mood() {
        let (rx, addr) = receiver();
        let mut osc = OscOut::connect(&addr.to_string()).unwrap();
        let mut snap = EngineSnapshot::default();
        let mut layer = LayerSnapshot { id: LayerId(0), name: "traffic".into(), ..Default::default() };
        layer.signals.insert("flow".into(), 0.42);
        snap.layers.push(layer);
        snap.harmony = HarmonySnapshot { key_name: "D".into(), mode: "lydian".into(), chord_name: "Dmaj7".into(), ..Default::default() };
        snap.form.section = "build".into();
        snap.form.arc = 0.3;
        snap.mood = Mood { energy: 0.5, tension: 0.2, brightness: 0.7 };
        osc.on_snapshot(&snap);
        let msgs = recv_messages(&rx);
        let find = |a: &str| msgs.iter().find(|m| m.addr == a).unwrap_or_else(|| panic!("no {a} in {msgs:?}"));
        assert_eq!(find("/sonder/signal/0/flow").args, vec![OscType::Float(0.42)]);
        assert_eq!(find("/sonder/harmony").args, vec![OscType::String("D".into()), OscType::String("lydian".into()), OscType::String("Dmaj7".into())]);
        assert_eq!(find("/sonder/form").args, vec![OscType::String("build".into()), OscType::Float(0.3)]);
        assert_eq!(find("/sonder/mood").args.len(), 3);
        assert!(find("/sonder/transport").args.len() == 4);
    }

    #[test]
    fn big_ticks_split_into_datagram_sized_bundles() {
        let (rx, addr) = receiver();
        let mut osc = OscOut::connect(&addr.to_string()).unwrap();
        for i in 0..200 {
            osc.on_command(&SynthCommand::SetLayerParam { layer: LayerId(i % 8), param: LayerParam::Gain(0.5) });
        }
        osc.flush();
        let mut total = 0;
        let mut buf = vec![0u8; 65_536];
        while let Ok(n) = rx.recv(&mut buf) {
            assert!(n <= MAX_BUNDLE_BYTES + 64, "datagram of {n} bytes");
            let (_, p) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
            if let OscPacket::Bundle(b) = p {
                total += b.content.len();
            }
            if total >= 200 {
                break;
            }
        }
        assert_eq!(total, 200);
    }
}
