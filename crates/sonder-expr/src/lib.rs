//! # sonder-expr
//!
//! A tiny expression language for feature extraction and mapping, evaluated at the
//! engine's 50 Hz control rate with **stateful nodes**: every call-site of a stateful
//! builtin (`smooth`, `rate`, `norm`, `onset`, `random`, …) owns its own state.
//!
//! ```
//! use std::collections::BTreeMap;
//! use sonder_expr::LayerPipeline;
//!
//! let signals = BTreeMap::from([
//!     ("density".to_string(), "norm(smooth(rate(event('car'), 10s), 3s))".to_string()),
//!     ("spike".to_string(), "onset(density, 0.8)".to_string()),
//! ]);
//! let map = BTreeMap::from([("energy".to_string(), "density".to_string())]);
//! let mut p = LayerPipeline::compile(&signals, &map, 42).unwrap();
//!
//! p.push_event("car", 1.0, &BTreeMap::new(), 0.0);
//! let fired = p.tick(0.02, 0.02, 0.0, 0.0, 70.0);
//! assert!(fired.is_empty());
//! assert!(p.signals().contains_key("density"));
//! assert!(p.params().contains_key("energy"));
//! ```
//!
//! See `README.md` in this crate for the language reference. The grammar:
//!
//! ```text
//! expr   := or
//! or     := and ( "||" and )*
//! and    := cmp ( "&&" cmp )*
//! cmp    := add ( ("<" | "<=" | ">" | ">=" | "==" | "!=") add )?
//! add    := mul ( ("+" | "-") mul )*
//! mul    := unary ( ("*" | "/" | "%") unary )*
//! unary  := ("-" | "!") unary | pow
//! pow    := atom ( "^" unary )?
//! atom   := number | string | ident | ident "(" args ")" | "(" expr ")"
//! number := 1.5 | 2 | 10s | 250ms | 2m          -- unit suffixes convert to seconds
//! string := 'single' | "double"
//! ```
//!
//! Determinism: evaluation never reads the wall clock; `random()`/`walk()` are seeded
//! from the pipeline seed and the call-site index. Same inputs ⇒ same outputs.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod ast;
mod compile;
mod error;
mod event;
mod grid;
mod lexer;
mod node;
mod parser;
mod pipeline;

pub use compile::{
    compile, compile_seeded, Program, Scope, Tick, CONSTANTS, EVENT_CONSTANTS, PURE_FUNCTIONS,
    STATEFUL_FUNCTIONS,
};
pub use error::ExprError;
pub use event::{EventCtx, EventExpr};
pub use grid::parse_grid;
pub use node::{hash_num, hash_text, MAX_WINDOW_ENTRIES, NORM_HALF_LIFE, NORM_WARMUP};
pub use pipeline::{Fired, LayerPipeline};

/// Parse an expression without compiling it, returning only syntax errors.
pub fn check_syntax(src: &str) -> Result<(), ExprError> {
    parser::parse(src).map(|_| ())
}
