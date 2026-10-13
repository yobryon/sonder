//! Per-layer pipeline: ordered signals, then map expressions, ticked at control rate.

use std::collections::BTreeMap;

use sonder_core::{Seconds, Value};

use crate::ast::Expr;
use crate::compile::{lower, referenced_signals, Lowerer, Scope, Tick};
use crate::error::ExprError;
use crate::node::{finite, Ctx, Env, Node, SignalView};
use crate::parser::parse;

/// A signal whose top-level node is an edge detector (`onset`, `every`, `change`)
/// fired this tick. The engine turns these into `LayerEvent`s named after the signal.
#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    /// Signal name.
    pub name: String,
    /// The detector's output (1.0 for the builtin detectors).
    pub intensity: f64,
}

#[derive(Debug, Clone)]
struct Compiled {
    name: String,
    node: Node,
    /// Index into `Env::signal_values` (signals only).
    slot: usize,
    is_edge: bool,
}

/// Everything compiled for one layer: its `signals` (evaluated in dependency order,
/// each may reference the others acyclically) then its `map` expressions.
#[derive(Debug, Clone)]
pub struct LayerPipeline {
    env: Env,
    /// Signals in evaluation (dependency) order.
    signals: Vec<Compiled>,
    /// Map expressions in key order.
    params: Vec<Compiled>,
    signal_values: BTreeMap<String, f64>,
    param_values: BTreeMap<String, f64>,
    /// `signal_values` key order → `Env` slot, for the per-tick copy.
    signal_order: Vec<usize>,
    seed: u64,
    ticks: u64,
}

impl LayerPipeline {
    /// Compile a layer's `signals` and `map` tables. Any source channel name is accepted.
    pub fn compile(signals: &BTreeMap<String, String>, map: &BTreeMap<String, String>, seed: u64) -> Result<Self, ExprError> {
        Self::compile_with_fields(signals, map, seed, &[])
    }

    /// Like [`LayerPipeline::compile`] but restricts bare identifiers / `field(..)` to
    /// `fields` when non-empty (for validation when the source's channels are known).
    pub fn compile_with_fields(
        signals: &BTreeMap<String, String>,
        map: &BTreeMap<String, String>,
        seed: u64,
        fields: &[String],
    ) -> Result<Self, ExprError> {
        let names: Vec<String> = signals.keys().cloned().collect();
        for n in &names {
            if n.is_empty() {
                return Err(ExprError::new("signal name must not be empty"));
            }
        }

        // Parse everything first (errors carry the table key as context).
        let mut asts: BTreeMap<&str, Expr> = BTreeMap::new();
        for (name, src) in signals {
            let ast = parse(src).map_err(|e| e.with_context(&format!("signals.{name}")))?;
            if referenced_signals(&ast, std::slice::from_ref(name)).contains(name) {
                return Err(ExprError::new(format!(
                    "signals.{name}: a signal cannot reference itself; use field(\"{name}\") for the source channel"
                )));
            }
            asts.insert(name.as_str(), ast);
        }
        let order = dependency_order(&names, &asts)?;

        let scope = Scope { signals: names.clone(), fields: fields.to_vec(), event_ctx: false };
        let mut env = Env::default();
        // Allocate signal slots in key order so indices are stable.
        for n in &names {
            env.signal_slot(n);
        }
        let mut sites = 0usize;
        let mut compiled_signals = Vec::with_capacity(names.len());
        for name in &order {
            let ast = &asts[name.as_str()];
            let node = lower(ast, &mut Lowerer { env: &mut env, scope: &scope, seed, call_sites: &mut sites })
                .map_err(|e| e.with_context(&format!("signals.{name}")))?;
            let slot = env.signal_slot(name);
            compiled_signals.push(Compiled { name: name.clone(), is_edge: node.is_edge_detector(), node, slot });
        }
        let mut params = Vec::with_capacity(map.len());
        for (key, src) in map {
            let ast = parse(src).map_err(|e| e.with_context(&format!("map.{key}")))?;
            let node = lower(&ast, &mut Lowerer { env: &mut env, scope: &scope, seed, call_sites: &mut sites })
                .map_err(|e| e.with_context(&format!("map.{key}")))?;
            params.push(Compiled { name: key.clone(), is_edge: node.is_edge_detector(), node, slot: usize::MAX });
        }

        let signal_values: BTreeMap<String, f64> = names.iter().map(|n| (n.clone(), 0.0)).collect();
        let signal_order = signal_values.keys().map(|n| env.signal_slot(n)).collect();
        let param_values = map.keys().map(|k| (k.clone(), 0.0)).collect();
        Ok(Self { env, signals: compiled_signals, params, signal_values, param_values, signal_order, seed, ticks: 0 })
    }

    /// Update the latest values of source channels (only referenced ones are kept).
    pub fn push_reading(&mut self, values: &BTreeMap<String, f64>) {
        self.env.push_reading(values);
    }

    /// Record an event. Its intensity is added to every matching `event(..)` impulse
    /// for the next tick; `attrs` and `at` are accepted for interface symmetry and
    /// are not used by signal expressions.
    pub fn push_event(&mut self, name: &str, intensity: f64, attrs: &BTreeMap<String, Value>, at: Seconds) {
        let _ = (attrs, at);
        self.env.push_event(name, intensity);
    }

    /// Advance one control tick: evaluate signals in dependency order, then map
    /// expressions; clear accumulated events. Returns edge-detector signals that fired.
    pub fn tick(&mut self, now: Seconds, dt: f64, beat: f64, bar: f64, bpm: f64) -> Vec<Fired> {
        let mut fired = Vec::new();
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.02 };
        for i in 0..self.signals.len() {
            let v = {
                let ctx = Ctx {
                    now,
                    dt,
                    beat,
                    bar,
                    bpm,
                    env: &self.env,
                    signals: SignalView::Slice(&self.env.signal_values),
                    intensity: 0.0,
                    attrs: None,
                };
                finite(self.signals[i].node.eval(&ctx))
            };
            let sig = &self.signals[i];
            self.env.signal_values[sig.slot] = v;
            if sig.is_edge && v != 0.0 {
                fired.push(Fired { name: sig.name.clone(), intensity: v });
            }
        }
        for (dst, &slot) in self.signal_values.values_mut().zip(self.signal_order.iter()) {
            *dst = self.env.signal_values[slot];
        }
        for (i, dst) in self.param_values.values_mut().enumerate() {
            let ctx = Ctx {
                now,
                dt,
                beat,
                bar,
                bpm,
                env: &self.env,
                signals: SignalView::Slice(&self.env.signal_values),
                intensity: 0.0,
                attrs: None,
            };
            *dst = finite(self.params[i].node.eval(&ctx));
        }
        self.env.clear_events();
        self.ticks += 1;
        fired
    }

    /// Convenience for tests and tools: tick with a [`Tick`].
    pub fn tick_with(&mut self, t: &Tick) -> Vec<Fired> {
        self.tick(t.now, t.dt, t.beat, t.bar, t.bpm)
    }

    /// Current value of each signal (updated by [`LayerPipeline::tick`]).
    pub fn signals(&self) -> &BTreeMap<String, f64> {
        &self.signal_values
    }

    /// Current value of each map expression (updated by [`LayerPipeline::tick`]).
    pub fn params(&self) -> &BTreeMap<String, f64> {
        &self.param_values
    }

    /// Source channels referenced by any expression (for probe/validation).
    pub fn field_names(&self) -> Vec<String> {
        self.env.field_names.clone()
    }

    /// Event name patterns referenced via `event(..)`.
    pub fn event_names(&self) -> Vec<String> {
        self.env.event_patterns.clone()
    }

    /// Names of signals whose top-level node is an edge detector (they produce [`Fired`]).
    pub fn edge_signals(&self) -> Vec<String> {
        self.signals.iter().filter(|s| s.is_edge).map(|s| s.name.clone()).collect()
    }

    /// Signal names in evaluation order.
    pub fn evaluation_order(&self) -> Vec<String> {
        self.signals.iter().map(|s| s.name.clone()).collect()
    }

    /// Seed this pipeline was compiled with.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Number of ticks processed so far.
    pub fn ticks(&self) -> u64 {
        self.ticks
    }
}

/// Topologically sort signals by their references (stable: ties broken by key order).
fn dependency_order(names: &[String], asts: &BTreeMap<&str, Expr>) -> Result<Vec<String>, ExprError> {
    let deps: BTreeMap<&str, Vec<String>> =
        names.iter().map(|n| (n.as_str(), referenced_signals(&asts[n.as_str()], names))).collect();
    let mut done: Vec<String> = Vec::with_capacity(names.len());
    let mut remaining: Vec<&str> = names.iter().map(String::as_str).collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut i = 0;
        while i < remaining.len() {
            let n = remaining[i];
            if deps[n].iter().all(|d| done.iter().any(|x| x == d)) {
                done.push(n.to_string());
                remaining.remove(i);
            } else {
                i += 1;
            }
        }
        if remaining.len() == before {
            return Err(ExprError::new(format!(
                "signals reference each other in a cycle: {}",
                remaining.join(" -> ")
            )));
        }
    }
    Ok(done)
}
