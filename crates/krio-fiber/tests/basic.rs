//! End-to-end behavioural tests for `krio-fiber`. These actually
//! cross the asm context-switch boundary.

use krio_fiber::{
    Fiber, FiberState, FiberStep, current_fiber_id, is_cancelled, should_yield_early, take_input,
    yield_now, yield_value,
};
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
        vec![
            "before helper",
            "entered helper",
            "exited helper",
            "after helper"
        ]
    );
}

#[test]
#[should_panic(expected = "yield_now: called outside of any active fiber")]
fn yield_from_host_panics() {
    yield_now();
}

#[test]
#[should_panic(expected = "cannot resume a fiber in Done state")]
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

// ── State + error model ───────────────────────────────────────────

#[test]
fn fiber_state_transitions_through_resumes() {
    let mut fiber = Fiber::new(|| {
        yield_now();
    });
    assert_eq!(fiber.state(), FiberState::New);
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.state(), FiberState::Suspended);
    assert_eq!(fiber.resume(), FiberStep::Done);
    assert_eq!(fiber.state(), FiberState::Done);
}

#[test]
fn panicking_fiber_lands_in_errored_state() {
    let mut fiber = Fiber::new(|| {
        panic!("kaboom");
    });

    let step = fiber.resume();
    assert_eq!(step, FiberStep::Errored);
    assert_eq!(fiber.state(), FiberState::Errored);

    let payload = fiber.take_error().expect("error payload");
    let msg = payload.downcast::<&'static str>().expect("string panic");
    assert_eq!(*msg, "kaboom");

    // Subsequent take_error returns None.
    assert!(fiber.take_error().is_none());
}

#[test]
fn panic_after_yield_still_caught() {
    // Panicking after one or more yields — the trampoline still
    // catches the unwind, the asm switch is never crossed.
    let mut fiber = Fiber::new(|| {
        yield_now();
        yield_now();
        panic!("third time's the charm");
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.resume(), FiberStep::Errored);
    assert_eq!(fiber.state(), FiberState::Errored);

    let payload = fiber.take_error().unwrap();
    assert_eq!(
        *payload.downcast::<&'static str>().unwrap(),
        "third time's the charm"
    );
}

#[test]
#[should_panic(expected = "cannot resume a fiber in Errored state")]
fn resuming_errored_fiber_panics() {
    let mut fiber = Fiber::new(|| panic!("nope"));
    fiber.resume(); // Errored
    fiber.resume(); // panic
}

// ── Cancellation + deadlines ─────────────────────────────────────

#[test]
fn cancellation_observed_inside_fiber() {
    // The fiber polls is_cancelled() at each yield boundary and
    // returns early when set.
    let saw_cancel = Rc::new(RefCell::new(false));
    let saw_clone = saw_cancel.clone();

    let mut fiber = Fiber::new(move || {
        for _ in 0..10 {
            if is_cancelled() {
                *saw_clone.borrow_mut() = true;
                return;
            }
            yield_now();
        }
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    fiber.cancel();
    // Next resume — fiber observes the cancel and returns.
    assert_eq!(fiber.resume(), FiberStep::Done);
    assert!(*saw_cancel.borrow());
}

#[test]
fn cancellation_outside_fiber_is_false() {
    // From the host thread, is_cancelled() is just false (no panic).
    assert!(!is_cancelled());
    assert!(!should_yield_early());
}

#[test]
fn deadline_in_the_past_triggers_should_yield() {
    let mut fiber = Fiber::new(|| {
        // First yield — host sets deadline before resuming us again.
        yield_now();
        // Now should_yield_early() should be true.
        if should_yield_early() {
            return;
        }
        yield_now();
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    // Set a deadline in the distant past.
    fiber.set_deadline_ms(0.0);
    assert_eq!(fiber.resume(), FiberStep::Done);
}

// ── Bidirectional value passing ───────────────────────────────────

#[test]
fn yield_value_returns_resume_with_input() {
    // The fiber yields integers, the host doubles them and resumes.
    let mut fiber = Fiber::new(|| {
        let initial: i64 = take_input().expect("initial input");
        let next = yield_value::<i64, i64>(initial * 2).unwrap();
        let final_ = yield_value::<i64, i64>(next * 2).unwrap();
        // Final yield with no further continuation — fiber returns.
        let _: Option<i64> = yield_value::<i64, i64>(final_ * 2);
    });

    assert_eq!(fiber.resume_with(10i64), FiberStep::Yielded);
    assert_eq!(fiber.take_yield_value::<i64>(), Some(20));

    assert_eq!(fiber.resume_with(50i64), FiberStep::Yielded);
    assert_eq!(fiber.take_yield_value::<i64>(), Some(100));

    assert_eq!(fiber.resume_with(7i64), FiberStep::Yielded);
    assert_eq!(fiber.take_yield_value::<i64>(), Some(14));

    assert_eq!(fiber.resume(), FiberStep::Done);
}

#[test]
fn yield_value_returns_none_for_wrong_type() {
    let mut fiber = Fiber::new(|| {
        let received: Option<i64> = yield_value::<&'static str, i64>("hello");
        // Host sent a String, not an i64 — downcast fails.
        assert!(received.is_none());
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.take_yield_value::<&'static str>(), Some("hello"));
    assert_eq!(fiber.resume_with(String::from("oops")), FiberStep::Done);
}

#[test]
fn take_input_lets_first_call_receive_args() {
    // No yield_value yet — but resume_with set the input slot,
    // and take_input retrieves it.
    let mut fiber = Fiber::new(|| {
        let n: i64 = take_input().expect("first arg");
        assert_eq!(n, 42);
    });
    assert_eq!(fiber.resume_with(42i64), FiberStep::Done);
}

// ── Nested fibers (caller chain) ──────────────────────────────────

#[test]
fn fiber_can_resume_another_fiber_from_inside() {
    // Fiber A spawns + drives Fiber B from inside its own body.
    // Yields from B return to A (not the host); yields from A
    // return to the host. The caller-chain bookkeeping in
    // Fiber::resume's prev_active / prev_fiber save/restore is
    // what makes this work.
    let mut a = Fiber::new(|| {
        let mut b = Fiber::new(|| {
            yield_value::<&'static str, ()>("from b");
            yield_value::<&'static str, ()>("from b again");
        });

        // Drive B to its first yield.
        let step = b.resume();
        assert_eq!(step, FiberStep::Yielded);
        let from_b = b.take_yield_value::<&'static str>();
        assert_eq!(from_b, Some("from b"));

        // Yield to host between B's yields.
        yield_value::<&'static str, ()>("a midway");

        // Drive B to its second yield.
        let step = b.resume();
        assert_eq!(step, FiberStep::Yielded);
        assert_eq!(b.take_yield_value::<&'static str>(), Some("from b again"));

        // Drive B to completion.
        assert_eq!(b.resume(), FiberStep::Done);
    });

    assert_eq!(a.resume(), FiberStep::Yielded);
    assert_eq!(a.take_yield_value::<&'static str>(), Some("a midway"));
    assert_eq!(a.resume(), FiberStep::Done);
}

#[test]
fn nested_yield_doesnt_escape_to_outer_caller() {
    // Smoke check: when B (nested in A) yields, control returns
    // to A — not the host. If yield-targets were ever broken,
    // this would deadlock or skip A entirely.
    let visited = Rc::new(RefCell::new(Vec::<&'static str>::new()));
    let v = visited.clone();

    let mut a = Fiber::new(move || {
        v.borrow_mut().push("a-before");
        let mut b = Fiber::new({
            let v = v.clone();
            move || {
                v.borrow_mut().push("b-before");
                yield_now();
                v.borrow_mut().push("b-after");
            }
        });
        let _ = b.resume(); // B yields, control comes back here
        v.borrow_mut().push("a-middle");
        let _ = b.resume(); // B finishes
        v.borrow_mut().push("a-after");
    });

    assert_eq!(a.resume(), FiberStep::Done);
    assert_eq!(
        *visited.borrow(),
        vec!["a-before", "b-before", "a-middle", "b-after", "a-after"]
    );
}

#[test]
fn cancellation_persists_across_resumes() {
    // Once cancelled, is_cancelled() stays true until the fiber
    // exits — the host can't un-cancel.
    let observations = Rc::new(RefCell::new(Vec::new()));
    let observations_clone = observations.clone();

    let mut fiber = Fiber::new(move || {
        observations_clone.borrow_mut().push(is_cancelled());
        yield_now();
        observations_clone.borrow_mut().push(is_cancelled());
        yield_now();
        observations_clone.borrow_mut().push(is_cancelled());
    });

    assert_eq!(fiber.resume(), FiberStep::Yielded);
    fiber.cancel();
    assert_eq!(fiber.resume(), FiberStep::Yielded);
    assert_eq!(fiber.resume(), FiberStep::Done);
    assert_eq!(*observations.borrow(), vec![false, true, true]);
}

// ── Identity + name ───────────────────────────────────────────────

#[test]
fn fiber_ids_are_unique() {
    let a = Fiber::new(|| {});
    let b = Fiber::new(|| {});
    let c = Fiber::new(|| {});
    assert_ne!(a.id(), b.id());
    assert_ne!(b.id(), c.id());
    assert_ne!(a.id(), c.id());
}

#[test]
fn current_fiber_id_matches_inside() {
    let observed = Rc::new(RefCell::new(0u64));
    let observed_clone = observed.clone();

    let mut fiber = Fiber::new(move || {
        *observed_clone.borrow_mut() = current_fiber_id().unwrap();
    });
    let expected = fiber.id();
    fiber.resume();
    assert_eq!(*observed.borrow(), expected);
}

#[test]
fn current_fiber_id_is_none_on_host() {
    assert!(current_fiber_id().is_none());
}

#[test]
fn fiber_name_round_trips() {
    let mut fiber = Fiber::new(|| {});
    assert_eq!(fiber.name(), None);
    fiber.set_name("worker-1");
    assert_eq!(fiber.name(), Some("worker-1"));
    fiber.set_name(String::from("renamed"));
    assert_eq!(fiber.name(), Some("renamed"));
}

#[test]
fn take_yield_value_wrong_type_preserves_slot() {
    // The fiber yields an i64. Calling `take_yield_value::<&str>`
    // must not consume the value — a follow-up
    // `take_yield_value::<i64>` should still see it. Without the
    // restore-on-mismatch behaviour the value would silently
    // disappear on the first wrong-type take.
    let mut fiber = Fiber::new(|| {
        let _: Option<()> = yield_value(42i64);
    });
    fiber.resume();

    // First peek with the wrong type — should return None and leave
    // the slot intact.
    let wrong: Option<&str> = fiber.take_yield_value();
    assert_eq!(wrong, None);

    // Now the right type — should still find the value.
    let right: Option<i64> = fiber.take_yield_value();
    assert_eq!(right, Some(42));

    // After a successful take the slot is empty again.
    let again: Option<i64> = fiber.take_yield_value();
    assert_eq!(again, None);
}

#[test]
fn has_yield_value_does_not_consume() {
    let mut fiber = Fiber::new(|| {
        let _: Option<()> = yield_value("hello");
    });
    fiber.resume();
    assert!(fiber.has_yield_value());
    // Repeated peeks stay observable.
    assert!(fiber.has_yield_value());
    let v: Option<&str> = fiber.take_yield_value();
    assert_eq!(v, Some("hello"));
    assert!(!fiber.has_yield_value());
}

#[test]
fn take_yield_any_returns_opaque_box() {
    let mut fiber = Fiber::new(|| {
        let _: Option<()> = yield_value(7u32);
    });
    fiber.resume();
    let any = fiber.take_yield_any().expect("yield value present");
    let v: u32 = *any.downcast::<u32>().expect("downcast u32");
    assert_eq!(v, 7);
    // After take_yield_any the slot is empty.
    assert!(!fiber.has_yield_value());
}

// ── GC introspection: saved frame-pointer + return address ──────────

#[test]
fn saved_fp_is_none_for_new_and_done_fibers() {
    let fiber = Fiber::new(|| {});
    assert_eq!(fiber.state(), FiberState::New);
    assert!(fiber.saved_fp().is_none());
    assert!(fiber.saved_ret().is_none());

    let mut fiber = Fiber::new(|| {});
    fiber.resume();
    assert_eq!(fiber.state(), FiberState::Done);
    assert!(fiber.saved_fp().is_none());
    assert!(fiber.saved_ret().is_none());
}

#[test]
fn saved_fp_points_into_fiber_stack_when_suspended() {
    let mut fiber = Fiber::new(|| {
        yield_now();
    });
    fiber.resume();
    assert_eq!(fiber.state(), FiberState::Suspended);

    let saved_fp = fiber.saved_fp().expect("suspended fiber has saved fp");
    let saved_ret = fiber.saved_ret().expect("suspended fiber has saved ret");

    // saved_fp should be a plausible heap pointer, NOT null, NOT
    // word-misaligned (frame pointers must be 8-byte-aligned).
    assert!(!saved_fp.is_null(), "saved_fp must not be null");
    assert_eq!(
        (saved_fp as usize) & 7,
        0,
        "saved_fp must be 8-byte aligned"
    );
    // saved_ret points into executable code (the trampoline /
    // yield_now's caller). Sanity: it's not null and not zero.
    assert!(!saved_ret.is_null(), "saved_ret must not be null");
}

#[test]
fn saved_fp_chain_terminates_cleanly() {
    // Walk the frame chain from the fiber's saved_fp and verify
    // every step decreases (frames grow downward) or hits null.
    // This is the exact pattern a GC walker uses to traverse a
    // suspended fiber's stack.
    let mut fiber = Fiber::new(|| {
        yield_now();
    });
    fiber.resume();

    let mut fp = fiber.saved_fp().expect("suspended fiber") as usize;
    let mut walked = 0;
    let max_frames = 64;
    while fp != 0 && walked < max_frames {
        walked += 1;
        // Frame chain invariant: aligned word.
        assert_eq!(fp & 7, 0, "frame pointer must be 8-byte aligned");
        // Read the saved fp at *fp (first word of the frame).
        let next_fp = unsafe { *(fp as *const usize) };
        // Either we've reached a sentinel (0 / null) marking the
        // top of the fiber's stack, or the chain continues with
        // a higher address (stack grows downward, so caller's fp
        // is higher than callee's).
        if next_fp == 0 {
            break;
        }
        // The next fp should be at a higher address (older frame).
        // If it's lower, the chain is corrupt — likely we've walked
        // off the end of the fiber's stack into garbage.
        assert!(
            next_fp > fp,
            "frame chain went backwards: next_fp={next_fp:#x} < fp={fp:#x} at step {walked}"
        );
        fp = next_fp;
    }
    assert!(walked > 0, "walker must traverse at least one frame");
    assert!(walked < max_frames, "walker must terminate (max_frames hit)");
}
