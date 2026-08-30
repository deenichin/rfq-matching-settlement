//! Contracts, as the core sees them (SPEC §5.3).
//!
//! **The core never sees a description.** Identity is byte equality over the full
//! description plus event date plus resolution source, and that comparison happens at the
//! gateway, which owns `description_bytes → ContractIdx` and hands the core an index. No
//! hashing, no string comparison, nothing reachable from `apply` that could allocate
//! (SPEC §3, CLAUDE 11).
//!
//! Why byte equality and not a hash: a non-cryptographic hash would be strictly worse here,
//! because a collision would let a trade formed on one contract resolve under another's
//! outcome, and contract identity is an adversarial surface (§11). A cryptographic hash
//! would buy nothing over byte equality while adding a dependency. Nothing in v1 puts a
//! contract id on a wire or a chain, so there is nothing to compress.

use crate::types::Ts;

/// A dense contract index, assigned by the gateway on first sight.
///
/// **No generation, and deliberately.** SPEC §3's table calls this a slab, but nothing ever
/// frees a contract — escrows resolve against it for months after the trade — so an index
/// is never reused and a stale one cannot exist. That is the same argument that makes
/// [`AccountIdx`](crate::account::AccountIdx) generation-free, and a generation here would
/// be ceremony that proves nothing. Flagged rather than assumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractIdx(pub u32);

/// What the core knows about a contract.
///
/// Two fields' worth in S2: the index it is addressed by, and the date the event settles.
/// Resolution state — `Unresolved | Resolved(Outcome)` and the oracle status — arrives in
/// S5 (§10.1). The description, the resolution source and every byte of wording stay at the
/// gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contract {
    event_date: Ts,
}

impl Contract {
    /// A contract settling at `event_date`.
    #[must_use]
    pub const fn new(event_date: Ts) -> Self {
        Self { event_date }
    }

    /// When the event this contract is about occurs.
    ///
    /// Gates admission: `SubmitRequest` is refused if any leg's contract has
    /// `now >= event_date − MIN_HORIZON` (§5.2), so escrow can never form on a contract
    /// whose stall grace has already elapsed.
    #[must_use]
    pub const fn event_date(&self) -> Ts {
        self.event_date
    }
}
