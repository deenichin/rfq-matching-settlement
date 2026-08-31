//! The event queue: bounded, **drop-oldest**, sequence-numbered (SPEC §13).
//!
//! Backpressure is drop-oldest, never block. Blocking the sole writer on a full queue would
//! let one slow consumer stall the entire venue — a denial vector strictly worse than the
//! lost events it prevents. The engine must never be stallable by a consumer, so a full ring
//! overwrites its oldest entry and the writer returns immediately.
//!
//! Every event ever pushed gets the next sequence number, **including the ones that are
//! later evicted**. That is the whole mechanism: a consumer seeing 5, 6, then 12 knows
//! 7 through 11 existed and were dropped. Numbering only the events that survive would make
//! a lossy queue indistinguishable from a lossless one, which is precisely the information
//! a consumer needs in order to go and resynchronise.
//!
//! The ring is not engine state. It is the audit channel between the single writer and the
//! publisher, it holds no claims and no balances, and losing all of it costs the venue
//! nothing but visibility — which is why dropping from it is an acceptable answer at all.

use rfq_core::event::Event;

/// A payload with the position it occupied in the total order.
///
/// Generic over the payload, because none of the ring's properties — order, eviction,
/// sequence continuity — depend on what it carries. Keeping it generic is also what lets the
/// ring be tested without forging a slab handle: a handle is a claim that something exists,
/// and `Handle` deliberately has no `Default` for exactly that reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequenced<T> {
    /// Position in the total order of everything ever pushed. Gaps mean eviction, not
    /// reordering.
    pub sequence: u64,
    /// What happened.
    pub event: T,
}

/// The engine's events, sequenced.
pub type SequencedEvent = Sequenced<Event>;

/// A bounded ring that evicts its oldest entry rather than blocking its writer.
#[derive(Debug)]
pub struct EventRing<T = Event> {
    slots: Vec<Option<Sequenced<T>>>,
    /// Index of the oldest live entry.
    head: usize,
    /// Live entries.
    len: usize,
    /// Sequence to assign next.
    next_sequence: u64,
    /// How many entries have been evicted unread over this ring's lifetime.
    dropped: u64,
}

impl<T: Clone> EventRing<T> {
    /// A ring holding `capacity` events. Preallocated; it never grows.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero. A zero-capacity ring drops everything, which is a silently
    /// broken audit trail rather than a small one, and it is a startup misconfiguration
    /// rather than a runtime condition.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "an event ring of zero capacity publishes nothing");
        Self {
            slots: vec![None; capacity],
            head: 0,
            len: 0,
            next_sequence: 0,
            dropped: 0,
        }
    }

    /// How many events the ring can hold. Fixed at construction.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// How many events are waiting to be published.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many events have been evicted unread.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Push an event, evicting the oldest if the ring is full. Returns the sequence number
    /// assigned. **Never blocks and never fails**, which is the point.
    pub fn push(&mut self, event: T) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);

        let capacity = self.slots.len();
        let tail = wrap(self.head.checked_add(self.len).unwrap_or(0), capacity);
        if let Some(slot) = self.slots.get_mut(tail) {
            *slot = Some(Sequenced { sequence, event });
        }

        if self.len == capacity {
            // Full: the write above landed on the oldest entry, so the head moves on and the
            // evicted event is counted. The consumer will see the gap in the sequence.
            self.head = wrap(self.head.checked_add(1).unwrap_or(0), capacity);
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.len = self.len.saturating_add(1);
        }

        sequence
    }

    /// Take the oldest waiting event.
    pub fn pop(&mut self) -> Option<Sequenced<T>> {
        if self.len == 0 {
            return None;
        }
        let capacity = self.slots.len();
        let taken = self.slots.get_mut(self.head).and_then(Option::take);
        self.head = wrap(self.head.checked_add(1).unwrap_or(0), capacity);
        self.len = self.len.saturating_sub(1);
        taken
    }
}

/// Index into a ring of `capacity` slots. Checked division: a zero-capacity ring is refused
/// at construction, so the fallback is unreachable rather than a silent policy.
fn wrap(index: usize, capacity: usize) -> usize {
    index.checked_rem(capacity).unwrap_or(0)
}
