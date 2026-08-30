//! Quotes, as far as S1 needs them.
//!
//! A quote is the owner of one maker claim. Price, size, expiry, the `Active | Consumed |
//! Released` state and the per-leg intrusive chain are S2's; the back-pointer is what the
//! ledger needs, because invariant 3 is a statement about a *bidirectional* link.

use crate::reservation::ResIdx;
use crate::slab::Handle;

/// A generation-carrying handle to a quote.
pub type QuoteIdx = Handle<Quote>;

/// The owner side of a maker's claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quote {
    /// The maker's claim, or `None` if it has been released.
    ///
    /// A quote not backed by reserved capital is a promise, and promises are worthless
    /// here (§6) — so in S2 a live quote always holds one. `None` is reachable in S1
    /// because the ledger is exercised directly.
    claim: Option<ResIdx>,
}

impl Quote {
    /// A quote owning no claim yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { claim: None }
    }

    /// The maker's claim, if it still holds one.
    #[must_use]
    pub const fn claim(&self) -> Option<ResIdx> {
        self.claim
    }

    pub(crate) const fn set_claim(&mut self, claim: Option<ResIdx>) {
        self.claim = claim;
    }
}
