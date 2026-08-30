//! Accounts: the balance mirror and the two claim totals (SPEC §2.2, §2.3).
//!
//! The core holds a **read-only projection** of custody's balances, used for admission
//! only. It is never authoritative: admission uses it, settlement revalidates against
//! custody and reverts wholesale on disagreement (§9.1). A stale mirror can therefore cause
//! a failed settlement — a liveness cost — but never a money-state error.
//!
//! `reserved` and `committed` sit beside the mirrored balance because they are **claims
//! against it, not partitions of it** (§2.2). Reserving moves no money; it records that the
//! core has promised a unit will be available. So `free` is not decremented by a
//! reservation, and the quantity admission actually asks about is
//! `free − reserved − committed`.

use crate::types::Amount;

/// A dense account index, assigned by the gateway on first sight (SPEC §3).
///
/// Unlike a slab handle this carries **no generation**, and deliberately: accounts are
/// never freed, so an index is never reused and a stale one cannot exist. A generation
/// here would be ceremony that proves nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountIdx(pub u32);

/// The nil link. `u32::MAX` is reserved, so a chain can hold `u32::MAX - 1` entries.
pub(crate) const NIL: u32 = u32::MAX;

/// An intrusive chain link: a bare index into the reservation slab, or nil.
///
/// Bare rather than a generation-carrying handle because this is a link the ledger
/// maintains itself and never hands out. A generation protects against a *stale* reference
/// held by someone else; a link the structure owns cannot be stale, and if it were, the
/// structure is already corrupt. Cross-structure references — a claim's owner, an owner's
/// back-pointer — do carry generations, because those genuinely can go stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Link(u32);

impl Default for Link {
    /// Nil, not index zero. A derived `Default` would make every fresh account's chain
    /// endpoints point at slot 0 — a chain that looks populated before anything is on it.
    fn default() -> Self {
        Self::NIL
    }
}

impl Link {
    /// The nil link.
    pub(crate) const NIL: Self = Self(NIL);

    /// A link to `index`.
    pub(crate) const fn to(index: u32) -> Self {
        debug_assert!(index != NIL, "u32::MAX is reserved as the nil link");
        Self(index)
    }

    /// The index this link names, or `None` if it is nil.
    pub(crate) const fn index(self) -> Option<u32> {
        if self.0 == NIL { None } else { Some(self.0) }
    }
}

/// One account's core-side state: the mirrored balance, the two stored claim totals, and
/// the endpoints of its expiry-ordered reservation chain (§4.3).
///
/// The chain endpoints live here rather than in a parallel array because §15.1's assertion
/// is per-account — `account.reserved == Σ` amounts on that account's chain — and a total
/// stored apart from the chain it summarises is a desynchronisation waiting to happen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MirroredBalance {
    /// Projection of `custody.free(a)`. Exact in v1; lagged by confirmation depth plus
    /// indexer lag in v2 (§2.3). Never written by engine logic — only by a chain event.
    free: Amount,
    /// Stored total. Equal to the sum of amounts on this account's expiry chain (§15.1),
    /// asserted in the chain-sum form and never in the expiry-predicate form: the latter
    /// shrinks with the passage of time alone and is unsatisfiable against a stored value
    /// for any un-normalised account (CLAUDE 16).
    reserved: Amount,
    /// Stored total. Equal to the sum of this account's amounts across the committed lists
    /// of every request (§2.4, §15.1). Committed capital has no expiry and may not be
    /// released on a guess (§8.3).
    committed: Amount,
    /// Head of the expiry-ordered chain — the soonest expiry, and where `release_expired`
    /// starts.
    pub(crate) expiry_head: Link,
    /// Tail — the latest expiry, and where insertion walks back from (§4.3).
    pub(crate) expiry_tail: Link,
}

impl MirroredBalance {
    /// The mirrored free balance (§2.3).
    #[must_use]
    pub const fn free(&self) -> Amount {
        self.free
    }

    /// The stored `reserved` total.
    #[must_use]
    pub const fn reserved(&self) -> Amount {
        self.reserved
    }

    /// The stored `committed` total.
    #[must_use]
    pub const fn committed(&self) -> Amount {
        self.committed
    }

    /// `free − reserved − committed`: what a new claim may draw on.
    ///
    /// Saturating at zero rather than checked: the subtraction cannot go negative on a
    /// ledger satisfying claim coverage (§15.6), and a caller asking "how much is spare"
    /// has no use for an error. A ledger that *has* broken coverage is caught by the
    /// invariant assertions, not by this returning a surprising number.
    #[must_use]
    pub const fn available(&self) -> Amount {
        Amount(self.free.0.saturating_sub(self.reserved.0).saturating_sub(self.committed.0))
    }

    pub(crate) const fn set_free(&mut self, free: Amount) {
        self.free = free;
    }

    pub(crate) const fn set_reserved(&mut self, reserved: Amount) {
        self.reserved = reserved;
    }

    pub(crate) const fn set_committed(&mut self, committed: Amount) {
        self.committed = committed;
    }
}
