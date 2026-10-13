//! `synthetic.freeway` — the founding image: a camera above a freeway at night.
//!
//! Cars enter at the top of the frame (y = 0) and travel to the bottom (y = 1).
//! Arrivals are a Poisson process whose rate ebbs and surges on a slow "rush"
//! cycle and clusters into platoons; a simple car-following rule keeps faster
//! cars behind slower ones, so platoons stay platoons. Trucks are bigger, dimmer
//! and slower than cars. Left lanes (lane 0) are faster than right lanes.
//!
//! Everything is driven by [`Freeway::advance_to`] with the engine clock, so it
//! works identically under a wall clock and under a virtual render clock, and is
//! deterministic for a fixed seed.

use std::f64::consts::TAU;

use rand::RngExt;
use rand_chacha::ChaCha8Rng;
use serde_json::json;
use sonder_core::{Seconds, SourceSpec, SourceState};

use crate::registry::{ParamInfo, SourceInfo};
use crate::util::{self, clamp01, Attrs, Emitter, RateWindow};
use crate::{Source, SourceCtx, SourceError, SourceFuture};

/// Frame-traversal speed at normalized speed 1.0 (fraction of frame per second):
/// a car at speed 1 crosses the frame in 2.5 s.
const TRAVERSAL: f64 = 0.4;
/// Simulation step cap (seconds).
const MAX_STEP: f64 = 0.05;
/// Max catch-up when the clock jumps (seconds).
const MAX_CATCHUP: f64 = 10.0;
/// Cars per second per lane at flow = 1.0 (before modulation).
const BASE_RATE_PER_LANE: f64 = 0.3;

/// Parameters of the simulation.
#[derive(Debug, Clone, PartialEq)]
pub struct FreewayConfig {
    pub lanes: usize,
    /// 0..1 mean arrival rate scale.
    pub flow: f64,
    /// 0..1 mean normalized speed.
    pub speed: f64,
    /// Period (s) of the slow flow modulation; None = steady.
    pub rush: Option<f64>,
    /// Fraction of vehicles that are trucks.
    pub truck_ratio: f64,
}

impl Default for FreewayConfig {
    fn default() -> Self {
        Self { lanes: 4, flow: 0.4, speed: 0.5, rush: Some(240.0), truck_ratio: 0.15 }
    }
}

impl FreewayConfig {
    pub fn from_spec(spec: &SourceSpec) -> Result<Self, SourceError> {
        let lanes = util::param_f64_in(spec, "lanes", 4.0, 1.0, 12.0)?.round() as usize;
        let flow = util::param_f64_in(spec, "flow", 0.4, 0.0, 1.0)?;
        let speed = util::param_f64_in(spec, "speed", 0.5, 0.05, 1.0)?;
        let rush = util::param_f64(spec, "rush", 240.0)?;
        let rush = if rush > 0.0 && rush.is_finite() { Some(rush.max(10.0)) } else { None };
        let truck_ratio = util::param_f64_in(spec, "trucks", 0.15, 0.0, 1.0)?;
        Ok(Self { lanes, flow, speed, rush, truck_ratio })
    }
}

/// A vehicle in the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Car {
    pub lane: usize,
    /// Lateral position 0..1 (lane centre + jitter).
    pub x: f64,
    /// Vertical position: 0 = top (entry), 1 = bottom (exit).
    pub y: f64,
    /// Desired normalized speed 0..1.
    pub want_speed: f64,
    /// Current normalized speed (after car-following).
    pub speed: f64,
    /// Relative size (1 = ordinary car; trucks ≈ 2).
    pub size: f64,
    /// Headlight brightness 0..1.
    pub brightness: f64,
    pub truck: bool,
    pub entered_at: Seconds,
}

/// A car leaving the frame: becomes a `car` event.
#[derive(Debug, Clone, PartialEq)]
pub struct CarExit {
    pub car: Car,
    pub at: Seconds,
    pub duration: f64,
}

impl CarExit {
    /// Event intensity: size × brightness, scaled so ordinary cars land around 0.4.
    pub fn intensity(&self) -> f64 {
        clamp01(0.5 * self.car.size * self.car.brightness)
    }
    pub fn attrs(&self, lanes: usize) -> Attrs {
        Attrs::new()
            .num("lane", self.car.lane as f64)
            .num("lane01", if lanes > 1 { self.car.lane as f64 / (lanes - 1) as f64 } else { 0.5 })
            .num("x", self.car.x)
            .num("speed", self.car.speed)
            .num("size", self.car.size)
            .num("brightness", self.car.brightness)
            .num("duration", self.duration)
            .bool("truck", self.car.truck)
    }
}

/// The simulation state.
#[derive(Debug, Clone)]
pub struct Freeway {
    pub cfg: FreewayConfig,
    rng: ChaCha8Rng,
    /// Simulation time (engine seconds).
    pub t: Seconds,
    pub cars: Vec<Car>,
    exits: RateWindow,
    pub total_exits: u64,
    pub total_arrivals: u64,
    rush_phase: f64,
    platoon_until: Seconds,
    platoon_mult: f64,
    started_at: Seconds,
}

impl Freeway {
    pub fn new(cfg: FreewayConfig, seed: u64, start: Seconds) -> Self {
        let mut rng = util::rng_from_seed(seed);
        let rush_phase = rng.random_range(0.0..TAU);
        Self {
            cfg,
            rng,
            t: start,
            cars: Vec::new(),
            exits: RateWindow::default(),
            total_exits: 0,
            total_arrivals: 0,
            rush_phase,
            platoon_until: -1.0,
            platoon_mult: 1.0,
            started_at: start,
        }
    }

    /// Slow modulation of flow (mean 1.0) from the rush cycle.
    pub fn rush_factor(&self, t: Seconds) -> f64 {
        match self.cfg.rush {
            Some(period) => {
                let m = 0.55 + 0.45 * (TAU * (t - self.started_at) / period + self.rush_phase).sin();
                m / 0.55
            }
            None => 1.0,
        }
    }

    /// Current total arrival rate (cars/s) including rush and platoon modulation.
    pub fn arrival_rate(&self, t: Seconds) -> f64 {
        let base = self.cfg.flow * self.cfg.lanes as f64 * BASE_RATE_PER_LANE;
        let platoon = if t < self.platoon_until { self.platoon_mult } else { 1.0 };
        base * self.rush_factor(t) * platoon
    }

    fn poisson(&mut self, lambda: f64) -> u32 {
        // Knuth; lambda is small (≤ ~1) because steps are ≤ 50 ms.
        if lambda <= 0.0 {
            return 0;
        }
        let l = (-lambda).exp();
        let mut k = 0u32;
        let mut p = 1.0;
        loop {
            p *= self.rng.random::<f64>();
            if p <= l || k > 50 {
                return k;
            }
            k += 1;
        }
    }

    fn lane_speed(&self, lane: usize) -> f64 {
        if self.cfg.lanes <= 1 {
            return self.cfg.speed;
        }
        let f = lane as f64 / (self.cfg.lanes - 1) as f64;
        self.cfg.speed * (1.2 - 0.4 * f)
    }

    fn spawn(&mut self) {
        let lanes = self.cfg.lanes;
        // Try a few lanes; skip the arrival if every candidate is blocked at the entry.
        for _ in 0..lanes.max(1) {
            let lane = self.rng.random_range(0..lanes);
            let blocked = self.cars.iter().any(|c| c.lane == lane && c.y < 0.05);
            if blocked {
                continue;
            }
            let truck = self.rng.random_bool(self.cfg.truck_ratio);
            let noise: f64 = self.rng.random_range(-0.08..0.08);
            let mut want = (self.lane_speed(lane) + noise).clamp(0.05, 1.0);
            let (size, brightness) = if truck {
                want *= 0.85;
                (self.rng.random_range(1.6..2.3), self.rng.random_range(0.45..0.7))
            } else {
                (self.rng.random_range(0.8..1.2), self.rng.random_range(0.6..1.0))
            };
            let centre = (lane as f64 + 0.5) / lanes as f64;
            let jitter: f64 = self.rng.random_range(-0.15..0.15) / lanes as f64;
            let car = Car {
                lane,
                x: (centre + jitter).clamp(0.0, 1.0),
                y: -0.02,
                want_speed: want,
                speed: want,
                size,
                brightness,
                truck,
                entered_at: self.t,
            };
            self.cars.push(car);
            self.total_arrivals += 1;
            return;
        }
    }

    /// Advance by `dt` (≤ 50 ms recommended). Returns the cars that left the frame.
    pub fn step(&mut self, dt: f64) -> Vec<CarExit> {
        let dt = dt.clamp(0.0, MAX_STEP);
        let t0 = self.t;
        self.t += dt;

        // Platoon state machine: mean 45 s between platoons, 4–10 s long, ×2.5–3.5.
        if t0 >= self.platoon_until && self.rng.random_bool((dt / 45.0).min(1.0)) {
            let len: f64 = self.rng.random_range(4.0..10.0);
            self.platoon_mult = self.rng.random_range(2.5..3.5);
            self.platoon_until = self.t + len;
        }

        // Arrivals.
        let lambda = self.arrival_rate(t0) * dt;
        let n = self.poisson(lambda);
        for _ in 0..n {
            self.spawn();
        }

        // Car-following: within a lane, nobody drives faster than the car ahead
        // when the gap is short. Sort by lane then y descending (front first).
        self.cars.sort_by(|a, b| a.lane.cmp(&b.lane).then(b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal)));
        for i in 0..self.cars.len() {
            let lane = self.cars[i].lane;
            let ahead = i
                .checked_sub(1)
                .filter(|&j| self.cars[j].lane == lane)
                .map(|j| (self.cars[j].y, self.cars[j].speed));
            let c = &mut self.cars[i];
            match ahead {
                None => {
                    // Front car in lane relaxes toward its desired speed.
                    c.speed += (c.want_speed - c.speed) * (dt * 0.8).min(1.0);
                }
                Some((ahead_y, ahead_speed)) => {
                    let gap = ahead_y - c.y;
                    let min_gap = 0.05 + 0.08 * c.speed * c.size.sqrt();
                    if gap < min_gap {
                        // Too close: match (slightly undercut) the car ahead.
                        let target = (ahead_speed * 0.97).min(c.want_speed);
                        c.speed += (target - c.speed) * (dt * 6.0).min(1.0);
                    } else {
                        c.speed += (c.want_speed - c.speed) * (dt * 0.8).min(1.0);
                    }
                }
            }
        }

        // Move and collect exits.
        let mut exits = Vec::new();
        let now = self.t;
        let mut keep = Vec::with_capacity(self.cars.len());
        for mut c in self.cars.drain(..) {
            c.y += c.speed * TRAVERSAL * dt;
            if c.y >= 1.0 {
                let duration = now - c.entered_at;
                exits.push(CarExit { car: c, at: now, duration });
            } else {
                keep.push(c);
            }
        }
        self.cars = keep;
        for _ in &exits {
            self.exits.push(now);
            self.total_exits += 1;
        }
        exits
    }

    /// Advance the simulation to engine time `t` in ≤ 50 ms steps. Large jumps are
    /// capped at 10 s of simulated time (the gap is skipped).
    pub fn advance_to(&mut self, t: Seconds) -> Vec<CarExit> {
        if t <= self.t {
            return Vec::new();
        }
        if t - self.t > MAX_CATCHUP {
            self.t = t - MAX_CATCHUP;
        }
        let mut exits = Vec::new();
        while self.t < t - 1e-9 {
            let dt = (t - self.t).min(MAX_STEP);
            exits.extend(self.step(dt));
        }
        exits
    }

    /// Current readings: count, mean_speed, flow (cars/min over the last minute), density.
    pub fn readings(&mut self) -> Vec<(&'static str, f64)> {
        let count = self.cars.len() as f64;
        let mean_speed = if self.cars.is_empty() {
            self.cfg.speed
        } else {
            self.cars.iter().map(|c| c.speed).sum::<f64>() / count
        };
        let window = (self.t - self.started_at).clamp(1.0, 60.0);
        let flow = self.exits.count(self.t, 60.0) as f64 * 60.0 / window;
        let density = count / self.cfg.lanes as f64;
        vec![
            ("count", count),
            ("mean_speed", mean_speed),
            ("flow", flow),
            ("density", density),
            ("rush", self.rush_factor(self.t)),
        ]
    }

    /// Visualization payload: `{"cars":[{x,y,lane,size,bright}], "lanes": N}`.
    pub fn viz(&self) -> serde_json::Value {
        let cars: Vec<_> = self
            .cars
            .iter()
            .map(|c| {
                json!({
                    "x": round3(c.x), "y": round3(c.y), "lane": c.lane,
                    "size": round3(c.size), "bright": round3(c.brightness)
                })
            })
            .collect();
        json!({ "cars": cars, "lanes": self.cfg.lanes })
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

struct FreewaySource {
    cfg: FreewayConfig,
    seed: Option<u64>,
}

impl Source for FreewaySource {
    fn run(self: Box<Self>, ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let seed = util::instance_seed(ctx.seed, ctx.id.as_str(), self.seed);
            let mut em = Emitter::new(ctx);
            let start = em.now();
            let mut sim = Freeway::new(self.cfg.clone(), seed, start);
            let lanes = self.cfg.lanes;
            em.status(
                SourceState::Live,
                format!("simulating {} lanes, flow {:.2}, seed {seed}", lanes, self.cfg.flow),
            )
            .await;
            let mut next_reading = start + 0.5;
            let mut next_viz = start + 0.1;
            loop {
                let now = em.now();
                if now - sim.t > MAX_CATCHUP {
                    sim.t = now - MAX_CATCHUP;
                    next_reading = sim.t;
                    next_viz = sim.t;
                }
                while sim.t < now - 1e-9 {
                    let dt = (now - sim.t).min(MAX_STEP);
                    let exits = sim.step(dt);
                    for e in exits {
                        em.event_at(e.at, "car", e.intensity(), e.attrs(lanes).into_map()).await;
                    }
                    if sim.t + 1e-9 >= next_reading {
                        let r = sim.readings();
                        em.reading_at(sim.t, r).await;
                        next_reading += 0.5;
                    }
                    if sim.t + 1e-9 >= next_viz {
                        em.viz_at(sim.t, sim.viz()).await;
                        next_viz += 0.1;
                    }
                }
                if !em.sleep(std::time::Duration::from_millis(50)).await {
                    return;
                }
            }
        })
    }
}

pub fn info() -> SourceInfo {
    SourceInfo {
        kind: "synthetic.freeway",
        summary: "Simulated night freeway: headlights crossing a frame, with rush cycles and platoons",
        params: vec![
            ParamInfo { name: "lanes", summary: "Number of lanes (1..12)", default: Some("4") },
            ParamInfo { name: "flow", summary: "Mean arrival rate scale 0..1", default: Some("0.4") },
            ParamInfo { name: "speed", summary: "Mean normalized speed 0..1", default: Some("0.5") },
            ParamInfo { name: "rush", summary: "Period in seconds of the slow ebb/surge cycle (0 = steady)", default: Some("240") },
            ParamInfo { name: "trucks", summary: "Fraction of vehicles that are trucks", default: Some("0.15") },
            ParamInfo { name: "seed", summary: "RNG seed (default: engine seed mixed with source id)", default: None },
        ],
        channels: vec!["count", "mean_speed", "flow", "density", "rush"],
        events: vec!["car"],
        network: false,
    }
}

pub fn factory(spec: &SourceSpec) -> Result<Box<dyn Source>, SourceError> {
    let cfg = FreewayConfig::from_spec(spec)?;
    let seed = util::param_seed(spec)?;
    Ok(Box::new(FreewaySource { cfg, seed }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cfg: FreewayConfig, seed: u64, secs: f64) -> Freeway {
        let mut sim = Freeway::new(cfg, seed, 0.0);
        let mut t = 0.0;
        while t < secs {
            t += 0.05;
            sim.advance_to(t);
        }
        sim
    }

    #[test]
    fn ten_minutes_has_plausible_traffic() {
        let sim = run(FreewayConfig::default(), 42, 600.0);
        // Base rate 0.4 * 4 * 0.3 = 0.48 cars/s ≈ 288 in 10 min; platoons add some,
        // blocked entries remove some.
        eprintln!("freeway: {} exits in 10 min, {} in frame", sim.total_exits, sim.cars.len());
        assert!(sim.total_exits > 120 && sim.total_exits < 700, "exits = {}", sim.total_exits);
        assert!(sim.cars.len() < 60);
    }

    #[test]
    fn deterministic_for_seed() {
        let a = run(FreewayConfig::default(), 7, 120.0);
        let b = run(FreewayConfig::default(), 7, 120.0);
        assert_eq!(a.total_exits, b.total_exits);
        assert_eq!(a.cars, b.cars);
        let c = run(FreewayConfig::default(), 8, 120.0);
        assert!(a.total_exits != c.total_exits || a.cars != c.cars);
    }

    #[test]
    fn exits_carry_attrs_and_sane_intensity() {
        let mut sim = Freeway::new(FreewayConfig { flow: 1.0, ..Default::default() }, 3, 0.0);
        let mut exits = Vec::new();
        let mut t = 0.0;
        while t < 60.0 {
            t += 0.05;
            exits.extend(sim.advance_to(t));
        }
        assert!(!exits.is_empty());
        for e in &exits {
            let a = e.attrs(4).into_map();
            for k in ["lane", "x", "speed", "size", "brightness", "duration", "truck"] {
                assert!(a.contains_key(k), "missing {k}");
            }
            let i = e.intensity();
            assert!((0.0..=1.0).contains(&i));
            assert!(e.duration > 1.0 && e.duration < 60.0, "duration {}", e.duration);
            assert!((0.0..=1.0).contains(&e.car.x));
        }
    }

    #[test]
    fn viz_shape_and_readings() {
        let mut sim = run(FreewayConfig::default(), 1, 30.0);
        let v = sim.viz();
        assert_eq!(v["lanes"], 4);
        let cars = v["cars"].as_array().unwrap();
        for c in cars {
            for k in ["x", "y", "lane", "size", "bright"] {
                assert!(c.get(k).is_some());
            }
            let y = c["y"].as_f64().unwrap();
            assert!(y < 1.0);
        }
        let r: std::collections::BTreeMap<_, _> = sim.readings().into_iter().collect();
        assert_eq!(r["count"], cars.len() as f64);
        assert!(r["flow"] > 0.0);
        assert!(r["mean_speed"] > 0.0 && r["mean_speed"] <= 1.0);
    }

    #[test]
    fn clock_jump_is_capped() {
        let mut sim = Freeway::new(FreewayConfig::default(), 1, 0.0);
        sim.advance_to(1000.0);
        assert!((sim.t - 1000.0).abs() < 1e-6);
        // Only 10 s were simulated.
        assert!(sim.total_arrivals < 80);
    }

    #[test]
    fn rush_modulates_rate() {
        let sim = Freeway::new(FreewayConfig { rush: Some(100.0), ..Default::default() }, 1, 0.0);
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for i in 0..100 {
            let f = sim.rush_factor(i as f64);
            lo = lo.min(f);
            hi = hi.max(f);
        }
        assert!(lo < 0.3 && hi > 1.7);
        let steady = Freeway::new(FreewayConfig { rush: None, ..Default::default() }, 1, 0.0);
        assert_eq!(steady.rush_factor(50.0), 1.0);
    }

    #[test]
    fn factory_validates() {
        assert!(factory(&SourceSpec::new("synthetic.freeway").with("lanes", 99)).is_err());
        assert!(factory(&SourceSpec::new("synthetic.freeway").with("flow", 0.7)).is_ok());
    }
}
