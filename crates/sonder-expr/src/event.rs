//! `on_event` value expressions: evaluated once per event, stateless.

use std::collections::BTreeMap;

use sonder_core::{Seconds, Value};

use crate::compile::{compile, Program, Scope};
use crate::error::ExprError;

/// Context for evaluating one event's `on_event` expressions.
#[derive(Debug, Clone, Copy)]
pub struct EventCtx<'a> {
    /// The event's intensity (sources use ~0..1).
    pub intensity: f64,
    /// The event's attributes (`attr("k")`, `attr_text("k")`, `has("k")`).
    pub attrs: &'a BTreeMap<String, Value>,
    /// The layer's current signal values, referenced by bare name.
    pub signals: &'a BTreeMap<String, f64>,
    /// Engine time (`time`).
    pub time: Seconds,
    /// Transport beats (`beat`).
    pub beat: f64,
}

/// A compiled `on_event` value expression (e.g. `on_event.car.pitch = "attr('lane') / 4"`).
///
/// Stateful builtins are rejected at compile time, so evaluation is `&self` and
/// side-effect free.
#[derive(Debug, Clone)]
pub struct EventExpr {
    program: Program,
    src: String,
}

impl EventExpr {
    /// Compile `src`; `signal_names` are the layer's signals that may be referenced.
    pub fn compile(src: &str, signal_names: &[String]) -> Result<Self, ExprError> {
        let scope = Scope::event(signal_names.iter().cloned());
        let program = compile(src, &scope)?;
        Ok(Self {
            program,
            src: src.to_string(),
        })
    }

    /// Evaluate for one event. Non-finite results become 0.
    pub fn eval(&self, ctx: &EventCtx) -> f64 {
        self.program.eval_event(ctx)
    }

    /// Source text.
    pub fn source(&self) -> &str {
        &self.src
    }

    /// Signals referenced by this expression.
    pub fn signal_names(&self) -> Vec<String> {
        self.program.signal_names()
    }
}
