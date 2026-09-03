//! Behaviour that holds on every target, so it can be checked on the
//! development machine rather than only under a wasm engine.
//!
//! The statics behind the clock and the spawn hook are process-global,
//! and `cargo test` runs test functions concurrently in one process, so
//! each area gets exactly one test function rather than several that
//! would race each other.

use krio_core::Clock;
use krio_runtime::AgentId;
use krio_wasm::{EpochClock, SpawnError, advance_epoch_ms, epoch_ms, publish_epoch_ms};

#[test]
fn the_epoch_never_runs_backwards() {
    // A ticker publishes absolute readings.
    publish_epoch_ms(1_000);
    assert_eq!(epoch_ms(), 1_000);

    publish_epoch_ms(1_500);
    assert_eq!(epoch_ms(), 1_500);

    // A late or duplicated reading must be ignored, not applied. A
    // deadline that moves backwards turns into a fiber that never
    // yields, which is far harder to diagnose than a stalled clock.
    let after_stale = publish_epoch_ms(1_200);
    assert_eq!(after_stale, 1_500);
    assert_eq!(epoch_ms(), 1_500);

    // A ticker that only knows its own interval bumps instead.
    let after_bump = advance_epoch_ms(16);
    assert_eq!(after_bump, 1_516);

    // And the Clock impl reads the same counter.
    assert_eq!(EpochClock::new().now_ms(), 1_516.0);
}

#[test]
fn spawning_without_a_host_says_so_rather_than_guessing() {
    // A wasm module cannot invent a way to start a Worker, and this is
    // the error a host sees if it forgets to say how.
    let err = krio_wasm::spawn_agent(AgentId(1)).unwrap_err();
    assert_eq!(err, SpawnError::NoHost);
    assert!(!krio_wasm::can_spawn());

    krio_wasm::set_spawn(|agent| {
        if agent.0 < 4 {
            Ok(())
        } else {
            Err(SpawnError::Refused)
        }
    });

    assert!(krio_wasm::can_spawn());
    assert_eq!(krio_wasm::spawn_agent(AgentId(1)), Ok(()));
    assert_eq!(
        krio_wasm::spawn_agent(AgentId(9)),
        Err(SpawnError::Refused),
        "a host that declines must be able to say so"
    );
}

#[test]
fn a_build_without_atomics_refuses_to_claim_cluster_support() {
    let support = krio_wasm::cluster_support();

    if cfg!(target_feature = "atomics") {
        let support = support.expect("atomics is on; support must be reported");
        assert!(support.atomics);
    } else {
        // The point of returning Option: a host writing `?` here cannot
        // accidentally start a cluster on a build that cannot host one.
        assert!(
            support.is_none(),
            "a build without atomics must not claim it can cluster"
        );
    }
}
