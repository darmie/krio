//! Layout validation — a debug audit pass over [`StateMachineLayout`]
//! that catches misuse before the host emits broken codegen.
//!
//! Hosts that call [`crate::transform_to_state_machine`] should
//! invoke [`validate_layout`] in debug builds. It runs in O(n) on
//! the layout's vector lengths and produces structured errors via
//! [`LayoutError`] when it finds an inconsistency.
//!
//! ## What it catches
//!
//! - **Save without matching load**: a slot saved at a yield with no
//!   corresponding load at any resume — saved data is dead. Suggests
//!   the host's liveness analysis over-reports.
//! - **Load without matching save**: a slot loaded at a resume with
//!   no corresponding save anywhere — load reads garbage. Suggests
//!   the host's liveness analysis under-reports OR the host wrote a
//!   manual save somewhere that the layout doesn't reflect.
//! - **Duplicate slot per yield/resume**: the same slot index
//!   appears twice in one block's save list (or load list).
//!   Indicates the host's liveness has duplicate entries.
//! - **Resume entry / yield block count mismatch**: krio's contract
//!   is `resume_entries.len() == yield_blocks.len() + 1` (the +1 is
//!   the original entry block). A divergence is a transform-internal
//!   inconsistency — should never happen, but the audit catches it
//!   if it ever regresses.
//! - **Slot referenced beyond `next_slot`**: helpful when used with
//!   the slot-reservation API (so the host can sanity-check that
//!   krio respected the reservation).
//!
//! ## What it does NOT catch
//!
//! Out of scope:
//! - Whether the host's liveness is actually correct given the
//!   pre-transform CFG (krio doesn't model the host's CFG).
//! - Whether the host's downstream codegen rewires phi incoming
//!   entries correctly when blocks are split.
//! - Whether the host's `frame` SSA value is used consistently.
//!
//! These are documented in the README's "Host responsibilities"
//! section but can't be checked from the layout alone.

use alloc::collections::BTreeMap;
use core::fmt;

use krio_core::CfgId;

use crate::{FnId, StateMachineLayout};

/// Structured error returned by [`validate_layout`].
#[derive(Debug)]
#[non_exhaustive]
pub enum LayoutError<B: CfgId> {
    /// A slot is saved at one or more yield blocks but never loaded
    /// at any resume. Field: the offending slot index.
    SaveWithoutLoad { slot: u32 },
    /// A slot is loaded at one or more resume blocks but never saved
    /// at any yield. Field: the offending slot index.
    LoadWithoutSave { slot: u32 },
    /// The same slot index appears twice in a single block's save
    /// table (host's liveness has duplicate entries for that site).
    DuplicateSaveSlot { block: B, slot: u32 },
    /// The same slot index appears twice in a single block's load
    /// table.
    DuplicateLoadSlot { block: B, slot: u32 },
    /// `resume_entries.len() != yield_blocks.len() + 1`, indicating
    /// a transform-internal inconsistency.
    EntryCountMismatch {
        resume_entries: usize,
        yield_blocks: usize,
    },
    /// A slot index referenced in saves/loads is `>= next_slot`,
    /// indicating allocator drift.
    SlotOutOfRange { slot: u32, next_slot: u32 },
}

impl<B: CfgId> fmt::Display for LayoutError<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::SaveWithoutLoad { slot } => {
                write!(
                    f,
                    "slot {} is saved at a yield but never loaded at any resume \
                     — host's liveness analysis is over-reporting",
                    slot
                )
            }
            LayoutError::LoadWithoutSave { slot } => {
                write!(
                    f,
                    "slot {} is loaded at a resume but never saved at any yield \
                     — host's liveness analysis is under-reporting",
                    slot
                )
            }
            LayoutError::DuplicateSaveSlot { block, slot } => {
                write!(
                    f,
                    "slot {} appears twice in yield_saves for block {:?}",
                    slot, block
                )
            }
            LayoutError::DuplicateLoadSlot { block, slot } => {
                write!(
                    f,
                    "slot {} appears twice in resume_loads for block {:?}",
                    slot, block
                )
            }
            LayoutError::EntryCountMismatch {
                resume_entries,
                yield_blocks,
            } => {
                write!(
                    f,
                    "resume_entries.len()={} but yield_blocks.len()+1={} — \
                     transform-internal inconsistency",
                    resume_entries,
                    yield_blocks + 1
                )
            }
            LayoutError::SlotOutOfRange { slot, next_slot } => {
                write!(
                    f,
                    "slot {} is referenced but next_slot={} — allocator drift",
                    slot, next_slot
                )
            }
        }
    }
}

/// Validate a [`StateMachineLayout`] for internal consistency.
///
/// Call this in debug builds after [`crate::transform_to_state_machine`]
/// returns. Returns `Ok(())` on a clean layout. Returns the FIRST
/// error encountered — the caller can re-validate after fixing it,
/// since some errors mask others.
///
/// `next_slot` is the upper bound for slot indices: pass the highest
/// slot the host expects krio to have allocated (typically max over
/// `(yield_saves ∪ resume_loads)[*][*].0`, or `next_slot` from the
/// transform if the host preserved it). Pass `u32::MAX` to disable
/// the [`LayoutError::SlotOutOfRange`] check.
pub fn validate_layout<B, V, F>(
    layout: &StateMachineLayout<B, V, F>,
    next_slot: u32,
) -> Result<(), LayoutError<B>>
where
    B: CfgId,
    V: CfgId,
    F: FnId,
{
    // 1. Resume entry / yield block count check.
    if layout.resume_entries.len() != layout.yield_blocks.len() + 1 {
        return Err(LayoutError::EntryCountMismatch {
            resume_entries: layout.resume_entries.len(),
            yield_blocks: layout.yield_blocks.len(),
        });
    }

    // 2. Within each yield block, check no duplicate slot.
    for (block, entries) in &layout.yield_saves {
        let mut seen: BTreeMap<u32, ()> = BTreeMap::new();
        for (slot, _v) in entries {
            if seen.insert(*slot, ()).is_some() {
                return Err(LayoutError::DuplicateSaveSlot {
                    block: *block,
                    slot: *slot,
                });
            }
            if *slot >= next_slot {
                return Err(LayoutError::SlotOutOfRange {
                    slot: *slot,
                    next_slot,
                });
            }
        }
    }

    // 3. Within each resume block, check no duplicate slot.
    for (block, entries) in &layout.resume_loads {
        let mut seen: BTreeMap<u32, ()> = BTreeMap::new();
        for (slot, _v) in entries {
            if seen.insert(*slot, ()).is_some() {
                return Err(LayoutError::DuplicateLoadSlot {
                    block: *block,
                    slot: *slot,
                });
            }
            if *slot >= next_slot {
                return Err(LayoutError::SlotOutOfRange {
                    slot: *slot,
                    next_slot,
                });
            }
        }
    }

    // 4. Save↔load matching: union of all save slots vs union of all
    // load slots. Asymmetric differences indicate liveness bugs.
    let saved: BTreeMap<u32, ()> = layout
        .yield_saves
        .iter()
        .flat_map(|(_, entries)| entries.iter().map(|(s, _)| (*s, ())))
        .collect();
    let loaded: BTreeMap<u32, ()> = layout
        .resume_loads
        .iter()
        .flat_map(|(_, entries)| entries.iter().map(|(s, _)| (*s, ())))
        .collect();
    for slot in saved.keys() {
        if !loaded.contains_key(slot) {
            return Err(LayoutError::SaveWithoutLoad { slot: *slot });
        }
    }
    for slot in loaded.keys() {
        if !saved.contains_key(slot) {
            return Err(LayoutError::LoadWithoutSave { slot: *slot });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, StateMachineLayout};
    use alloc::vec;

    type Bb = u32;
    type V = u32;
    type Fid = u32;

    fn empty_layout() -> StateMachineLayout<Bb, V, Fid> {
        StateMachineLayout {
            resume_entries: vec![0],
            yield_blocks: vec![],
            yield_saves: vec![],
            resume_loads: vec![],
            block_kinds: vec![],
        }
    }

    #[test]
    fn empty_layout_is_valid() {
        let layout = empty_layout();
        validate_layout(&layout, 0).unwrap();
    }

    #[test]
    fn matched_save_load_is_valid() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0, 2];
        layout.yield_blocks = vec![(1, 1)];
        layout.yield_saves = vec![(1, vec![(0, 100), (1, 101)])];
        layout.resume_loads = vec![(2, vec![(0, 200), (1, 201)])];
        layout.block_kinds = vec![(1, BlockKind::DirectYield)];
        validate_layout(&layout, 2).unwrap();
    }

    #[test]
    fn save_without_load_detected() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0, 2];
        layout.yield_blocks = vec![(1, 1)];
        layout.yield_saves = vec![(1, vec![(0, 100), (1, 101)])];
        // Slot 1 saved but only slot 0 loaded.
        layout.resume_loads = vec![(2, vec![(0, 200)])];
        layout.block_kinds = vec![(1, BlockKind::DirectYield)];
        match validate_layout(&layout, 2) {
            Err(LayoutError::SaveWithoutLoad { slot }) => assert_eq!(slot, 1),
            other => panic!("expected SaveWithoutLoad, got {:?}", other),
        }
    }

    #[test]
    fn load_without_save_detected() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0, 2];
        layout.yield_blocks = vec![(1, 1)];
        layout.yield_saves = vec![(1, vec![(0, 100)])];
        layout.resume_loads = vec![(2, vec![(0, 200), (1, 201)])];
        layout.block_kinds = vec![(1, BlockKind::DirectYield)];
        match validate_layout(&layout, 2) {
            Err(LayoutError::LoadWithoutSave { slot }) => assert_eq!(slot, 1),
            other => panic!("expected LoadWithoutSave, got {:?}", other),
        }
    }

    #[test]
    fn duplicate_save_slot_detected() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0, 2];
        layout.yield_blocks = vec![(1, 1)];
        layout.yield_saves = vec![(1, vec![(0, 100), (0, 101)])];
        layout.resume_loads = vec![(2, vec![(0, 200)])];
        layout.block_kinds = vec![(1, BlockKind::DirectYield)];
        match validate_layout(&layout, 1) {
            Err(LayoutError::DuplicateSaveSlot { slot, .. }) => assert_eq!(slot, 0),
            other => panic!("expected DuplicateSaveSlot, got {:?}", other),
        }
    }

    #[test]
    fn entry_count_mismatch_detected() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0]; // 1 entry
        layout.yield_blocks = vec![(1, 1)]; // 1 yield → expect 2 entries
        match validate_layout(&layout, 0) {
            Err(LayoutError::EntryCountMismatch { .. }) => {}
            other => panic!("expected EntryCountMismatch, got {:?}", other),
        }
    }

    #[test]
    fn slot_out_of_range_detected() {
        let mut layout = empty_layout();
        layout.resume_entries = vec![0, 2];
        layout.yield_blocks = vec![(1, 1)];
        layout.yield_saves = vec![(1, vec![(5, 100)])];
        layout.resume_loads = vec![(2, vec![(5, 200)])];
        layout.block_kinds = vec![(1, BlockKind::DirectYield)];
        match validate_layout(&layout, 3) {
            Err(LayoutError::SlotOutOfRange { slot, next_slot }) => {
                assert_eq!(slot, 5);
                assert_eq!(next_slot, 3);
            }
            other => panic!("expected SlotOutOfRange, got {:?}", other),
        }
    }
}
