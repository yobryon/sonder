//! `synthetic.walk` — N seeded random-walk channels in 0..1 plus optional periodic
//! events. Handy for testing mappings without any network.

use std::time::Duration;

use rand::RngExt;
use rand_chacha::ChaCha8Rng;
use sonder_core::{Seconds, SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, Attrs, Emitter};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

#[derive(Debug, Clone)]
pub struct WalkConfig {
    pub names: Vec<String>,
    pub rate_hz: f64,
    pub step: f64,
    pub event_every: Option<f64>,
}

impl WalkConfig {
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, SourceError> {
        let names = util::param_str_list(spec, "names")
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| vec!["a".into(), "b".into(), "c".into()]);
        let rate_hz = util::param_f64_in(spec, "rate_hz", 10.0, 0.1, 200.0)?;
        let step = util::param_f64_in(spec, "step", 0.05, 0.0, 1.0)?;
        let ev = util::param_f64(spec, "event_every", 0.0)?;
        let event_every = if ev > 0.0 && ev.is_finite() { Some(ev) } else { None };
        Ok(Self { names, rate_hz, step, event_every })
    }
}

/// Pure random-walk state, stepped by time.
#[derive(Debug, Clone)]
pub struct Walk {
    pub cfg: WalkConfig,
    rng: ChaCha8Rng,
    pub values: Vec<f64>,
    pub t: Seconds,
    next_tick: Seconds,
    next_event: Seconds,
    pub events: u64,
}

/// What a step produced.
#[derive(Debug, Default, Clone)]
pub struct WalkOutput {
    pub readings: Vec<(Seconds, Vec<(String, f64)>)>,
    pub events: Vec<(Seconds, u64)>,
}

impl Walk {
    pub fn new(cfg: WalkConfig, seed: u64, start: Seconds) -> Self {
        let mut rng = util::rng_from_seed(seed);
        let values = (0..cfg.names.len()).map(|_| rng.random_range(0.2..0.8)).collect();
        let next_event = start + cfg.event_every.unwrap_or(0.0);
        Self { cfg, rng, values, t: start, next_tick: start, next_event, events: 0 }
    }

    /// Advance to `t`, producing readings at `rate_hz` and events at `event_every`.
    pub fn advance_to(&mut self, t: Seconds) -> WalkOutput {
        let mut out = WalkOutput::default();
        let dt_tick = 1.0 / self.cfg.rate_hz;
        // Cap catch-up to 10 s worth of ticks.
        if t - self.next_tick > 10.0 {
            self.next_tick = t - 10.0;
        }
        let mut guard = 0;
        while self.next_tick <= t && guard < 10_000 {
            guard += 1;
            for v in &mut self.values {
                let d: f64 = self.rng.random_range(-1.0..1.0) * self.cfg.step;
                let mut nv = *v + d;
                // Reflect at the boundaries so the walk stays in 0..1.
                if nv < 0.0 {
                    nv = -nv;
                }
                if nv > 1.0 {
                    nv = 2.0 - nv;
                }
                *v = nv.clamp(0.0, 1.0);
            }
            let vals = self.cfg.names.iter().cloned().zip(self.values.iter().copied()).collect();
            out.readings.push((self.next_tick, vals));
            self.next_tick += dt_tick;
        }
        if let Some(every) = self.cfg.event_every {
            if t - self.next_event > 10.0 {
                self.next_event = t - 10.0;
            }
            while self.next_event <= t {
                self.events += 1;
                out.events.push((self.next_event, self.events));
                self.next_event += every;
            }
        }
        self.t = t;
        out
    }
}

struct WalkSource {
    cfg: WalkConfig,
    seed: Option<u64>,
}

impl Source for WalkSource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let seed = util::instance_seed(ctx.seed, ctx.id.as_str(), self.seed);
            let mut em = Emitter::new(ctx);
            let mut walk = Walk::new(self.cfg, seed, em.now());
            em.status(SourceState::Live, "walking").await;
            loop {
                let out = walk.advance_to(em.now());
                for (at, vals) in out.readings {
                    em.reading_at(at, vals).await;
                }
                for (at, n) in out.events {
                    let intensity = walk.values.first().copied().unwrap_or(0.5);
                    let attrs = Attrs::new().num("n", n as f64).num("value", intensity);
                    em.event_at(at, "tick", intensity, attrs.into_map()).await;
                }
                if !em.sleep(Duration::from_millis(50)).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "synthetic.walk",
        summary: "Seeded random-walk channels (0..1) plus optional periodic `tick` events",
        params: vec![
            ParamInfo { name: "names", summary: "Channel names", default: Some("[\"a\", \"b\", \"c\"]") },
            ParamInfo { name: "rate_hz", summary: "Readings per second", default: Some("10") },
            ParamInfo { name: "step", summary: "Max step per tick", default: Some("0.05") },
            ParamInfo { name: "event_every", summary: "Seconds between `tick` events (0 = none)", default: Some("0") },
            ParamInfo { name: "seed", summary: "RNG seed", default: None },
        ],
        channels: vec!["<names>"],
        events: vec!["tick"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    Ok(Box::new(WalkSource { cfg: WalkConfig::from_spec(spec)?, seed: util::param_seed(spec)? }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_stays_in_range_and_is_deterministic() {
        let cfg = WalkConfig { names: vec!["x".into(), "y".into()], rate_hz: 10.0, step: 0.2, event_every: Some(1.0) };
        let mut a = Walk::new(cfg.clone(), 5, 0.0);
        let mut b = Walk::new(cfg, 5, 0.0);
        let oa = a.advance_to(5.0);
        let ob = b.advance_to(5.0);
        assert_eq!(oa.readings.len(), ob.readings.len());
        assert_eq!(oa.readings.len(), 51);
        assert_eq!(oa.events.len(), 5);
        assert_eq!(a.values, b.values);
        for (_, vals) in &oa.readings {
            for (_, v) in vals {
                assert!((0.0..=1.0).contains(v));
            }
        }
    }
}
