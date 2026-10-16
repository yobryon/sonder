//! Observers of the synth command stream (MIDI out, OSC out, Ableton Link, logging).

use sonder_core::{EngineSnapshot, SynthCommand, UserCommand};

/// Something a sink asks the control loop to do (Ableton Link following a peer's tempo).
#[derive(Debug, Clone, PartialEq)]
pub enum SinkRequest {
    /// Apply this as if the user had sent it.
    Command(UserCommand),
    /// Move the transport's position within the bar to this many beats (0 ≤ x < beats per
    /// bar), by the smallest signed shift. Nothing is replayed or fired by the jump itself.
    SetBarPhase(f64),
}

/// Anything that wants a copy of every synth command the composer emits.
pub trait CommandSink: Send {
    fn on_command(&mut self, cmd: &SynthCommand);
    /// Called once per control tick after all of that tick's commands.
    fn flush(&mut self) {}
    /// Called whenever the control loop publishes a snapshot (every other tick), so a sink
    /// can forward signals, harmony, form and transport without the composer knowing.
    fn on_snapshot(&mut self, _snap: &EngineSnapshot) {}
    /// Called for every user command before the composer sees it.
    fn on_user_command(&mut self, _cmd: &UserCommand) {}
    /// Called at the start of every tick; push anything the sink wants applied.
    fn drain_requests(&mut self, _out: &mut Vec<SinkRequest>) {}
}
