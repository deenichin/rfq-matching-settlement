//! The event set (skeleton).
//!
//! Events are the engine's only output. `apply` performs no I/O — not logging, not
//! metrics, not `println!` (CLAUDE 8) — and writes into a caller-provided buffer whose
//! headroom is checked in the CHECK phase (CLAUDE 9). The publisher does all I/O, and if
//! it dies the engine keeps applying: the audit trail is best-effort, the state machine is
//! authoritative (SPEC §13).
//!
//! [`Event::SubmitIntent`] is the **only** path from the engine to custody (SPEC §13.1).
//! It is an event rather than a call because settlement is never invoked inside the commit
//! phase: a fallible custody call there would violate CLAUDE 18 in v1 and be impossible in
//! v2, where settlement is a network round trip.

/// Something the engine did, for whoever is listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A request is open: contract description, side, size and deadline, fanned out to
    /// makers. **Never the limit price** (SPEC §5.2) — a revealed reserve shades quotes
    /// toward the limit rather than toward the maker's best price. Without this event no
    /// maker learns a request exists and the venue is not an RFQ. Payload: S2.
    RequestOpened,
    /// The best eligible selection changed, published to the requester on quote arrival —
    /// the only point at which the engine emits it. The feed is eventually consistent by
    /// construction: normalisation emits nothing, so an expiry surfaces on the next
    /// command. Safe because accept binds at-or-better (SPEC §7.1.1). Payload: S2.
    BestSelectionChanged,
    /// A quote lost at selection and its reservation was released. Makers are never left
    /// inferring the fate of their capital from silence (SPEC §7.2). Payload: S2.
    QuoteRejected,
    /// A quote died of expiry. Emitted **only** from the accept commit phase, where the
    /// count is bounded by `MAX_LEGS × MAX_QUOTES_PER_LEG`; expiry outside that path is
    /// silent, because normalisation's count is unbounded (SPEC §4.3). Payload: S2.
    QuoteExpired,
    /// The one engine-to-custody path: a bundle and its nonce, picked up by the settlement
    /// adapter (SPEC §7.2, §13.1). Payload: S2/S3.
    SubmitIntent,
}

/// The caller-provided event buffer (CLAUDE 9, SPEC §13).
///
/// `apply` allocates nothing on any path, so events are written into a buffer the caller
/// preallocated and drains. The buffer is a type rather than a bare `Vec` so that the two
/// halves of the discipline are enforced separately and in the right phases:
///
/// - [`EventBuffer::headroom`] is fallible and belongs in the **CHECK** phase. It is where
///   `EventBufferFull` comes from, and it is answerable there because the accept commit
///   phase's event count is statically bounded by `MAX_LEGS × MAX_QUOTES_PER_LEG` (§4.3).
/// - [`EventBuffer::push`] is **infallible** and belongs in the commit phase, where no
///   fallible call may appear (CLAUDE 18).
///
/// A bare `&mut Vec` would let the commit phase discover it is out of room, which is the
/// one place that cannot be true.
#[derive(Debug, Default)]
pub struct EventBuffer {
    events: Vec<Event>,
}

/// The buffer cannot hold what the command would emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventBufferFull;

impl EventBuffer {
    /// Preallocate room for `capacity` events. The only allocation the buffer performs.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { events: Vec::with_capacity(capacity) }
    }

    /// Room, fixed at construction. Never changes — this is the allocation proxy of
    /// CLAUDE 25 for the event path.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.events.capacity()
    }

    /// Events written and not yet drained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// CHECK phase: assert room for `needed` more events before committing to emit them.
    ///
    /// # Errors
    ///
    /// [`EventBufferFull`].
    pub fn headroom(&self, needed: usize) -> Result<(), EventBufferFull> {
        match self.events.len().checked_add(needed) {
            Some(total) if total <= self.events.capacity() => Ok(()),
            _ => Err(EventBufferFull),
        }
    }

    /// Commit phase: write an event. Infallible, and never grows the buffer.
    ///
    /// # Panics
    ///
    /// In debug builds, if the CHECK phase did not verify headroom first. In release the
    /// event is dropped rather than the buffer reallocated: `apply` allocates nothing on any
    /// path (CLAUDE 9), and a lost audit entry is a smaller failure than a heap allocation
    /// inside the single writer.
    pub fn push(&mut self, event: Event) {
        debug_assert!(
            self.events.len() < self.events.capacity(),
            "the CHECK phase must verify headroom before the commit phase emits (CLAUDE 9)"
        );
        if self.events.len() < self.events.capacity() {
            self.events.push(event);
        }
    }

    /// Take everything written, leaving the buffer empty and its capacity untouched.
    pub fn drain(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.events.drain(..)
    }

    /// Discard everything written.
    pub fn clear(&mut self) {
        self.events.clear();
    }
}
