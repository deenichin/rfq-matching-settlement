//! The one thing the core knows about an escrow: its id (SPEC §13.1).
//!
//! Escrows are owned by custody, not by the core. The engine holds an `EscrowId` and cannot
//! reach into balances, contributions or state — everything an escrow *contains* lives on
//! the other side of the seam and arrives, if at all, as a chain event through the indexer.
//!
//! The type lives here rather than in `chain` for the same reason `Amount` does: the core
//! must be able to hold one, and `core` never depends on `chain`.

/// A dense escrow index, assigned by custody.
///
/// No generation: an escrow is never freed. `Locked → Settled` is its whole lifecycle
/// (§9.2), and a settled escrow stays in the record because re-settlement must be a no-op
/// rather than a lookup failure. An index is therefore never reused and a stale one cannot
/// exist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct EscrowId(pub u32);
