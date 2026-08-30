//! Quotes (SPEC §6).
//!
//! ```text
//!   SubmitQuote ──► Active ──accept, winner───► Consumed   (terminal)
//!                     │
//!                     ├──accept, outbid───────► Released   (terminal)
//!                     ├──replaced by a better─► Released
//!                     └──expired at accept────► Released
//! ```
//!
//! Liveness is a predicate, `Active && now < expires_at` (§4.2) — half-open, so a quote
//! expiring at exactly `now` is dead. Nothing sweeps: correctness never depends on a pass
//! having run.
//!
//! **Quotes are irrevocable until expiry.** The maker's exposure control is the expiry they
//! chose. `CancelQuote` exists as a *rejected transition*, not as an absent one, so enabling
//! it later is a policy change rather than a redesign (§14).
//!
//! **A quote not backed by reserved capital is a promise, and promises are worthless here.**
//! Reservation is against the leg's *fillable* size, not the quoted size: a quote offering
//! more than the leg needs is admissible, but only the leg's size can ever fill.

use crate::account::{AccountIdx, Link};
use crate::request::ReqIdx;
use crate::reservation::ResIdx;
use crate::slab::Handle;
use crate::types::{LegId, Price, Size, Ts};

/// A generation-carrying handle to a quote.
pub type QuoteIdx = Handle<Quote>;

/// Stored quote state (§6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteState {
    /// Standing. Live iff `now < expires_at` as well.
    Active,
    /// Won its leg at accept. Its claim is `committed` and the slot stays alive, because a
    /// committed claim names this quote as its owner and that owner must resolve (§15.3).
    Consumed,
    /// Lost, replaced, or dead at accept. Its claim is released.
    Released,
}

/// A maker's firm offer on one leg of one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    maker: AccountIdx,
    request: ReqIdx,
    leg: LegId,
    price: Price,
    size: Size,
    expires_at: Ts,
    /// Arrival order, assigned by the engine. The tiebreak at selection, and deterministic
    /// under replay because the counter is engine state, not a clock reading.
    arrival: u64,
    state: QuoteState,
    /// Next quote on the same `(request, leg)` chain (§3).
    pub(crate) next_on_leg: Link,
    /// The maker's claim. `None` once released.
    claim: Option<ResIdx>,
}

impl Quote {
    /// A standing quote.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // A quote is seven facts; bundling them into a
    // struct just to pass them would name the same seven fields one line earlier.
    pub const fn new(
        maker: AccountIdx,
        request: ReqIdx,
        leg: LegId,
        price: Price,
        size: Size,
        expires_at: Ts,
        arrival: u64,
    ) -> Self {
        Self {
            maker,
            request,
            leg,
            price,
            size,
            expires_at,
            arrival,
            state: QuoteState::Active,
            next_on_leg: Link::NIL,
            claim: None,
        }
    }

    /// Who wrote it.
    #[must_use]
    pub const fn maker(&self) -> AccountIdx {
        self.maker
    }

    /// Which request.
    #[must_use]
    pub const fn request(&self) -> ReqIdx {
        self.request
    }

    /// Which leg.
    #[must_use]
    pub const fn leg(&self) -> LegId {
        self.leg
    }

    /// The price of the side the requester is buying (§2.1), so lowest-is-best holds for
    /// `Yes` and `No` legs alike.
    #[must_use]
    pub const fn price(&self) -> Price {
        self.price
    }

    /// How much the maker is willing to fill. May exceed the leg's size; only the leg's size
    /// can ever fill.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// When this quote stops binding.
    #[must_use]
    pub const fn expires_at(&self) -> Ts {
        self.expires_at
    }

    /// Arrival order — the deterministic tiebreak, which denies a maker any gain from
    /// spamming identical quotes (§7.1).
    #[must_use]
    pub const fn arrival(&self) -> u64 {
        self.arrival
    }

    /// Stored state.
    #[must_use]
    pub const fn state(&self) -> QuoteState {
        self.state
    }

    /// Liveness (§4.2): `Active && now < expires_at`. Half-open, always.
    #[must_use]
    pub const fn is_live_at(&self, now: Ts) -> bool {
        matches!(self.state, QuoteState::Active) && now.0 < self.expires_at.0
    }

    /// The maker's claim, if it still holds one.
    #[must_use]
    pub const fn claim(&self) -> Option<ResIdx> {
        self.claim
    }

    pub(crate) const fn set_claim(&mut self, claim: Option<ResIdx>) {
        self.claim = claim;
    }

    pub(crate) const fn set_state(&mut self, state: QuoteState) {
        self.state = state;
    }
}
