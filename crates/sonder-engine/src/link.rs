//! Ableton Link: share tempo and beat phase with Link peers on the LAN (Live, Reason,
//! Traktor, Reaper via plugin, iOS apps, other Sonders).
//!
//! Who leads: with no peers in the session Sonder's transport is the source of truth and
//! its tempo and bar phase are pushed to Link. Once peers exist, Link's session tempo is
//! followed — every control tick the difference is nudged away when it exceeds
//! [`TEMPO_TOLERANCE`] — and when the user nudges Sonder's tempo, the new value is
//! proposed to the session so everyone moves. Phase alignment is coarse: at each of
//! Sonder's bar boundaries the transport's bar phase is snapped to Link's (quantum = beats
//! per bar); between boundaries both clocks run at the same tempo and stay aligned.
//!
//! Needs cmake, a C++ compiler and libclang to build (`rusty_link` compiles the official
//! Link library and bindgens its C wrapper), which is why the `link` feature is off by default.

use rusty_link::{AblLink, SessionState};
use sonder_core::{EngineSnapshot, SynthCommand, UserCommand};

use crate::sink::{CommandSink, SinkRequest};

/// Tempo differences below this (bpm) are left alone.
pub const TEMPO_TOLERANCE: f64 = 0.05;
/// Phase differences below this (beats) are left alone.
const PHASE_TOLERANCE: f64 = 0.03;

pub struct LinkSync {
    link: AblLink,
    state: SessionState,
    /// Our tempo as of the last snapshot.
    bpm: f64,
    /// Our position as of the last snapshot.
    beats: f64,
    beats_per_bar: f64,
    last_bar: Option<u64>,
    /// Tempo the user just asked for (our bpm + nudge), to propose to the session.
    propose: Option<f64>,
    requests: Vec<SinkRequest>,
    peers: u64,
}

impl LinkSync {
    /// Join (or start) the Link session at this tempo.
    pub fn start(bpm: f64) -> Self {
        let link = AblLink::new(bpm.clamp(20.0, 999.0));
        link.enable(true);
        Self { link, state: SessionState::new(), bpm, beats: 0.0, beats_per_bar: 4.0, last_bar: None, propose: None, requests: Vec::new(), peers: 0 }
    }

    pub fn num_peers(&self) -> u64 {
        self.link.num_peers()
    }

    /// Session tempo as Link currently has it.
    pub fn session_tempo(&mut self) -> f64 {
        self.link.capture_app_session_state(&mut self.state);
        self.state.tempo()
    }

    /// Push a tempo to the session now.
    pub fn set_session_tempo(&mut self, bpm: f64) {
        self.link.capture_app_session_state(&mut self.state);
        self.state.set_tempo(bpm.clamp(20.0, 999.0), self.link.clock_micros());
        self.link.commit_app_session_state(&self.state);
    }

    /// Reconcile our transport (from the snapshot) with the session. Pure bookkeeping
    /// apart from Link calls; the resulting requests go to the control loop next tick.
    fn reconcile(&mut self, snap: &EngineSnapshot) {
        let t = &snap.transport;
        self.bpm = t.bpm;
        self.beats = t.beats;
        self.beats_per_bar = t.beats_per_bar.max(1) as f64;
        self.peers = self.link.num_peers();
        let now = self.link.clock_micros();
        self.link.capture_app_session_state(&mut self.state);
        let mut dirty = false;

        if let Some(bpm) = self.propose.take() {
            self.state.set_tempo(bpm.clamp(20.0, 999.0), now);
            dirty = true;
        }
        let session_bpm = self.state.tempo();
        let bar_started = self.last_bar.is_some_and(|b| b != t.bar);
        self.last_bar = Some(t.bar);

        if self.peers == 0 {
            // Alone: Link mirrors us.
            if (session_bpm - self.bpm).abs() > TEMPO_TOLERANCE {
                self.state.set_tempo(self.bpm.clamp(20.0, 999.0), now);
                dirty = true;
            }
            if bar_started {
                self.state.request_beat_at_time(self.beats, now, self.beats_per_bar);
                dirty = true;
            }
        } else {
            // Peers: we follow the session.
            let delta = session_bpm - self.bpm;
            if delta.abs() > TEMPO_TOLERANCE {
                self.requests.push(SinkRequest::Command(UserCommand::NudgeTempo { delta }));
            }
            if bar_started {
                let phase = self.state.phase_at_time(now, self.beats_per_bar);
                let ours = self.beats.rem_euclid(self.beats_per_bar);
                let mut diff = phase - ours;
                if diff > self.beats_per_bar / 2.0 {
                    diff -= self.beats_per_bar;
                } else if diff < -self.beats_per_bar / 2.0 {
                    diff += self.beats_per_bar;
                }
                if diff.abs() > PHASE_TOLERANCE {
                    self.requests.push(SinkRequest::SetBarPhase(phase));
                }
            }
        }
        if dirty {
            self.link.commit_app_session_state(&self.state);
        }
    }
}

impl CommandSink for LinkSync {
    fn on_command(&mut self, _cmd: &SynthCommand) {}

    fn on_user_command(&mut self, cmd: &UserCommand) {
        if let UserCommand::NudgeTempo { delta } = cmd {
            if self.peers > 0 {
                self.propose = Some(self.bpm + delta);
            }
        }
    }

    fn on_snapshot(&mut self, snap: &EngineSnapshot) {
        self.reconcile(snap);
    }

    fn drain_requests(&mut self, out: &mut Vec<SinkRequest>) {
        out.append(&mut self.requests);
    }
}

impl Drop for LinkSync {
    fn drop(&mut self) {
        self.link.enable(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonder_core::TransportPos;

    /// Link on a loopback-only CI box has no peers; the parts that need none still run.
    #[test]
    fn tempo_round_trips_and_follows_our_transport_when_alone() {
        let mut link = LinkSync::start(120.0);
        link.set_session_tempo(97.5);
        assert!((link.session_tempo() - 97.5).abs() < 1e-6);

        let mut snap = EngineSnapshot { transport: TransportPos { bpm: 64.0, beats_per_bar: 4, bar: 0, beat: 0, phase: 0.0, beats: 0.0 }, ..Default::default() };
        link.on_snapshot(&snap);
        if link.num_peers() == 0 {
            assert!((link.session_tempo() - 64.0).abs() < 1e-6, "alone, Link mirrors our tempo");
            let mut reqs = Vec::new();
            link.drain_requests(&mut reqs);
            assert!(reqs.is_empty(), "alone, nothing to follow");
        }
        // A bar boundary alone: beat 4 is requested at "now"; nothing to assert beyond not panicking.
        snap.transport.bar = 1;
        snap.transport.beats = 4.0;
        link.on_snapshot(&snap);
        drop(link);
    }
}
