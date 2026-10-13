//! Observers of the synth command stream (MIDI out, logging).

use sonder_core::SynthCommand;

/// Anything that wants a copy of every synth command the composer emits.
pub trait CommandSink: Send {
    fn on_command(&mut self, cmd: &SynthCommand);
    /// Called once per control tick after all of that tick's commands.
    fn flush(&mut self) {}
}
