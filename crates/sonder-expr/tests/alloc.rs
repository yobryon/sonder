//! The steady-state `tick()` path must not allocate (fired edge detectors excepted).

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use sonder_expr::LayerPipeline;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY-free: this is a plain forwarding allocator. `unsafe impl` is required by the
// trait; the crate itself forbids unsafe, this test binary is separate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn tick_is_allocation_free_after_warmup() {
    let sig: BTreeMap<String, String> = [
        ("a", "norm(smooth(rate(event('car'), 10), 3))"),
        ("b", "norm(smooth(x, 4))"),
        ("c", "maxw(x, 2s) - minw(x, 2s)"),
        ("d", "sum(event('*'), 5s) + count(event('car'), 1s)"),
        ("e", "derivative(b) + lag(b, 1s)"),
        ("f", "hold(event('car'), 2s)"),
        ("g", "walk(0.1, 1s) + random()"),
        ("h", "onset(a, 1.5)"),
        ("i", "ema(x, 0.1) + hash(x)"),
        ("j", "if(a > b, clamp(c, 0, 1), lerp(0, 1, g))"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let map: BTreeMap<String, String> = [("energy", "a"), ("brightness", "clamp(b + c, 0, 1)")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut p = LayerPipeline::compile(&sig, &map, 1).unwrap();
    let reading = BTreeMap::from([("x".to_string(), 0.5)]);
    let attrs = BTreeMap::new();

    // Warm up: ring buffers are sized on the first tick.
    for i in 0..10u64 {
        p.push_event("car", 0.5, &attrs, i as f64 * 0.02);
        p.push_reading(&reading);
        p.tick(i as f64 * 0.02, 0.02, 0.0, 0.0, 60.0);
    }

    let before = ALLOCS.load(Ordering::Relaxed);
    for i in 10..2010u64 {
        let t = i as f64 * 0.02;
        p.push_event("car", 0.5, &attrs, t);
        p.push_event("bus", 0.2, &attrs, t);
        p.push_reading(&reading);
        let fired = p.tick(t, 0.02, 0.0, 0.0, 60.0);
        assert!(
            fired.is_empty(),
            "`a` never exceeds 1.0, so `onset(a, 1.5)` must not fire"
        );
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        0,
        "tick allocated {} times in 2000 ticks",
        after - before
    );
}
