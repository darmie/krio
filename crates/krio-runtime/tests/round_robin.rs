//! Round-robin scheduler driving a mix of fiber tasks.

use krio_fiber::{Fiber, yield_now};
use krio_runtime::{RoundRobin, Scheduler};
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn round_robin_drives_fibers_to_completion() {
    let log = Rc::new(RefCell::new(Vec::<String>::new()));

    let mut sched = RoundRobin::new();
    for id in 0..3 {
        let log = log.clone();
        sched.spawn(Box::new(Fiber::new(move || {
            log.borrow_mut().push(format!("fiber {id} a"));
            yield_now();
            log.borrow_mut().push(format!("fiber {id} b"));
            yield_now();
            log.borrow_mut().push(format!("fiber {id} c"));
        })));
    }

    assert_eq!(sched.task_count(), 3);
    sched.run_to_completion();
    assert_eq!(sched.task_count(), 0);

    // Round-robin order: every fiber's "a" before any "b", etc.
    let log = log.borrow();
    let positions: Vec<&String> = log.iter().collect();
    assert_eq!(positions.len(), 9);
    // First three are all "a" steps, second three "b", last three "c".
    for entry in &positions[0..3] {
        assert!(entry.ends_with("a"), "expected 'a' step, got {entry}");
    }
    for entry in &positions[3..6] {
        assert!(entry.ends_with("b"), "expected 'b' step, got {entry}");
    }
    for entry in &positions[6..9] {
        assert!(entry.ends_with("c"), "expected 'c' step, got {entry}");
    }
}

#[test]
fn empty_scheduler_terminates_immediately() {
    let mut sched = RoundRobin::new();
    sched.run_to_completion();
    assert_eq!(sched.task_count(), 0);
}

#[test]
fn finished_tasks_are_dropped_each_tick() {
    // Fiber A finishes immediately, fiber B yields once.
    let mut sched = RoundRobin::new();
    sched.spawn(Box::new(Fiber::new(|| {})));
    sched.spawn(Box::new(Fiber::new(|| {
        yield_now();
    })));

    assert_eq!(sched.task_count(), 2);
    let alive_after_first_tick = sched.tick();
    assert!(alive_after_first_tick, "expected fiber B still alive");
    assert_eq!(sched.task_count(), 1, "fiber A should have been dropped");

    let alive_after_second_tick = sched.tick();
    assert!(!alive_after_second_tick, "expected all tasks done");
    assert_eq!(sched.task_count(), 0);
}
