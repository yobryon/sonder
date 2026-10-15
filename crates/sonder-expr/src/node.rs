//! Evaluator: a tree of nodes, each stateful call-site owning its own state.
//!
//! Nodes are produced by the compiler from the AST. Evaluation never allocates.

use std::collections::{BTreeMap, HashMap, VecDeque};

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sonder_core::Value;

use crate::ast::{BinOp, UnOp};

/// Half-life of the adaptive-normalization envelope, in seconds (~2 minutes).
pub const NORM_HALF_LIFE: f64 = 120.0;

/// Seconds over which `norm` fades from a neutral 0.5 to its adaptive output, so a
/// value that is simply rising from zero at startup doesn't read as "maximum".
pub const NORM_WARMUP: f64 = 30.0;
/// Upper bound on per-node ring buffer entries (bounded memory for huge windows).
pub const MAX_WINDOW_ENTRIES: usize = 1 << 16;

/// Replace non-finite values with 0 so NaN/inf never poison state or the music.
#[inline]
pub(crate) fn finite(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// 64-bit FNV-1a over bytes.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Stable hash of a string to 0..1 (FNV-1a, independent of std's `RandomState`).
pub fn hash_text(s: &str) -> f64 {
    unit_from_u64(fnv1a(s.as_bytes()))
}

/// Stable hash of a number to 0..1. `-0.0` hashes like `0.0`.
pub fn hash_num(x: f64) -> f64 {
    let x = if x == 0.0 { 0.0 } else { x };
    unit_from_u64(fnv1a(&x.to_bits().to_le_bytes()))
}

#[inline]
fn unit_from_u64(h: u64) -> f64 {
    // Top 53 bits → [0, 1).
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Seed for the `n`-th stateful call-site of a pipeline seeded with `seed` (splitmix64).
pub(crate) fn site_seed(seed: u64, site: usize) -> u64 {
    let mut z = seed ^ (site as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Does `pattern` match an event name? `*` matches anything, `foo*` is a prefix match.
pub(crate) fn event_matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

/// The shared data environment of a compiled program or pipeline: latest readings per
/// field, per-tick event accumulators, and signal values.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    pub field_names: Vec<String>,
    pub field_values: Vec<f64>,
    field_index: HashMap<String, usize>,
    pub event_patterns: Vec<String>,
    pub event_sum: Vec<f64>,
    pub event_count: Vec<f64>,
    pub signal_names: Vec<String>,
    pub signal_values: Vec<f64>,
}

impl Env {
    pub fn field_slot(&mut self, name: &str) -> usize {
        if let Some(&i) = self.field_index.get(name) {
            return i;
        }
        let i = self.field_names.len();
        self.field_names.push(name.to_string());
        self.field_values.push(0.0);
        self.field_index.insert(name.to_string(), i);
        i
    }

    pub fn event_slot(&mut self, pattern: &str) -> usize {
        if let Some(i) = self.event_patterns.iter().position(|p| p == pattern) {
            return i;
        }
        self.event_patterns.push(pattern.to_string());
        self.event_sum.push(0.0);
        self.event_count.push(0.0);
        self.event_patterns.len() - 1
    }

    pub fn signal_slot(&mut self, name: &str) -> usize {
        if let Some(i) = self.signal_names.iter().position(|p| p == name) {
            return i;
        }
        self.signal_names.push(name.to_string());
        self.signal_values.push(0.0);
        self.signal_names.len() - 1
    }

    pub fn push_reading(&mut self, values: &BTreeMap<String, f64>) {
        // Iterate whichever side is smaller; both are tiny in practice.
        if values.len() < self.field_names.len() {
            for (k, v) in values {
                if let Some(&i) = self.field_index.get(k.as_str()) {
                    self.field_values[i] = finite(*v);
                }
            }
        } else {
            for (i, name) in self.field_names.iter().enumerate() {
                if let Some(v) = values.get(name) {
                    self.field_values[i] = finite(*v);
                }
            }
        }
    }

    pub fn push_event(&mut self, name: &str, intensity: f64) {
        for (i, pat) in self.event_patterns.iter().enumerate() {
            if event_matches(pat, name) {
                self.event_sum[i] += finite(intensity);
                self.event_count[i] += 1.0;
            }
        }
    }

    pub fn clear_events(&mut self) {
        self.event_sum.iter_mut().for_each(|v| *v = 0.0);
        self.event_count.iter_mut().for_each(|v| *v = 0.0);
    }
}

/// Where signal values come from during evaluation.
#[derive(Clone, Copy)]
pub(crate) enum SignalView<'a> {
    /// Pipeline mode: indexed slice (indices resolved at compile time).
    Slice(&'a [f64]),
    /// Event mode: looked up by name in the composer-provided map.
    Map(&'a BTreeMap<String, f64>),
}

/// Everything a node may read during one evaluation.
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'a> {
    pub now: f64,
    pub dt: f64,
    pub beat: f64,
    pub bar: f64,
    pub bpm: f64,
    pub env: &'a Env,
    pub signals: SignalView<'a>,
    pub intensity: f64,
    pub attrs: Option<&'a BTreeMap<String, Value>>,
}

impl Ctx<'_> {
    fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs.and_then(|a| a.get(key))
    }
}

/// Pure (stateless) builtin functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PureFn {
    Abs,
    Min,
    Max,
    Clamp,
    Lerp,
    Pow,
    Sqrt,
    Exp,
    Log,
    Log2,
    Log10,
    Floor,
    Ceil,
    Round,
    Fract,
    Sign,
    Step,
    Smoothstep,
    Sin,
    Cos,
    Tan,
    Tanh,
    Wrap,
    /// `norm(x, lo, hi)` fixed-range normalization.
    NormFixed,
}

impl PureFn {
    pub fn apply(self, a: &[f64]) -> f64 {
        let x = a.first().copied().unwrap_or(0.0);
        match self {
            PureFn::Abs => x.abs(),
            PureFn::Min => a.iter().copied().fold(f64::INFINITY, f64::min),
            PureFn::Max => a.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            PureFn::Clamp => {
                let (lo, hi) = (a[1].min(a[2]), a[1].max(a[2]));
                x.clamp(lo, hi)
            }
            PureFn::Lerp => a[0] + (a[1] - a[0]) * a[2],
            PureFn::Pow => x.powf(a[1]),
            PureFn::Sqrt => x.max(0.0).sqrt(),
            PureFn::Exp => x.exp(),
            PureFn::Log => x.ln(),
            PureFn::Log2 => x.log2(),
            PureFn::Log10 => x.log10(),
            PureFn::Floor => x.floor(),
            PureFn::Ceil => x.ceil(),
            PureFn::Round => x.round(),
            PureFn::Fract => x - x.floor(),
            PureFn::Sign => {
                if x > 0.0 {
                    1.0
                } else if x < 0.0 {
                    -1.0
                } else {
                    0.0
                }
            }
            PureFn::Step => {
                if a[1] >= a[0] {
                    1.0
                } else {
                    0.0
                }
            }
            PureFn::Smoothstep => {
                let (e0, e1, v) = (a[0], a[1], a[2]);
                if e1 == e0 {
                    return if v >= e1 { 1.0 } else { 0.0 };
                }
                let t = ((v - e0) / (e1 - e0)).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            }
            PureFn::Sin => x.sin(),
            PureFn::Cos => x.cos(),
            PureFn::Tan => x.tan(),
            PureFn::Tanh => x.tanh(),
            PureFn::Wrap => x - x.floor(),
            PureFn::NormFixed => {
                let (lo, hi) = (a[1], a[2]);
                if (hi - lo).abs() < f64::EPSILON {
                    0.5
                } else {
                    ((x - lo) / (hi - lo)).clamp(0.0, 1.0)
                }
            }
        }
    }
}

/// Fixed-capacity ring buffer of per-tick values with a running total.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ring {
    buf: Vec<f64>,
    head: usize,
    len: usize,
    total: f64,
    window: f64,
}

impl Ring {
    fn new(window: f64) -> Self {
        Self {
            window,
            ..Default::default()
        }
    }
    /// Size the buffer on the first tick (needs `dt`). Capacity = ticks per window.
    fn ensure(&mut self, dt: f64) {
        if self.buf.is_empty() {
            let dt = if dt > 0.0 { dt } else { 0.02 };
            let n = (self.window / dt).ceil();
            let n = if n.is_finite() {
                (n as usize).clamp(1, MAX_WINDOW_ENTRIES)
            } else {
                1
            };
            self.buf = vec![0.0; n];
        }
    }
    fn push(&mut self, v: f64) {
        let cap = self.buf.len();
        if self.len == cap {
            let evicted = self.buf[self.head];
            self.total -= evicted;
            self.buf[self.head] = v;
            self.head = (self.head + 1) % cap;
        } else {
            let idx = (self.head + self.len) % cap;
            self.buf[idx] = v;
            self.len += 1;
        }
        self.total += v;
        // Periodically re-sum to kill floating drift (cheap: once per full wrap).
        if self.head == 0 && self.len == cap {
            self.total = self.buf.iter().sum();
        }
    }
    fn oldest(&self) -> f64 {
        if self.len == 0 {
            0.0
        } else {
            self.buf[self.head]
        }
    }
    fn total(&self) -> f64 {
        self.total
    }
    fn capacity(&self) -> usize {
        self.buf.len()
    }
}

/// Monotonic deque for sliding-window max (or min when `is_max == false`).
#[derive(Debug, Clone)]
pub(crate) struct Mono {
    dq: VecDeque<(u64, f64)>,
    tick: u64,
    window: f64,
    n: u64,
    is_max: bool,
}

impl Mono {
    fn new(window: f64, is_max: bool) -> Self {
        Self {
            dq: VecDeque::new(),
            tick: 0,
            window,
            n: 0,
            is_max,
        }
    }
    fn push(&mut self, v: f64, dt: f64) -> f64 {
        if self.n == 0 {
            let dt = if dt > 0.0 { dt } else { 0.02 };
            let n = (self.window / dt).ceil();
            self.n = if n.is_finite() {
                (n as u64).clamp(1, MAX_WINDOW_ENTRIES as u64)
            } else {
                1
            };
            self.dq.reserve(16);
        }
        while let Some(&(_, back)) = self.dq.back() {
            let dominated = if self.is_max { back <= v } else { back >= v };
            if dominated {
                self.dq.pop_back();
            } else {
                break;
            }
        }
        self.dq.push_back((self.tick, v));
        while let Some(&(t, _)) = self.dq.front() {
            if t + self.n <= self.tick {
                self.dq.pop_front();
            } else {
                break;
            }
        }
        self.tick += 1;
        self.dq.front().map(|&(_, v)| v).unwrap_or(v)
    }
}

/// Stateful builtins; each variant carries the state of one call-site.
#[derive(Debug, Clone)]
pub(crate) enum Stateful {
    Smooth {
        y: f64,
        init: bool,
    },
    Ema {
        y: f64,
        init: bool,
    },
    Derivative {
        prev: f64,
        prev_d: f64,
        init: bool,
    },
    Norm {
        min: f64,
        max: f64,
        init: bool,
        age: f64,
    },
    Rate(Ring),
    Count(Ring),
    Sum(Ring),
    Lag(Ring),
    Hold {
        y: f64,
    },
    MaxW(Mono),
    MinW(Mono),
    Onset {
        prev: f64,
        init: bool,
    },
    Every {
        acc: f64,
        init: bool,
    },
    Change {
        prev: f64,
        init: bool,
    },
    Random(ChaCha8Rng),
    Walk {
        rng: ChaCha8Rng,
        start: f64,
        target: f64,
        acc: f64,
        init: bool,
    },
}

impl Stateful {
    /// Construct a stateful node by builtin name. `args` are the constant arguments
    /// needed at construction (window sizes); `None` for names that are not stateful.
    pub fn new(name: &str, window: f64, seed: u64) -> Option<Self> {
        Some(match name {
            "smooth" => Stateful::Smooth {
                y: 0.0,
                init: false,
            },
            "ema" => Stateful::Ema {
                y: 0.0,
                init: false,
            },
            "derivative" => Stateful::Derivative {
                prev: 0.0,
                prev_d: 0.0,
                init: false,
            },
            "norm" => Stateful::Norm {
                min: 0.0,
                max: 0.0,
                init: false,
                age: 0.0,
            },
            "rate" => Stateful::Rate(Ring::new(window)),
            "count" => Stateful::Count(Ring::new(window)),
            "sum" => Stateful::Sum(Ring::new(window)),
            "lag" => Stateful::Lag(Ring::new(window)),
            "hold" => Stateful::Hold { y: 0.0 },
            "maxw" => Stateful::MaxW(Mono::new(window, true)),
            "minw" => Stateful::MinW(Mono::new(window, false)),
            "onset" => Stateful::Onset {
                prev: 0.0,
                init: false,
            },
            "every" => Stateful::Every {
                acc: 0.0,
                init: false,
            },
            "change" => Stateful::Change {
                prev: 0.0,
                init: false,
            },
            "random" => Stateful::Random(ChaCha8Rng::seed_from_u64(seed)),
            "walk" => Stateful::Walk {
                rng: ChaCha8Rng::seed_from_u64(seed),
                start: 0.5,
                target: 0.5,
                acc: 0.0,
                init: false,
            },
            _ => return None,
        })
    }

    pub fn is_edge_detector(&self) -> bool {
        matches!(
            self,
            Stateful::Onset { .. } | Stateful::Every { .. } | Stateful::Change { .. }
        )
    }

    /// Does this node need the impulse *count* of its first argument (vs. its value)?
    fn wants_count(&self) -> bool {
        matches!(self, Stateful::Rate(_) | Stateful::Count(_))
    }

    /// Advance one tick. `a` holds the evaluated arguments (sanitized), `count` the
    /// impulse count of the first argument.
    fn step(&mut self, a: &[f64; 3], count: f64, dt: f64) -> f64 {
        let x = a[0];
        match self {
            Stateful::Smooth { y, init } => {
                if !*init {
                    *init = true;
                    *y = x;
                } else {
                    let tau = a[1];
                    if tau <= 0.0 || dt <= 0.0 {
                        *y = x;
                    } else {
                        let k = 1.0 - (-dt / tau).exp();
                        *y += (x - *y) * k;
                    }
                }
                *y
            }
            Stateful::Ema { y, init } => {
                if !*init {
                    *init = true;
                    *y = x;
                } else {
                    let alpha = a[1].clamp(0.0, 1.0);
                    *y += (x - *y) * alpha;
                }
                *y
            }
            Stateful::Derivative { prev, prev_d, init } => {
                if !*init {
                    *init = true;
                    *prev = x;
                    *prev_d = 0.0;
                    return 0.0;
                }
                let d = if dt > 0.0 { (x - *prev) / dt } else { 0.0 };
                *prev = x;
                let out = 0.5 * (d + *prev_d);
                *prev_d = d;
                out
            }
            Stateful::Norm { min, max, init, age } => {
                if !*init {
                    *init = true;
                    *min = x;
                    *max = x;
                    return 0.5;
                }
                *age += dt.max(0.0);
                let confidence = (*age / NORM_WARMUP).clamp(0.0, 1.0);
                // Decay the envelope toward x with the configured half-life, then
                // expand it to include x.
                let k = if dt > 0.0 {
                    1.0 - (-dt * std::f64::consts::LN_2 / NORM_HALF_LIFE).exp()
                } else {
                    0.0
                };
                *min += (x - *min) * k;
                *max += (x - *max) * k;
                if x < *min {
                    *min = x;
                }
                if x > *max {
                    *max = x;
                }
                let range = *max - *min;
                let scale = 1.0 + min.abs().max(max.abs());
                let raw = if range <= 1e-9 * scale {
                    0.5
                } else {
                    ((x - *min) / range).clamp(0.0, 1.0)
                };
                0.5 + (raw - 0.5) * confidence
            }
            Stateful::Rate(ring) => {
                ring.ensure(dt);
                ring.push(count);
                let window = ring.capacity() as f64 * if dt > 0.0 { dt } else { 0.02 };
                ring.total() / window
            }
            Stateful::Count(ring) => {
                ring.ensure(dt);
                ring.push(count);
                ring.total()
            }
            Stateful::Sum(ring) => {
                ring.ensure(dt);
                ring.push(x);
                ring.total()
            }
            Stateful::Lag(ring) => {
                ring.ensure(dt);
                let full = ring.len == ring.capacity();
                let out = if full {
                    ring.oldest()
                } else if ring.len == 0 {
                    x
                } else {
                    ring.oldest()
                };
                ring.push(x);
                out
            }
            Stateful::Hold { y } => {
                let tau = a[1];
                let decayed = if tau > 0.0 && dt > 0.0 {
                    *y * (-dt / tau).exp()
                } else {
                    0.0
                };
                *y = x.max(decayed);
                *y
            }
            Stateful::MaxW(m) | Stateful::MinW(m) => m.push(x, dt),
            Stateful::Onset { prev, init } => {
                let th = a[1];
                let fired = *init && *prev < th && x >= th;
                *init = true;
                *prev = x;
                if fired {
                    1.0
                } else {
                    0.0
                }
            }
            Stateful::Every { acc, init } => {
                let period = x;
                if !*init {
                    *init = true;
                    *acc = 0.0;
                    return 1.0;
                }
                if period <= 0.0 {
                    return 1.0;
                }
                *acc += dt;
                if *acc >= period {
                    *acc -= period;
                    if *acc >= period {
                        // Missed several periods (huge dt): don't burst-fire later.
                        *acc %= period;
                    }
                    1.0
                } else {
                    0.0
                }
            }
            Stateful::Change { prev, init } => {
                let eps = a[1].max(0.0);
                let fired = *init && (x - *prev).abs() > eps;
                *init = true;
                *prev = x;
                if fired {
                    1.0
                } else {
                    0.0
                }
            }
            Stateful::Random(rng) => rng.random::<f64>(),
            Stateful::Walk {
                rng,
                start,
                target,
                acc,
                init,
            } => {
                let (step, period) = (x.abs(), a[1]);
                if !*init {
                    *init = true;
                    *target = pick_target(rng, *start, step);
                }
                if period <= 0.0 {
                    *start = *target;
                    *target = pick_target(rng, *start, step);
                    return *start;
                }
                *acc += dt;
                while *acc >= period {
                    *acc -= period;
                    *start = *target;
                    *target = pick_target(rng, *start, step);
                }
                let t = (*acc / period).clamp(0.0, 1.0);
                *start + (*target - *start) * t
            }
        }
    }
}

/// New random-walk target within `step` of `from`, reflected into 0..1.
fn pick_target(rng: &mut ChaCha8Rng, from: f64, step: f64) -> f64 {
    let delta = (rng.random::<f64>() * 2.0 - 1.0) * step;
    let mut t = from + delta;
    if t < 0.0 {
        t = -t;
    }
    if t > 1.0 {
        t = 2.0 - t;
    }
    t.clamp(0.0, 1.0)
}

/// One evaluator node.
#[derive(Debug, Clone)]
pub(crate) enum Node {
    Const(f64),
    Time,
    Dt,
    Beat,
    Bar,
    Bpm,
    Intensity,
    Signal {
        idx: usize,
        name: String,
    },
    Field(usize),
    Event(usize),
    /// `attr("k", default)`: numeric attr, text hashed unless numeric, bool 0/1.
    Attr {
        key: String,
        default: f64,
    },
    /// `has("k")`.
    Has(String),
    /// `attr_text("k") == "lit"` (or `!=`).
    StrEq {
        key: String,
        lit: String,
        negate: bool,
    },
    /// `hash(attr_text("k"))`.
    HashAttr(String),
    /// `attr_len("k")`: character length of a text attribute (0 if missing / non-text).
    AttrLen(String),
    /// `hash(<numeric expr>)`.
    HashNum(Box<Node>),
    Unary(UnOp, Box<Node>),
    Binary(BinOp, Box<Node>, Box<Node>),
    If(Box<Node>, Box<Node>, Box<Node>),
    Pure(PureFn, Vec<Node>),
    Stateful {
        f: Box<Stateful>,
        args: Vec<Node>,
    },
}

/// Scratch space for pure-function arguments (bounded, no heap).
const MAX_ARGS: usize = 8;

impl Node {
    pub fn is_edge_detector(&self) -> bool {
        matches!(self, Node::Stateful { f, .. } if f.is_edge_detector())
    }

    /// Impulse count of this node given its just-computed value.
    #[inline]
    fn impulse_count(&self, value: f64, ctx: &Ctx) -> f64 {
        match self {
            Node::Event(slot) => ctx.env.event_count[*slot],
            _ => {
                if value != 0.0 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Evaluate, advancing any stateful nodes by one tick.
    pub fn eval(&mut self, ctx: &Ctx) -> f64 {
        match self {
            Node::Stateful { f, args } => {
                let mut a = [0.0; 3];
                let mut count = 0.0;
                for (i, arg) in args.iter_mut().enumerate().take(3) {
                    let v = finite(arg.eval(ctx));
                    a[i] = v;
                    if i == 0 && f.wants_count() {
                        count = arg.impulse_count(v, ctx);
                    }
                }
                finite(f.step(&a, count, ctx.dt))
            }
            Node::HashNum(inner) => hash_num(inner.eval(ctx)),
            Node::Unary(op, rhs) => apply_unary(*op, rhs.eval(ctx)),
            Node::Binary(op, lhs, rhs) => {
                let l = lhs.eval(ctx);
                let r = rhs.eval(ctx);
                apply_binary(*op, l, r)
            }
            Node::If(c, a, b) => {
                // Both branches are evaluated so stateful nodes inside keep ticking.
                let c = c.eval(ctx);
                let a = a.eval(ctx);
                let b = b.eval(ctx);
                if c != 0.0 {
                    a
                } else {
                    b
                }
            }
            Node::Pure(f, args) => {
                let mut buf = [0.0; MAX_ARGS];
                let n = args.len().min(MAX_ARGS);
                for (slot, arg) in buf.iter_mut().zip(args.iter_mut()) {
                    *slot = arg.eval(ctx);
                }
                f.apply(&buf[..n])
            }
            other => other.eval_leaf(ctx),
        }
    }

    /// Evaluate without mutation. Only valid for trees without stateful nodes (the
    /// compiler guarantees this for event expressions); a stateful node yields 0.
    pub fn eval_pure(&self, ctx: &Ctx) -> f64 {
        match self {
            Node::Stateful { .. } => 0.0,
            Node::HashNum(inner) => hash_num(inner.eval_pure(ctx)),
            Node::Unary(op, rhs) => apply_unary(*op, rhs.eval_pure(ctx)),
            Node::Binary(op, lhs, rhs) => apply_binary(*op, lhs.eval_pure(ctx), rhs.eval_pure(ctx)),
            Node::If(c, a, b) => {
                if c.eval_pure(ctx) != 0.0 {
                    a.eval_pure(ctx)
                } else {
                    b.eval_pure(ctx)
                }
            }
            Node::Pure(f, args) => {
                let mut buf = [0.0; MAX_ARGS];
                let n = args.len().min(MAX_ARGS);
                for (slot, arg) in buf.iter_mut().zip(args.iter()) {
                    *slot = arg.eval_pure(ctx);
                }
                f.apply(&buf[..n])
            }
            other => other.eval_leaf(ctx),
        }
    }

    /// Leaves (no children, no state).
    fn eval_leaf(&self, ctx: &Ctx) -> f64 {
        match self {
            Node::Const(v) => *v,
            Node::Time => ctx.now,
            Node::Dt => ctx.dt,
            Node::Beat => ctx.beat,
            Node::Bar => ctx.bar,
            Node::Bpm => ctx.bpm,
            Node::Intensity => ctx.intensity,
            Node::Signal { idx, name } => match ctx.signals {
                SignalView::Slice(s) => s.get(*idx).copied().unwrap_or(0.0),
                SignalView::Map(m) => m.get(name).copied().unwrap_or(0.0),
            },
            Node::Field(i) => ctx.env.field_values[*i],
            Node::Event(i) => ctx.env.event_sum[*i],
            Node::Attr { key, default } => match ctx.attr(key) {
                None => *default,
                Some(Value::Num(n)) => finite(*n),
                Some(Value::Bool(b)) => {
                    if *b {
                        1.0
                    } else {
                        0.0
                    }
                }
                Some(Value::Text(t)) => t
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(finite)
                    .unwrap_or_else(|| hash_text(t)),
            },
            Node::AttrLen(key) => match ctx.attr(key) {
                Some(Value::Text(t)) => t.chars().count() as f64,
                Some(Value::Num(n)) => n.to_string().len() as f64,
                _ => 0.0,
            },
            Node::Has(key) => {
                if ctx.attr(key).is_some() {
                    1.0
                } else {
                    0.0
                }
            }
            Node::StrEq { key, lit, negate } => {
                let eq = match ctx.attr(key) {
                    Some(Value::Text(t)) => t == lit,
                    Some(Value::Num(n)) => lit.trim().parse::<f64>().ok() == Some(*n),
                    Some(Value::Bool(b)) => {
                        lit.eq_ignore_ascii_case(if *b { "true" } else { "false" })
                    }
                    None => false,
                };
                if eq != *negate {
                    1.0
                } else {
                    0.0
                }
            }
            Node::HashAttr(key) => match ctx.attr(key) {
                Some(Value::Text(t)) => hash_text(t),
                Some(Value::Num(n)) => hash_num(*n),
                Some(Value::Bool(b)) => hash_text(if *b { "true" } else { "false" }),
                None => 0.0,
            },
            // Composite nodes are handled by the callers.
            _ => 0.0,
        }
    }

    /// Visit every node in the tree.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Node)) {
        f(self);
        match self {
            Node::HashNum(n) | Node::Unary(_, n) => n.walk(f),
            Node::Binary(_, a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Node::If(a, b, c) => {
                a.walk(f);
                b.walk(f);
                c.walk(f);
            }
            Node::Pure(_, args) | Node::Stateful { args, .. } => {
                args.iter().for_each(|a| a.walk(f))
            }
            _ => {}
        }
    }
}

#[inline]
fn apply_unary(op: UnOp, v: f64) -> f64 {
    match op {
        UnOp::Neg => -v,
        UnOp::Not => {
            if v == 0.0 {
                1.0
            } else {
                0.0
            }
        }
    }
}

#[inline]
fn b(v: bool) -> f64 {
    if v {
        1.0
    } else {
        0.0
    }
}

#[inline]
fn apply_binary(op: BinOp, l: f64, r: f64) -> f64 {
    match op {
        BinOp::Add => l + r,
        BinOp::Sub => l - r,
        BinOp::Mul => l * r,
        BinOp::Div => {
            if r == 0.0 {
                0.0
            } else {
                l / r
            }
        }
        BinOp::Rem => {
            if r == 0.0 {
                0.0
            } else {
                l.rem_euclid(r)
            }
        }
        BinOp::Pow => finite(l.powf(r)),
        BinOp::Lt => b(l < r),
        BinOp::Le => b(l <= r),
        BinOp::Gt => b(l > r),
        BinOp::Ge => b(l >= r),
        BinOp::Eq => b(l == r),
        BinOp::Ne => b(l != r),
        BinOp::And => b(l != 0.0 && r != 0.0),
        BinOp::Or => b(l != 0.0 || r != 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_in_range() {
        let h = hash_text("car");
        assert!((0.0..1.0).contains(&h));
        assert_eq!(h, hash_text("car"));
        assert_ne!(h, hash_text("cat"));
        assert_eq!(hash_num(0.0), hash_num(-0.0));
        assert_ne!(hash_num(1.0), hash_num(2.0));
        // Pin a value so renders stay reproducible across releases.
        assert!((hash_text("") - 0.7967).abs() < 1e-3, "{}", hash_text(""));
    }

    #[test]
    fn event_pattern_matching() {
        assert!(event_matches("*", "anything"));
        assert!(event_matches("car", "car"));
        assert!(!event_matches("car", "cars"));
        assert!(event_matches("car*", "cars"));
        assert!(!event_matches("car*", "bus"));
    }

    #[test]
    fn ring_window_sum() {
        let mut r = Ring::new(0.1); // 5 ticks at 20ms
        r.ensure(0.02);
        assert_eq!(r.capacity(), 5);
        for v in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0] {
            r.push(v);
        }
        assert_eq!(r.total(), 2.0 + 3.0 + 4.0 + 5.0 + 6.0);
        assert_eq!(r.oldest(), 2.0);
    }

    #[test]
    fn mono_deque_windowed_max_and_min() {
        let mut m = Mono::new(0.06, true); // 3 ticks
        let outs: Vec<f64> = [1.0, 3.0, 2.0, 0.0, 0.0, 0.0]
            .iter()
            .map(|&v| m.push(v, 0.02))
            .collect();
        assert_eq!(outs, vec![1.0, 3.0, 3.0, 3.0, 2.0, 0.0]);
        let mut m = Mono::new(0.06, false);
        let outs: Vec<f64> = [3.0, 1.0, 2.0, 5.0, 5.0, 5.0]
            .iter()
            .map(|&v| m.push(v, 0.02))
            .collect();
        assert_eq!(outs, vec![3.0, 1.0, 1.0, 1.0, 2.0, 5.0]);
    }

    #[test]
    fn site_seeds_differ() {
        assert_ne!(site_seed(1, 0), site_seed(1, 1));
        assert_ne!(site_seed(1, 0), site_seed(2, 0));
        assert_eq!(site_seed(7, 3), site_seed(7, 3));
    }

    #[test]
    fn division_and_modulo_by_zero_are_zero() {
        assert_eq!(apply_binary(BinOp::Div, 1.0, 0.0), 0.0);
        assert_eq!(apply_binary(BinOp::Rem, 1.0, 0.0), 0.0);
        assert_eq!(apply_binary(BinOp::Rem, -1.0, 3.0), 2.0);
        assert_eq!(apply_binary(BinOp::Pow, -8.0, 0.5), 0.0);
    }
}
