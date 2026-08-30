//! S1.5 gate: the event queue's backpressure policy (SPEC §13).
//!
//! Drop-oldest, never block, with a sequence number so consumers detect gaps. This is
//! tested on the ring directly — the same call the accept commit phase makes in S2 — because
//! this stage's command set emits no events: nothing in it owes a participant a
//! notification, and inventing one to give the publisher something to carry would be a
//! design decision made for a test's convenience.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_runtime::event_ring::EventRing;

/// The ring is indifferent to what it carries — its whole job is order, eviction and
/// sequence continuity — so these tests carry integers. That is not a shortcut: it is the
/// reason the ring is generic. Forging an `Event` here would mean forging a slab handle,
/// and `Handle` has no `Default` precisely because a handle is a claim that something
/// exists.

#[test]
fn a_full_ring_evicts_its_oldest_entry_rather_than_refusing_the_writer() {
    // Blocking the sole writer on a full queue would let one slow consumer stall the entire
    // venue — a denial vector strictly worse than the lost events it prevents. So `push`
    // has no failure mode at all: its return type is a sequence number, not a Result.
    let mut ring: EventRing<u32> = EventRing::with_capacity(3);
    for _ in 0..3 {
        ring.push(0);
    }
    assert_eq!(ring.len(), 3);
    assert_eq!(ring.dropped(), 0);

    let sequence = ring.push(1);
    assert_eq!(sequence, 3, "the writer was served");
    assert_eq!(ring.len(), 3, "the ring did not grow");
    assert_eq!(ring.capacity(), 3);
    assert_eq!(ring.dropped(), 1, "the oldest was evicted, not the newest refused");

    // The oldest survivor is sequence 1: sequence 0 is the one that went.
    assert_eq!(ring.pop().unwrap().sequence, 1);
    assert_eq!(ring.pop().unwrap().sequence, 2);
    assert_eq!(ring.pop().unwrap().sequence, 3);
    assert_eq!(ring.pop(), None);
}

#[test]
fn sequence_numbers_are_assigned_to_dropped_events_too_so_gaps_are_visible() {
    // The whole mechanism. Numbering only the survivors would make a lossy queue
    // indistinguishable from a lossless one, and a consumer would have no way to know it
    // needs to resynchronise.
    let mut ring: EventRing<u32> = EventRing::with_capacity(2);
    for _ in 0..10 {
        ring.push(0);
    }

    let first = ring.pop().unwrap();
    let second = ring.pop().unwrap();
    assert_eq!(first.sequence, 8, "eight events were pushed past a two-slot ring");
    assert_eq!(second.sequence, 9);
    assert_eq!(ring.dropped(), 8);

    // A consumer that saw sequence 0 and then sequence 8 can compute the gap exactly.
    let previously_seen = 0_u64;
    assert_eq!(first.sequence - previously_seen, 8, "eight events are missing, and knowably so");
    assert!(ring.is_empty());
}

#[test]
fn a_ring_that_is_kept_drained_never_drops() {
    // The precondition the drop path is measured against (CLAUDE 39): if this also dropped,
    // the test above would be showing eviction under conditions where any queue evicts.
    let mut ring: EventRing<u32> = EventRing::with_capacity(2);
    for expected in 0..20 {
        let sequence = ring.push(2);
        assert_eq!(sequence, expected);
        assert_eq!(ring.pop().unwrap().sequence, expected);
    }
    assert_eq!(ring.dropped(), 0);
    assert!(ring.is_empty());
}

#[test]
fn the_ring_wraps_without_losing_order() {
    let mut ring: EventRing<u32> = EventRing::with_capacity(4);
    for _ in 0..3 {
        ring.push(0);
    }
    assert_eq!(ring.pop().unwrap().sequence, 0);
    assert_eq!(ring.pop().unwrap().sequence, 1);
    // Head is now at index 2; pushing four more wraps the tail past it.
    for _ in 0..4 {
        ring.push(1);
    }
    assert_eq!(ring.len(), 4);
    assert_eq!(ring.dropped(), 1, "one of the five live entries had to go");
    let drained: Vec<u64> = std::iter::from_fn(|| ring.pop()).map(|event| event.sequence).collect();
    assert_eq!(drained, vec![3, 4, 5, 6], "still oldest-first, and sequence 2 was the eviction");
}
