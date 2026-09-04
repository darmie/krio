//! Suspension on targets that cannot switch their own stack.
//!
//! A wasm module has no addressable stack, so [`krio_fiber::Fiber`] is
//! unavailable there and says so. Suspension is a separate question: the
//! capability exists one level up in the host, so `yield_now` routes to
//! it. This file checks the routing, and — just as importantly — that
//! nothing silently pretends to suspend when no host has volunteered.
//!
//! Runs on any target without a native context switch. In practice that
//! is wasm under wasmtime:
//!
//! ```text
//! CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime \
//!   cargo test -p krio-fiber --target wasm32-wasip1 --test host_suspend
//! ```

#![cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
)))]

use std::sync::atomic::{AtomicUsize, Ordering};

static SUSPENSIONS: AtomicUsize = AtomicUsize::new(0);

/// Stands in for what a browser installs. JSPI suspends the whole call
/// here and resumes it when a promise settles; wasmtime cannot suspend,
/// so this only records that the point was reached — which is the
/// documented degradation, and a property of the host rather than of
/// the program.
fn host_suspend() {
    SUSPENSIONS.fetch_add(1, Ordering::Relaxed);
}

/// Library code written against krio's free functions, with no idea
/// whether it is on a native fiber or a wasm host. This is the shape
/// that matters: these calls are scattered through a language's
/// standard library, far from anything that knows about scheduling.
fn cooperative_work(n: u32) -> u32 {
    let mut acc = 0;
    for i in 0..n {
        acc += i;
        if krio_fiber::should_yield_early() {
            break;
        }
        krio_fiber::yield_now();
    }
    acc
}

// One test function, not several: the suspender is process-global and
// `cargo test` runs test functions concurrently in one process.
#[test]
fn yield_now_routes_to_the_host_once_one_volunteers() {
    assert!(
        !krio_fiber::has_suspender(),
        "nothing should be installed before the host asks for it"
    );

    krio_fiber::set_suspender(host_suspend);
    assert!(krio_fiber::has_suspender());

    let before = SUSPENSIONS.load(Ordering::Relaxed);
    let total = cooperative_work(5);

    assert_eq!(total, 0 + 1 + 2 + 3 + 4);
    assert_eq!(
        SUSPENSIONS.load(Ordering::Relaxed) - before,
        5,
        "every yield point must reach the host"
    );

    // The polling accessors keep working off the fiber path: they report
    // "no active fiber" rather than panicking, which is what lets the
    // same library code compile for both worlds.
    assert!(!krio_fiber::is_cancelled());
    assert!(!krio_fiber::is_deadline_passed());
    assert!(!krio_fiber::should_yield_early());
    assert_eq!(krio_fiber::current_fiber_id(), None);
}

/// `Fiber` itself stays unavailable, and loudly so. A fiber that looked
/// like it worked and then diverged on `yield_value` would be worse
/// than one that refuses.
#[test]
#[should_panic(expected = "native fibers are unavailable on this target")]
fn fiber_still_refuses_to_pretend() {
    let _ = krio_fiber::Fiber::new(|| {});
}
