//! The two hand-offs out of the engine thread.
//!
//! The engine produces two streams that leave its thread: **events**, for whoever is
//! watching, and the **command log**, which `replay` reconstructs engine state from. Both
//! used to be the engine's problem — a mutex-guarded ring it shared with a spinning
//! publisher, and a `Vec` it grew itself. Both are now bounded channels to workers that own
//! the consequences.
//!
//! **Why this shape.** The engine's latency was exposed because its queue was shared with a
//! consumer whose speed it does not control — an `EventSink` doing arbitrary I/O. Here its
//! only counterparty is a thread that does nothing but receive. So a full channel no longer
//! means *a subscriber is slow*, which is unbounded; it means *the worker was descheduled*,
//! which is bounded by the scheduler. Everything genuinely hard — buffering, ordering, I/O,
//! reconnection, backpressure toward subscribers — moves to threads that are allowed to
//! block, because nothing waits on them.
//!
//! **What the engine pays.** `try_send` on a bounded `sync_channel` writes into a slot that
//! was allocated at construction. No lock the engine can see, no allocation, and no way to
//! block: the type has no such method on this path. That is three of CLAUDE 9's properties
//! obtained structurally rather than by discipline.
//!
//! **The two streams need different answers to a full queue, and that is the whole design
//! decision here.** Events are best-effort (SPEC §13): a refused event is a lost message and
//! the sequence gap exposes it. The command log is not: `replay` reproduces engine state from
//! it and SPEC §12 names it as the recovery path for a deep reorg, so a lost entry is a lost
//! guarantee rather than a lost message. Dropping there is not available and blocking is
//! forbidden, which leaves halting — durability over availability, stated rather than
//! discovered during an incident.
//!
//! **What was given up.** SPEC §11.2 said drop-*oldest*. A bounded channel refuses the
//! newest instead, because evicting the oldest would need the producer to advance the
//! consumer's cursor — the ownership violation that makes the textbook lock-free ring
//! inapplicable. The consequence is that a persistently slow consumer's staleness is
//! unbounded where the ring bounded it at capacity. That is a real cost and it is why the
//! worker exists: it drains unconditionally, so the boundary only refuses when the worker
//! itself has stopped, which is a fault rather than a workload.

/// A payload with the position it occupied in the total order.
///
/// Assigned by the engine thread before the hand-off, so a gap in the sequence is visible to
/// a consumer whatever was lost and wherever it was lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequenced<T> {
    /// Position in the total order of everything ever emitted.
    pub sequence: u64,
    /// What happened.
    pub event: T,
}

/// The engine's events, sequenced.
pub type SequencedEvent = Sequenced<rfq_core::event::Event>;

/// Why the engine stopped applying commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaltReason {
    /// Every command sender was dropped. The ordinary end of a round.
    ChannelClosed,
    /// The command log's queue is full, so the next entry could not be recorded.
    ///
    /// Continuing would apply commands that `replay` can never reproduce, which silently
    /// destroys the recovery guarantee of SPEC §12. Halting is the deliberate choice of
    /// durability over availability, and it is why this variant exists rather than a counter.
    LogUnrecordable,
}
