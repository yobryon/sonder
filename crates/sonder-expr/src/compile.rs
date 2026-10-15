//! Lowering from AST to evaluator nodes, plus the standalone [`Program`] API.

use std::collections::BTreeMap;

use sonder_core::Seconds;

use crate::ast::{BinOp, Expr};
use crate::error::ExprError;
use crate::event::EventCtx;
use crate::lexer::Span;
use crate::node::{hash_text, site_seed, Ctx, Env, Node, PureFn, SignalView, Stateful};
use crate::parser::parse;

/// Names visible to an expression at compile time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Signal names a bare identifier may resolve to (checked first).
    pub signals: Vec<String>,
    /// Source channels a bare identifier may resolve to. **Empty = allow any** name
    /// (the channels of a live source are not known at compile time).
    pub fields: Vec<String>,
    /// `true` inside `on_event` mappings: enables `intensity`, `attr`, `attr_text`,
    /// `has`; disables `field`, `event` and every stateful builtin.
    pub event_ctx: bool,
}

impl Scope {
    /// A signal-expression scope: these signals, any field, not event context.
    pub fn signals(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            signals: names.into_iter().map(Into::into).collect(),
            ..Default::default()
        }
    }
    /// An `on_event` scope.
    pub fn event(signal_names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            signals: signal_names.into_iter().map(Into::into).collect(),
            fields: Vec::new(),
            event_ctx: true,
        }
    }
}

/// Builtin constants usable as bare identifiers.
pub const CONSTANTS: &[&str] = &["time", "dt", "beat", "bar", "bpm", "pi", "tau", "e"];
/// Builtin constants only valid in event context.
pub const EVENT_CONSTANTS: &[&str] = &["intensity"];
/// Pure builtin function names.
pub const PURE_FUNCTIONS: &[&str] = &[
    "abs",
    "min",
    "max",
    "clamp",
    "lerp",
    "pow",
    "sqrt",
    "exp",
    "log",
    "log2",
    "log10",
    "floor",
    "ceil",
    "round",
    "fract",
    "sign",
    "step",
    "smoothstep",
    "if",
    "sin",
    "cos",
    "tan",
    "tanh",
    "wrap",
    "hash",
    "has",
    "attr_len",
    "attr",
    "attr_text",
    "field",
    "event",
];
/// Stateful builtin function names (each call-site owns state).
pub const STATEFUL_FUNCTIONS: &[&str] = &[
    "smooth",
    "ema",
    "derivative",
    "norm",
    "rate",
    "count",
    "sum",
    "lag",
    "hold",
    "maxw",
    "minw",
    "onset",
    "every",
    "change",
    "random",
    "walk",
];

/// Compile-time bookkeeping for one pipeline (shared across its expressions).
pub(crate) struct Lowerer<'a> {
    pub env: &'a mut Env,
    pub scope: &'a Scope,
    pub seed: u64,
    /// Running count of stateful call-sites, used to derive per-site RNG seeds.
    pub call_sites: &'a mut usize,
}

/// Lower a parsed expression into a node tree.
pub(crate) fn lower(expr: &Expr, lw: &mut Lowerer) -> Result<Node, ExprError> {
    match expr {
        Expr::Num { value, .. } => Ok(Node::Const(*value)),
        Expr::Str { span, .. } => Err(ExprError::at(
            *span,
            "string literal not allowed here; strings are only valid as arguments to field/event/attr/attr_text/has/hash, or compared with `attr_text(..) == \"..\"`",
        )),
        Expr::Ident { name, span } => lower_ident(name, *span, lw),
        Expr::Unary { op, rhs, .. } => Ok(Node::Unary(*op, Box::new(lower(rhs, lw)?))),
        Expr::Binary { op, lhs, rhs, span } => lower_binary(*op, lhs, rhs, *span, lw),
        Expr::Call { name, name_span, args, span } => lower_call(name, *name_span, args, *span, lw),
    }
}

fn lower_ident(name: &str, span: Span, lw: &mut Lowerer) -> Result<Node, ExprError> {
    if lw.scope.signals.iter().any(|s| s == name) {
        let idx = lw.env.signal_slot(name);
        return Ok(Node::Signal {
            idx,
            name: name.to_string(),
        });
    }
    match name {
        "time" => return Ok(Node::Time),
        "dt" => return Ok(Node::Dt),
        "beat" => return Ok(Node::Beat),
        "bar" => return Ok(Node::Bar),
        "bpm" => return Ok(Node::Bpm),
        "pi" => return Ok(Node::Const(std::f64::consts::PI)),
        "tau" => return Ok(Node::Const(std::f64::consts::TAU)),
        "e" => return Ok(Node::Const(std::f64::consts::E)),
        "intensity" if lw.scope.event_ctx => return Ok(Node::Intensity),
        "intensity" => {
            return Err(ExprError::at(
                span,
                "`intensity` is only available in on_event expressions",
            ));
        }
        _ => {}
    }
    // Constants win over bare channel names so that validation (known channels) and
    // runtime (any channel) agree; a channel called `e` or `time` is still reachable
    // as `field("e")`.
    if !lw.scope.event_ctx && (lw.scope.fields.is_empty() || lw.scope.fields.iter().any(|f| f == name)) {
        return Ok(Node::Field(lw.env.field_slot(name)));
    }
    Err(ExprError::at(span, unknown_ident_message(name, lw.scope)))
}

fn unknown_ident_message(name: &str, scope: &Scope) -> String {
    let mut msg = format!("unknown identifier `{name}`");
    if let Some(s) = suggest(
        name,
        scope
            .signals
            .iter()
            .chain(scope.fields.iter())
            .map(String::as_str),
    ) {
        msg.push_str(&format!(" (did you mean `{s}`?)"));
    }
    msg.push_str(". Known signals: ");
    msg.push_str(&list_or_none(&scope.signals));
    if scope.event_ctx {
        msg.push_str("; constants: intensity, ");
    } else {
        msg.push_str("; fields: ");
        msg.push_str(&list_or_none(&scope.fields));
        msg.push_str("; constants: ");
    }
    msg.push_str(&CONSTANTS.join(", "));
    msg
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

/// Smallest-edit-distance candidate within distance 2, if any.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|c| (levenshtein(name, c), c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn lower_binary(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    span: Span,
    lw: &mut Lowerer,
) -> Result<Node, ExprError> {
    // String equality: `attr_text("k") == "lit"` in either order.
    if matches!(op, BinOp::Eq | BinOp::Ne) {
        let negate = op == BinOp::Ne;
        match (lhs, rhs) {
            (Expr::Str { value: a, .. }, Expr::Str { value: b, .. }) => {
                return Ok(Node::Const(if (a == b) != negate { 1.0 } else { 0.0 }));
            }
            (Expr::Str { value, .. }, other) | (other, Expr::Str { value, .. }) => {
                let key = expect_attr_text(other, lw)?;
                return Ok(Node::StrEq {
                    key,
                    lit: value.clone(),
                    negate,
                });
            }
            _ => {}
        }
    }
    if let Some(call_span) = find_attr_text(lhs).or_else(|| find_attr_text(rhs)) {
        return Err(ExprError::at(call_span, "`attr_text(..)` can only be compared with `==`/`!=` to a string, or passed to `hash()`"));
    }
    if matches!(lhs, Expr::Str { .. }) || matches!(rhs, Expr::Str { .. }) {
        return Err(ExprError::at(
            span,
            format!("operator `{}` cannot be applied to a string; strings are only valid as function arguments or in `attr_text(..) == \"..\"`", op.symbol()),
        ));
    }
    Ok(Node::Binary(
        op,
        Box::new(lower(lhs, lw)?),
        Box::new(lower(rhs, lw)?),
    ))
}

fn find_attr_text(e: &Expr) -> Option<Span> {
    match e {
        Expr::Call { name, span, .. } if name == "attr_text" => Some(*span),
        _ => None,
    }
}

/// The other side of a string comparison must be `attr_text("k")`; returns `k`.
fn expect_attr_text(e: &Expr, lw: &mut Lowerer) -> Result<String, ExprError> {
    match e {
        Expr::Call { name, args, span, .. } if name == "attr_text" => {
            require_event_ctx(name, *span, lw)?;
            arity(name, args, 1, 1, *span)?;
            string_arg(name, &args[0])
        }
        other => Err(ExprError::at(
            other.span(),
            "a string can only be compared with `attr_text(\"key\")` (e.g. `attr_text('wiki') == 'enwiki'`)",
        )),
    }
}

fn require_event_ctx(name: &str, span: Span, lw: &Lowerer) -> Result<(), ExprError> {
    if lw.scope.event_ctx {
        Ok(())
    } else {
        Err(ExprError::at(
            span,
            format!("`{name}` is only available in on_event expressions"),
        ))
    }
}

fn arity(name: &str, args: &[Expr], min: usize, max: usize, span: Span) -> Result<(), ExprError> {
    let n = args.len();
    if n >= min && n <= max {
        return Ok(());
    }
    let expected = if min == max {
        format!("{min}")
    } else if max == usize::MAX {
        format!("at least {min}")
    } else {
        format!("{min} to {max}")
    };
    let plural = if expected == "1" { "" } else { "s" };
    Err(ExprError::at(
        span,
        format!("`{name}` takes {expected} argument{plural}, got {n}"),
    ))
}

fn string_arg(fname: &str, e: &Expr) -> Result<String, ExprError> {
    match e {
        Expr::Str { value, .. } => Ok(value.clone()),
        other => Err(ExprError::at(
            other.span(),
            format!("`{fname}` expects a quoted string, e.g. {fname}(\"name\")"),
        )),
    }
}

fn const_arg(fname: &str, what: &str, e: &Expr) -> Result<f64, ExprError> {
    match e {
        Expr::Num { value, .. } => Ok(*value),
        Expr::Unary {
            op: crate::ast::UnOp::Neg,
            rhs,
            ..
        } => const_arg(fname, what, rhs).map(|v| -v),
        other => Err(ExprError::at(
            other.span(),
            format!("`{fname}` {what} must be a constant number (e.g. `10s`, `250ms`, `2m`)"),
        )),
    }
}

fn lower_args(args: &[Expr], lw: &mut Lowerer) -> Result<Vec<Node>, ExprError> {
    args.iter().map(|a| lower(a, lw)).collect()
}

fn lower_call(
    name: &str,
    name_span: Span,
    args: &[Expr],
    span: Span,
    lw: &mut Lowerer,
) -> Result<Node, ExprError> {
    // Data sources and event-context accessors.
    match name {
        "field" => {
            if lw.scope.event_ctx {
                return Err(ExprError::at(span, "`field` is not available in on_event expressions; reference a signal by name instead"));
            }
            arity(name, args, 1, 1, span)?;
            let fname = string_arg(name, &args[0])?;
            if !lw.scope.fields.is_empty() && !lw.scope.fields.contains(&fname) {
                let mut msg = format!("unknown field `{fname}`");
                if let Some(s) = suggest(&fname, lw.scope.fields.iter().map(String::as_str)) {
                    msg.push_str(&format!(" (did you mean `{s}`?)"));
                }
                msg.push_str(&format!(". Known fields: {}", lw.scope.fields.join(", ")));
                return Err(ExprError::at(args[0].span(), msg));
            }
            return Ok(Node::Field(lw.env.field_slot(&fname)));
        }
        "event" => {
            if lw.scope.event_ctx {
                return Err(ExprError::at(
                    span,
                    "`event` is not available in on_event expressions; use `intensity`",
                ));
            }
            arity(name, args, 1, 1, span)?;
            let pat = string_arg(name, &args[0])?;
            if pat.is_empty() {
                return Err(ExprError::at(
                    args[0].span(),
                    "event name must not be empty (use \"*\" for any event)",
                ));
            }
            return Ok(Node::Event(lw.env.event_slot(&pat)));
        }
        "attr" => {
            require_event_ctx(name, span, lw)?;
            arity(name, args, 1, 2, span)?;
            let key = string_arg(name, &args[0])?;
            let default = if args.len() == 2 {
                const_arg(name, "default", &args[1])?
            } else {
                0.0
            };
            return Ok(Node::Attr { key, default });
        }
        "attr_text" => {
            return Err(ExprError::at(span, "`attr_text(..)` can only be compared with `==`/`!=` to a string, or passed to `hash()`"));
        }
        "has" => {
            require_event_ctx(name, span, lw)?;
            arity(name, args, 1, 1, span)?;
            return Ok(Node::Has(string_arg(name, &args[0])?));
        }
        "attr_len" => {
            require_event_ctx(name, span, lw)?;
            arity(name, args, 1, 1, span)?;
            return Ok(Node::AttrLen(string_arg(name, &args[0])?));
        }
        "hash" => {
            arity(name, args, 1, 1, span)?;
            return Ok(match &args[0] {
                Expr::Str { value, .. } => Node::Const(hash_text(value)),
                Expr::Call {
                    name: inner,
                    args: inner_args,
                    span: inner_span,
                    ..
                } if inner == "attr_text" => {
                    require_event_ctx(inner, *inner_span, lw)?;
                    arity(inner, inner_args, 1, 1, *inner_span)?;
                    Node::HashAttr(string_arg(inner, &inner_args[0])?)
                }
                other => Node::HashNum(Box::new(lower(other, lw)?)),
            });
        }
        "if" => {
            arity(name, args, 3, 3, span)?;
            let mut n = lower_args(args, lw)?;
            let c = n.remove(0);
            let a = n.remove(0);
            let b = n.remove(0);
            return Ok(Node::If(Box::new(c), Box::new(a), Box::new(b)));
        }
        _ => {}
    }

    // Pure functions.
    let pure = match name {
        "abs" => Some((PureFn::Abs, 1, 1)),
        "min" => Some((PureFn::Min, 2, 8)),
        "max" => Some((PureFn::Max, 2, 8)),
        "clamp" => Some((PureFn::Clamp, 3, 3)),
        "lerp" => Some((PureFn::Lerp, 3, 3)),
        "pow" => Some((PureFn::Pow, 2, 2)),
        "sqrt" => Some((PureFn::Sqrt, 1, 1)),
        "exp" => Some((PureFn::Exp, 1, 1)),
        "log" => Some((PureFn::Log, 1, 1)),
        "log2" => Some((PureFn::Log2, 1, 1)),
        "log10" => Some((PureFn::Log10, 1, 1)),
        "floor" => Some((PureFn::Floor, 1, 1)),
        "ceil" => Some((PureFn::Ceil, 1, 1)),
        "round" => Some((PureFn::Round, 1, 1)),
        "fract" => Some((PureFn::Fract, 1, 1)),
        "sign" => Some((PureFn::Sign, 1, 1)),
        "step" => Some((PureFn::Step, 2, 2)),
        "smoothstep" => Some((PureFn::Smoothstep, 3, 3)),
        "sin" => Some((PureFn::Sin, 1, 1)),
        "cos" => Some((PureFn::Cos, 1, 1)),
        "tan" => Some((PureFn::Tan, 1, 1)),
        "tanh" => Some((PureFn::Tanh, 1, 1)),
        "wrap" => Some((PureFn::Wrap, 1, 1)),
        "norm" if args.len() == 3 => Some((PureFn::NormFixed, 3, 3)),
        _ => None,
    };
    if let Some((f, min, max)) = pure {
        arity(name, args, min, max, span)?;
        return Ok(Node::Pure(f, lower_args(args, lw)?));
    }

    // Stateful functions.
    let (min, max, window_arg) = match name {
        "smooth" | "ema" | "hold" | "onset" | "walk" => (2, 2, None),
        "derivative" | "norm" | "every" => (1, 1, None),
        "change" => (1, 2, None),
        "rate" | "count" | "sum" | "lag" | "maxw" | "minw" => (2, 2, Some(1)),
        "random" => (0, 0, None),
        _ => {
            let mut msg = format!("unknown function `{name}`");
            if let Some(s) = suggest(
                name,
                PURE_FUNCTIONS
                    .iter()
                    .chain(STATEFUL_FUNCTIONS.iter())
                    .copied(),
            ) {
                msg.push_str(&format!(" (did you mean `{s}`?)"));
            }
            msg.push_str(&format!(
                ". Pure: {}. Stateful: {}",
                PURE_FUNCTIONS.join(", "),
                STATEFUL_FUNCTIONS.join(", ")
            ));
            return Err(ExprError::at(name_span, msg));
        }
    };
    if name == "norm" && args.len() != 1 {
        return Err(ExprError::at(
            span,
            format!(
                "`norm` takes 1 argument (adaptive) or 3 (`norm(x, lo, hi)`), got {}",
                args.len()
            ),
        ));
    }
    arity(name, args, min, max, span)?;
    if lw.scope.event_ctx {
        return Err(ExprError::at(
            span,
            format!("`{name}` keeps state between ticks and cannot be used in on_event expressions; compute it as a signal and reference it by name"),
        ));
    }
    let window = match window_arg {
        Some(i) => {
            let w = const_arg(name, "window", &args[i])?;
            if w <= 0.0 {
                return Err(ExprError::at(
                    args[i].span(),
                    format!("`{name}` window must be positive"),
                ));
            }
            w
        }
        None => 0.0,
    };
    let site = *lw.call_sites;
    *lw.call_sites += 1;
    let f = Stateful::new(name, window, site_seed(lw.seed, site)).ok_or_else(|| {
        ExprError::at(
            name_span,
            format!("internal: `{name}` has no state constructor"),
        )
    })?;
    Ok(Node::Stateful {
        f: Box::new(f),
        args: lower_args(args, lw)?,
    })
}

/// Collect the signal names referenced by bare identifiers in `expr`.
pub(crate) fn referenced_signals(expr: &Expr, signals: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    expr.walk(&mut |e| {
        if let Expr::Ident { name, .. } = e {
            if signals.iter().any(|s| s == name) && !out.contains(name) {
                out.push(name.clone());
            }
        }
    });
    out
}

/// Timing inputs for one control tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    /// Engine time in seconds.
    pub now: Seconds,
    /// Seconds since the previous tick (nominally 0.02).
    pub dt: f64,
    /// Transport position in beats (fractional).
    pub beat: f64,
    /// Transport position in bars (fractional).
    pub bar: f64,
    /// Current tempo.
    pub bpm: f64,
}

impl Tick {
    /// Convenience: a tick at `now` with the nominal 20 ms step and no transport.
    pub fn at(now: Seconds) -> Self {
        Self {
            now,
            dt: 0.02,
            beat: 0.0,
            bar: 0.0,
            bpm: 0.0,
        }
    }
}

/// A single compiled expression with its own data environment.
///
/// Used for validation and for one-off expressions outside a [`crate::LayerPipeline`].
/// Feed it readings/events, then call [`Program::tick`] once per control tick.
#[derive(Debug, Clone)]
pub struct Program {
    env: Env,
    node: Node,
    scope: Scope,
    signal_map: BTreeMap<String, f64>,
}

impl Program {
    /// Whether the top-level node is an edge detector (`onset`, `every`, `change`).
    pub fn is_edge_detector(&self) -> bool {
        self.node.is_edge_detector()
    }
    /// Source channels this expression reads (via `field(..)` or bare identifiers).
    pub fn field_names(&self) -> Vec<String> {
        self.env.field_names.clone()
    }
    /// Event name patterns this expression reads via `event(..)`.
    pub fn event_names(&self) -> Vec<String> {
        self.env.event_patterns.clone()
    }
    /// Signals referenced by name.
    pub fn signal_names(&self) -> Vec<String> {
        self.env.signal_names.clone()
    }
    /// Does the expression use any stateful builtin?
    pub fn is_stateful(&self) -> bool {
        let mut any = false;
        self.node
            .walk(&mut |n| any |= matches!(n, Node::Stateful { .. }));
        any
    }
    /// Update the latest value of referenced channels (others are ignored).
    pub fn push_reading(&mut self, values: &BTreeMap<String, f64>) {
        self.env.push_reading(values);
    }
    /// Record an event for the next tick.
    pub fn push_event(&mut self, name: &str, intensity: f64) {
        self.env.push_event(name, intensity);
    }
    /// Set the current value of a signal referenced by name. Returns `false` if the
    /// expression does not reference it.
    pub fn set_signal(&mut self, name: &str, value: f64) -> bool {
        match self.env.signal_names.iter().position(|n| n == name) {
            Some(i) => {
                self.env.signal_values[i] = value;
                self.signal_map.insert(name.to_string(), value);
                true
            }
            None => false,
        }
    }
    /// Evaluate for one control tick (signal mode). Non-finite results become 0.
    pub fn tick(&mut self, t: &Tick) -> f64 {
        let ctx = Ctx {
            now: t.now,
            dt: t.dt,
            beat: t.beat,
            bar: t.bar,
            bpm: t.bpm,
            env: &self.env,
            signals: SignalView::Slice(&self.env.signal_values),
            intensity: 0.0,
            attrs: None,
        };
        let v = crate::node::finite(self.node.eval(&ctx));
        self.env.clear_events();
        v
    }
    /// Evaluate in event mode (only meaningful if compiled with `Scope::event_ctx`).
    pub fn eval_event(&self, ctx: &EventCtx) -> f64 {
        let c = Ctx {
            now: ctx.time,
            dt: 0.0,
            beat: ctx.beat,
            bar: 0.0,
            bpm: 0.0,
            env: &self.env,
            signals: SignalView::Map(ctx.signals),
            intensity: ctx.intensity,
            attrs: Some(ctx.attrs),
        };
        crate::node::finite(self.node.eval_pure(&c))
    }
    /// The scope this program was compiled with.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
}

/// Compile one expression against `scope`. Stateful call-sites are seeded from 0;
/// use [`compile_seeded`] to choose the seed.
pub fn compile(src: &str, scope: &Scope) -> Result<Program, ExprError> {
    compile_seeded(src, scope, 0)
}

/// Like [`compile`] with an explicit RNG seed for `random()` / `walk()`.
pub fn compile_seeded(src: &str, scope: &Scope, seed: u64) -> Result<Program, ExprError> {
    let ast = parse(src)?;
    let mut env = Env::default();
    let mut sites = 0;
    let node = lower(
        &ast,
        &mut Lowerer {
            env: &mut env,
            scope,
            seed,
            call_sites: &mut sites,
        },
    )?;
    Ok(Program {
        env,
        node,
        scope: scope.clone(),
        signal_map: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("smooth", "smoothe"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
