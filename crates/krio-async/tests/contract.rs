//! Phase 1 contract smoke. Just checks that a host can wire its
//! types to the trait surface and call `transform_to_state_machine`
//! — no real lowering is exercised yet.

use krio_async::{FrameState, SuspendingFns, TransformError, transform_to_state_machine};
use std::collections::HashSet;

// Tiny "host" types — the kind of thing a real compiler would have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyBlockId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ToyValueId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ToyFnId(u32);

struct ToySuspending {
    suspending: HashSet<ToyFnId>,
    yields: HashSet<ToyFnId>,
}

impl SuspendingFns for ToySuspending {
    type FnId = ToyFnId;
    fn is_suspending(&self, fn_id: ToyFnId) -> bool {
        self.suspending.contains(&fn_id)
    }
    fn is_yield_primitive(&self, fn_id: ToyFnId) -> bool {
        self.yields.contains(&fn_id)
    }
}

#[test]
fn non_suspending_fn_returns_trivial_layout() {
    let mut cfg = ToyBlockId(0); // dummy
    let suspending = ToySuspending {
        suspending: HashSet::new(),
        yields: HashSet::new(),
    };

    let layout = transform_to_state_machine::<ToyBlockId, ToyValueId, ToyFnId, _>(
        &mut cfg,
        ToyFnId(42),
        &suspending,
    )
    .expect("non-suspending fn should produce a trivial Ok layout");
    assert!(layout.resume_entries.is_empty());
    assert!(layout.yield_blocks.is_empty());
    assert!(layout.block_kinds.is_empty());
}

#[test]
fn suspending_fn_currently_returns_unimplemented() {
    // Phase 1 stub. When Phase 2 lands, this test will be deleted /
    // replaced with a real layout assertion.
    let mut cfg = ToyBlockId(0);
    let mut suspending_set = HashSet::new();
    suspending_set.insert(ToyFnId(7));

    let suspending = ToySuspending {
        suspending: suspending_set,
        yields: HashSet::new(),
    };

    let result = transform_to_state_machine::<ToyBlockId, ToyValueId, ToyFnId, _>(
        &mut cfg,
        ToyFnId(7),
        &suspending,
    );
    assert!(matches!(result, Err(TransformError::Unimplemented)));
}

#[test]
fn frame_state_default_is_state_zero() {
    let frame: FrameState<u64> = FrameState::default();
    assert_eq!(frame.state_id, 0);
    assert!(frame.saved_values.is_empty());
}
