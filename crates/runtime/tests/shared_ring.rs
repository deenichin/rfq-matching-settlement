//! The handshake between the engine and the publisher.
//!
//! Like `event_ring.rs`, these carry integers: the ring is indifferent to what it holds, and
//! forging an `Event` would mean forging a slab handle.
//!
//! **What is not asserted here.** The property the change exists for — that an idle
//! publisher no longer contends with the engine for the ring's lock — is a statement about
//! cache-coherence traffic, and nothing in this repository can measure it. Proving it needs
//! a benchmark harness, which is out of scope (CLAUDE 36, rule 0). Asserted below instead is
//! everything the change could have *broken*: the probe agreeing with the locked state, and
//! the shutdown path losing nothing.
//!
//! **One gap, named rather than papered over (CLAUDE 29).** `run_publisher` drains under the
//! lock before retiring, because `is_idle` is a relaxed read that may lag a final append.
//! Deleting that drain leaves the whole suite green: by the time `join` calls `finish` the
//! publisher has almost always drained already, so the race resolves in the drain's favour
//! on its own. Forcing events to be pending at shutdown needs a sink that blocks until the
//! test releases it, and releasing it requires either a second thread (CLAUDE 28) or a wait
//! (CLAUDE 43). So the drain is correct by construction and unasserted, and the tests below
//! cover the half of it that is reachable: that `finish` does not discard what is waiting.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_runtime::SharedRing;

#[test]
fn the_idle_probe_agrees_with_the_locked_length() {
    let ring: SharedRing<u32> = SharedRing::with_capacity(4);
    assert!(ring.is_idle(), "a fresh ring is idle");
    assert!(ring.is_empty());

    assert_eq!(ring.append([1, 2, 3]), 3);
    assert!(!ring.is_idle(), "the probe must see what the lock sees");
    assert_eq!(ring.len(), 3);

    for expected in 1..=3 {
        assert_eq!(ring.pop().map(|entry| entry.event), Some(expected));
    }
    assert!(ring.is_idle(), "drained, so idle again");
    assert!(ring.pop().is_none());
}

#[test]
fn the_probe_tracks_eviction_rather_than_the_number_pushed() {
    // The length mirrors live entries, not lifetime pushes: a full ring that evicts stays at
    // capacity. If the mirror counted pushes, the publisher would spin on a ring it thought
    // was growing forever.
    let ring: SharedRing<u32> = SharedRing::with_capacity(2);
    assert_eq!(ring.append([1, 2, 3, 4]), 4, "every push was accepted");
    assert_eq!(ring.len(), 2, "but only capacity survives");
    assert!(!ring.is_idle());
    assert_eq!(ring.dropped(), 2);

    // And the survivors are the newest, which is the drop-oldest policy of SPEC §11.2.
    assert_eq!(ring.pop().map(|entry| entry.event), Some(3));
    assert_eq!(ring.pop().map(|entry| entry.event), Some(4));
    assert!(ring.is_idle());
}

#[test]
fn finishing_does_not_discard_events_that_are_still_waiting() {
    // The trap in moving the shutdown flag out of the mutex. A publisher that checked
    // `is_finished` before draining — or that trusted a relaxed `is_idle` on the way out —
    // would retire holding undelivered events. `finish` is a statement that no *more* will
    // arrive, never that the ring is empty.
    let ring: SharedRing<u32> = SharedRing::with_capacity(8);
    ring.append([10, 20, 30]);
    ring.finish();

    assert!(ring.is_finished());
    assert!(!ring.is_idle(), "finished is not the same fact as drained");

    let mut drained = Vec::new();
    while let Some(entry) = ring.pop() {
        drained.push(entry.event);
    }
    assert_eq!(drained, vec![10, 20, 30], "every event survived the shutdown");
    assert!(ring.is_idle());
}

#[test]
fn sequence_numbers_survive_the_wrapper() {
    // The wrapper must not disturb the numbering the whole drop-detection story rests on.
    let ring: SharedRing<u32> = SharedRing::with_capacity(2);
    ring.append([1, 2, 3, 4]);
    let first = ring.pop().unwrap();
    let second = ring.pop().unwrap();
    assert_eq!((first.sequence, first.event), (2, 3));
    assert_eq!((second.sequence, second.event), (3, 4));
    assert_eq!(
        second.sequence - first.sequence,
        1,
        "contiguous here; the gap from 0 is what tells a consumer two were lost"
    );
}
