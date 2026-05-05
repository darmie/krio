//! End-to-end behavioural tests for `krio-fiber`. These actually
//! cross the asm context-switch boundary.

use krio_fiber::{Fiber, FiberStep, yield_now};
use std::cell::RefCell;
use std::rc::Rc;

#[test]
fn no_yield_runs_to_done_in_one_resume() {
    let log = Rc::new(RefCell::new(Vec::<&'static str>::new()));
    let log_clone = log.clone();

    let mut fiber = Fiber::new(move || {
        log_clone.borrow_mut().push("ran");
    });

    assert_eq!(fiber.resume(), FiberStep::Done);
    assert_eq!(*log.borrow(), vec!["ran"]);
}

#[test]
fn three_yields_produces_four_resumes() {
    let log = Rc::new(RefCell::new(Vec::<&'static str>::new()));
    let log_clone = log.clone();

    let mut fiber = Fiber::new(move || {
        log_clone.borrow_mut().push("step 1");
        yield_now();
        log_clone.borrow_mut().push("step 2");
        yield_now();
        log_clone.borrow_mut().push("step 3");
        yield_now();
        log_clone.borrow_mut().push("step 4");
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(*log.borrow(), vec!["step 1"]);
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(*log.borrow(), vec!["step 1", "step 2"]);
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(*log.borrow(), vec!["step 1", "step 2", "step 3"]);
    assert_eq!(fiber.resume(), FiberStep::Done);
    assert_eq!(*log.borrow(), vec!["step 1", "step 2", "step 3", "step 4"]);
}

#[test]
fn yield_works_from_nested_call() {
    // The whole point of stackful fibers: yielding from any depth.
    let log = Rc::new(RefCell::new(Vec::<&'static str>::new()));
    let log_clone = log.clone();

    fn deep_helper(log: &Rc<RefCell<Vec<&'static str>>>) {
        log.borrow_mut().push("entered helper");
        yield_now();
        log.borrow_mut().push("exited helper");
    }

    let mut fiber = Fiber::new(move || {
        log_clone.borrow_mut().push("before helper");
        deep_helper(&log_clone);
        log_clone.borrow_mut().push("after helper");
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(*log.borrow(), vec!["before helper", "entered helper"]);
    assert_eq!(fiber.resume(), FiberStep::Done);
    assert_eq!(
        *log.borrow(),
        vec!["before helper", "entered helper", "exited helper", "after helper"]
    );
}

#[test]
#[should_panic(expected = "yield_now: called outside of any active fiber")]
fn yield_from_host_panics() {
    yield_now();
}

#[test]
#[should_panic(expected = "cannot resume a fiber that has Done")]
fn resuming_done_fiber_panics() {
    let mut fiber = Fiber::new(|| {});
    fiber.resume(); // Done
    fiber.resume(); // panic
}

#[test]
fn many_round_trips() {
    // Hammer the switch path — catches stack-corruption regressions
    // that wouldn't show up in a 4-yield test.
    let count = Rc::new(RefCell::new(0u64));
    let count_clone = count.clone();

    let mut fiber = Fiber::new(move || {
        for _ in 0..1000 {
            *count_clone.borrow_mut() += 1;
            yield_now();
        }
    });

    for i in 1..=1000 {
        assert_eq!(fiber.resume(), FiberStep::Yielded);
        assert_eq!(*count.borrow(), i);
    }
    assert_eq!(fiber.resume(), FiberStep::Done);
}

#[test]
fn fibers_are_independent() {
    let mut a = Fiber::new(|| {
        yield_now();
        yield_now();
    });
    let mut b = Fiber::new(|| {
        yield_now();
    });

    assert_eq!(a.resume(), FiberStep::Yielded);
    assert_eq!(b.resume(), FiberStep::Yielded);
    assert_eq!(a.resume(), FiberStep::Yielded);
    assert_eq!(b.resume(), FiberStep::Done);
    assert_eq!(a.resume(), FiberStep::Done);
}

#[test]
fn explicit_stack_size() {
    let mut fiber = Fiber::with_stack_size(4096, || {
        yield_now();
    });
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.resume(), FiberStep::Done);
}
