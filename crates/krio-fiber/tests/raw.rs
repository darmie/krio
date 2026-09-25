//! The allocation-free `raw` layer: contexts on caller-provided stacks,
//! switching directly between peers without returning through the host.

#![cfg(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    target_arch = "aarch64"
))]

use krio_fiber::raw::{Context, switch};

/// Everything a test's contexts share, reached through `arg`.
struct World {
    host: Context,
    a: Context,
    b: Context,
    log: Vec<&'static str>,
}

fn world<'w>(arg: *mut u8) -> &'w mut World {
    unsafe { &mut *(arg as *mut World) }
}

extern "C" fn a_entry(arg: *mut u8) -> ! {
    let w = world(arg);
    w.log.push("a1");
    // Straight to the peer, not back to the host.
    unsafe { switch(&mut w.a, &w.b) };
    let w = world(arg);
    w.log.push("a2");
    unsafe { switch(&mut w.a, &w.host) };
    unreachable!("a is not resumed after finishing");
}

extern "C" fn b_entry(arg: *mut u8) -> ! {
    let w = world(arg);
    w.log.push("b1");
    unsafe { switch(&mut w.b, &w.a) };
    unreachable!("b is not resumed after finishing");
}

#[test]
fn peers_transfer_directly() {
    let mut stack_a = vec![0u8; 64 * 1024];
    let mut stack_b = vec![0u8; 64 * 1024];
    let mut w = Box::new(World {
        host: Context::empty(),
        a: Context::empty(),
        b: Context::empty(),
        log: Vec::new(),
    });
    let arg = &mut *w as *mut World as *mut u8;
    unsafe {
        w.a = Context::new(&mut stack_a, a_entry, arg);
        w.b = Context::new(&mut stack_b, b_entry, arg);
        let World { host, a, .. } = &mut *w;
        switch(host, a);
    }
    assert_eq!(w.log, ["a1", "b1", "a2"]);
}

extern "C" fn counter(arg: *mut u8) -> ! {
    // Keeps a live local across many switches, and uses the FPU.
    let w = world(arg);
    let mut x = 0.5f64;
    for i in 0..1000 {
        x = x * 1.5 + i as f64;
        w.log.push(if i % 2 == 0 { "even" } else { "odd" });
        unsafe { switch(&mut w.a, &w.host) };
    }
    assert!(x.is_finite());
    w.log.push("done");
    unsafe { switch(&mut w.a, &w.host) };
    unreachable!();
}

#[test]
fn state_survives_many_round_trips() {
    let mut stack = vec![0u8; 64 * 1024];
    let mut w = Box::new(World {
        host: Context::empty(),
        a: Context::empty(),
        b: Context::empty(),
        log: Vec::new(),
    });
    let arg = &mut *w as *mut World as *mut u8;
    unsafe {
        w.a = Context::new(&mut stack, counter, arg);
        for _ in 0..1001 {
            let World { host, a, .. } = &mut *w;
            switch(host, a);
        }
    }
    assert_eq!(w.log.len(), 1001);
    assert_eq!(w.log[998], "even");
    assert_eq!(w.log[999], "odd");
    assert_eq!(w.log[1000], "done");
}
