//! S0 gate: the slab's two load-bearing properties.
//!
//! 1. **Capacity is never exceeded.** Slabs are preallocated and never grow (CLAUDE 9, 10);
//!    exhaustion is a rejection. `capacity()` unchanged after N randomised operations is
//!    the observable allocation proxy CLAUDE 25 asks for in place of an allocator hook.
//! 2. **A handle from a freed slot is rejected by generation mismatch.** Index reuse alone
//!    proves nothing — the whole question is whether a *stale* handle is refused rather
//!    than silently resolved to the slot's new occupant. The accept path and the
//!    settlement nonce (SPEC §8.1) both depend on this.

// Tests may unwrap, and their bookkeeping arithmetic is not the money path.
#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use proptest::prelude::*;
use rfq_core::slab::{Handle, Slab, SlabFull};

#[test]
fn stale_handle_is_rejected_after_the_slot_is_reused() {
    let mut slab: Slab<u32> = Slab::with_capacity(1);

    let first = slab.insert(7).unwrap();
    assert_eq!(slab.remove(first), Some(7));
    let second = slab.insert(9).unwrap();

    // Precondition (CLAUDE 39): the slot really was reused. Without this the test could
    // pass on a slab that simply handed out a fresh index, proving nothing about staleness.
    assert_eq!(first.index(), second.index(), "the test needs the slot to be reused");
    assert_ne!(first.generation(), second.generation(), "reuse must bump the generation");

    // The property: the stale handle resolves to nothing, not to the new occupant.
    assert_eq!(slab.get(first), None);
    assert_eq!(slab.get_mut(first), None);
    assert!(!slab.contains(first));
    assert_eq!(slab.get(second), Some(&9));

    // And a stale free does not evict the live entry — the same aliasing bug, silent.
    assert_eq!(slab.remove(first), None);
    assert_eq!(slab.get(second), Some(&9));
    assert_eq!(slab.len(), 1);
}

#[test]
fn exhaustion_is_a_rejection_not_a_reallocation() {
    let mut slab: Slab<u32> = Slab::with_capacity(2);
    let a = slab.insert(1).unwrap();
    let _b = slab.insert(2).unwrap();

    assert_eq!(slab.insert(3), Err(SlabFull));
    assert_eq!(slab.capacity(), 2, "a rejected insert must not grow the slab");
    assert_eq!(slab.len(), 2);

    // Freeing one makes room for exactly one.
    assert_eq!(slab.remove(a), Some(1));
    assert!(slab.insert(3).is_ok());
    assert_eq!(slab.insert(4), Err(SlabFull));
    assert_eq!(slab.capacity(), 2);
}

#[test]
fn a_zero_capacity_slab_admits_nothing() {
    let mut slab: Slab<u32> = Slab::with_capacity(0);
    assert!(slab.is_empty());
    assert_eq!(slab.insert(1), Err(SlabFull));
    assert_eq!(slab.capacity(), 0);
}

/// One randomised operation.
#[derive(Clone, Copy, Debug)]
enum Op {
    Insert(u32),
    /// Free the live handle at this position in the live set.
    Remove(usize),
    /// Try to free a handle that is already stale.
    RemoveStale(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => any::<u32>().prop_map(Op::Insert),
        2 => any::<usize>().prop_map(Op::Remove),
        1 => any::<usize>().prop_map(Op::RemoveStale),
    ]
}

proptest! {
    /// proptest seeds itself deterministically and prints the failing case, persisting it
    /// to `proptest-regressions/` so a failure replays exactly (CLAUDE 4).
    #[test]
    fn capacity_is_never_exceeded_and_stale_handles_never_resolve(
        capacity in 1u32..12,
        ops in proptest::collection::vec(op(), 0..300),
    ) {
        let mut slab: Slab<u32> = Slab::with_capacity(capacity);
        let mut live: Vec<(Handle<u32>, u32)> = Vec::new();
        let mut stale: Vec<Handle<u32>> = Vec::new();

        for op in ops {
            match op {
                Op::Insert(value) => {
                    if let Ok(handle) = slab.insert(value) {
                        prop_assert!(live.len() < capacity as usize);
                        live.push((handle, value));
                    } else {
                        // The only admissible reason to refuse is being full.
                        prop_assert_eq!(slab.len(), capacity);
                        prop_assert_eq!(live.len(), capacity as usize);
                    }
                }
                Op::Remove(pick) => {
                    if !live.is_empty() {
                        let (handle, value) = live.remove(pick % live.len());
                        prop_assert_eq!(slab.remove(handle), Some(value));
                        stale.push(handle);
                    }
                }
                Op::RemoveStale(pick) => {
                    if !stale.is_empty() {
                        let handle = stale[pick % stale.len()];
                        // A stale free must be a no-op, not an eviction.
                        prop_assert_eq!(slab.remove(handle), None);
                    }
                }
            }

            // Capacity is fixed at construction and the slab never grows (CLAUDE 25).
            prop_assert_eq!(slab.capacity(), capacity);
            prop_assert!(slab.len() <= capacity);
            prop_assert_eq!(slab.len() as usize, live.len());

            // Every live handle resolves to its own value...
            for (handle, value) in &live {
                prop_assert_eq!(slab.get(*handle), Some(value));
            }
            // ...and no freed handle resolves at all, whatever now occupies its index.
            for handle in &stale {
                prop_assert_eq!(slab.get(*handle), None);
            }
        }
    }
}
