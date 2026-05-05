//! TimeSliceScheduler smoke tests. These hit the real asm context
//! switch via krio-fiber.

use krio_fiber::{Fiber, should_yield_early, yield_now};
use krio_preempt::TimeSliceScheduler;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

#[test]
fn empty_scheduler_terminates_immediately() {
    let mut sched = TimeSliceScheduler::new(Duration::from_millis(10));
    sched.run_to_completion();
    assert_eq!(sched.fiber_count(), 0);
}

#[test]
fn drives_yielding_fibers_to_completion() {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    let mut sched = TimeSliceScheduler::new(Duration::from_millis(50));

    for id in 0..3 {
        let log = log.clone();
        sched.spawn(Fiber::new(move || {
            log.borrow_mut().push(format!("f{id}-a"));
            yield_now();
            log.borrow_mut().push(format!("f{id}-b"));
            yield_now();
            log.borrow_mut().push(format!("f{id}-c"));
        }));
    }

    assert_eq!(sched.fiber_count(), 3);
    sched.run_to_completion();
    assert_eq!(sched.fiber_count(), 0);

    // Round-robin order: every fiber's "a" before "b" before "c".
    let log = log.borrow();
    assert_eq!(log.len(), 9);
    for entry in &log[0..3] {
        assert!(entry.ends_with("a"), "{entry}");
    }
    for entry in &log[3..6] {
        assert!(entry.ends_with("b"), "{entry}");
    }
    for entry in &log[6..9] {
        assert!(entry.ends_with("c"), "{entry}");
    }
}

#[test]
fn deadline_polling_yields_mid_slice() {
    // A fiber that polls should_yield_early() in a tight-ish loop
    // should yield when its slice expires. With a 1ms slice and a
    // loop that does small work + checks, we expect multiple
    // resumes before the loop's iteration target is hit.
    let counter = Rc::new(RefCell::new(0u32));
    let counter_clone = counter.clone();
    let target = 500u32;

    let mut sched = TimeSliceScheduler::new(Duration::from_millis(1));
    sched.spawn(Fiber::new(move || {
        loop {
            *counter_clone.borrow_mut() += 1;
            if *counter_clone.borrow() >= target {
                return;
            }
            if should_yield_early() {
                yield_now();
            }
        }
    }));

    // Cap the rounds so a buggy implementation doesn't hang the test.
    let mut rounds = 0;
    while sched.tick() {
        rounds += 1;
        assert!(rounds < 10_000, "scheduler ran way too many rounds");
    }
    assert_eq!(*counter.borrow(), target);
}

#[test]
fn finished_fibers_dropped_each_tick() {
    let mut sched = TimeSliceScheduler::new(Duration::from_millis(10));
    sched.spawn(Fiber::new(|| {})); // immediate-done
    sched.spawn(Fiber::new(|| {
        yield_now();
    }));

    assert_eq!(sched.fiber_count(), 2);
    let alive = sched.tick();
    assert!(alive, "fiber B still alive");
    assert_eq!(sched.fiber_count(), 1);

    let alive = sched.tick();
    assert!(!alive);
    assert_eq!(sched.fiber_count(), 0);
}
