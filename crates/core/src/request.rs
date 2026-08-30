//! Requests and their legs (SPEC §5).
//!
//! ```text
//!                  SubmitRequest
//!                       │
//!                       ▼
//!                    ┌──────┐  RejectRequest (requester)   ┌──────────┐
//!                    │ Open │ ───────────────────────────► │ Rejected │
//!                    └──────┘                              └──────────┘
//!                       │  now >= deadline (derived)        ┌─────────┐
//!                       ├─────────────────────────────────► │ Expired │
//!                       │  AcceptRequest                    └─────────┘
//!                       ▼
//!               ┌────────────────┐   PollSettlement (S4)   ┌──────────┐
//!               │ Settling{nonce}│ ──────────────────────► │ Escrowed │
//!               └────────────────┘ ──────────────────────► │ Settlement
//!                                                            Failed   │
//! ```
//!
//! `Expired` is **derived, not stored**: the request is expired iff `now >= deadline` and no
//! accept has been made. Storing it would require a sweep, and then whether an accept
//! succeeded would depend on scheduler timing (§4.2).

use crate::account::{AccountIdx, Link};
use crate::config::MAX_LEGS;
use crate::contract::ContractIdx;
use crate::quote::QuoteIdx;
use crate::reservation::ResIdx;
use crate::slab::Handle;
use crate::types::{Price, Side, Size, Ts};

/// A generation-carrying handle to a request.
///
/// The generation is not bookkeeping: `(ReqIdx, generation)` **is** the settlement nonce
/// (§8.1). A request slot reused after its predecessor was freed yields a different
/// generation, so nonces are never reused even though indices are — which is what lets a
/// resubmission bounce off its own nonce instead of forming a second escrow.
pub type ReqIdx = Handle<Request>;

/// The settlement nonce: unique and deterministic without hashing (SPEC §8.1).
///
/// Content-derivation is deliberately avoided — it would require hashing inside `apply`,
/// which §3 forbids, and it serves a signature-binding purpose that does not exist in v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nonce {
    /// The request slot.
    pub request: u32,
    /// That slot's generation at the moment the bundle was formed.
    pub generation: u32,
}

impl Nonce {
    /// The nonce for a request handle.
    #[must_use]
    pub const fn of(request: ReqIdx) -> Self {
        Self { request: request.index(), generation: request.generation() }
    }
}

/// Stored request state. `Expired` is absent because it is derived (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestState {
    /// Accepting quotes until the deadline.
    Open,
    /// Accepted; a bundle carrying this nonce is in flight. Both sides' capital is
    /// `committed` and may not be released on a guess (§2.4, §8.3).
    Settling(Nonce),
    /// Settlement confirmed; custody holds the escrows. Terminal. Reached in S4.
    Escrowed,
    /// The requester withdrew the request. Terminal.
    Rejected,
    /// Settlement failed definitively. Terminal. Reached in S4.
    SettlementFailed,
}

impl RequestState {
    /// Whether no transition leaves this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Escrowed | Self::Rejected | Self::SettlementFailed)
    }
}

/// One leg of a request: what to trade, which side of it, how much, and the most the
/// requester will pay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leg {
    contract: ContractIdx,
    side: Side,
    size: Size,
    limit: Price,
    /// Head of this leg's intrusive quote chain (SPEC §3). Singly linked: the chain is
    /// walked whole at selection and at commit, and removal from the middle happens at most
    /// `MAX_QUOTES_PER_LEG` steps in.
    pub(crate) quotes_head: Link,
    /// Live quotes on this leg, bounded by `MAX_QUOTES_PER_LEG` so the commit phase's event
    /// count is statically bounded (§4.3, §6).
    pub(crate) quote_count: u8,
    /// The last selection *published* for this leg (§7.1.1). A publication cache, never an
    /// input to a fill: selection is a pure function re-run at accept time, so it cannot go
    /// stale.
    pub(crate) published: Option<Price>,
}

impl Leg {
    /// A leg with no quotes yet.
    #[must_use]
    pub const fn new(contract: ContractIdx, side: Side, quantity: Size, limit: Price) -> Self {
        Self {
            contract,
            side,
            size: quantity,
            limit,
            quotes_head: Link::NIL,
            quote_count: 0,
            published: None,
        }
    }

    /// The contract this leg trades.
    #[must_use]
    pub const fn contract(&self) -> ContractIdx {
        self.contract
    }

    /// The side the **requester** is buying (§2.1). The maker takes the opposite.
    #[must_use]
    pub const fn side(&self) -> Side {
        self.side
    }

    /// How many contracts. A quote must cover this in full — no aggregation across makers
    /// on one leg in v1 (§7.1).
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// The most the requester will pay, in price terms.
    ///
    /// Two jobs: it bounds the requester's reservation, and it protects them from a fill at
    /// a price they never agreed to. **Never broadcast** (§5.2) and **never checked at
    /// admission** (§6) — a rejection at admission would be a free oracle a maker could
    /// bisect against at no cost. It is enforced at selection and nowhere else.
    #[must_use]
    pub const fn limit(&self) -> Price {
        self.limit
    }

    /// The last price published for this leg, if any.
    #[must_use]
    pub const fn published(&self) -> Option<Price> {
        self.published
    }

    /// Live quotes on this leg.
    #[must_use]
    pub const fn quote_count(&self) -> u8 {
        self.quote_count
    }
}

impl Default for Leg {
    fn default() -> Self {
        Self::new(ContractIdx(0), Side::Yes, Size(0), Price(0))
    }
}

/// A request for quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    requester: AccountIdx,
    deadline: Ts,
    state: RequestState,
    n_legs: u8,
    /// Fixed-size, because `MAX_LEGS` is a compile-time storage bound and leg arrays cannot
    /// take a runtime value (§3). The configured `max_legs` is checked against it at startup.
    legs: [Leg; MAX_LEGS],
    /// The requester's own claim (§2.4). `None` once released.
    claim: Option<ResIdx>,
    /// Head of this request's committed list — both sides of the trade, once accepted.
    pub(crate) committed_head: Link,
}

impl Request {
    /// A request open until `deadline`.
    #[must_use]
    pub const fn new(
        requester: AccountIdx,
        deadline: Ts,
        legs: [Leg; MAX_LEGS],
        n_legs: u8,
    ) -> Self {
        Self {
            requester,
            deadline,
            state: RequestState::Open,
            n_legs,
            legs,
            claim: None,
            committed_head: Link::NIL,
        }
    }

    /// Who opened it. The only account authorised to reject or accept it (§5) — designed,
    /// and enforced at the gateway rather than here, since §16 excludes signatures.
    #[must_use]
    pub const fn requester(&self) -> AccountIdx {
        self.requester
    }

    /// When quoting closes. There is no post-deadline acceptance window (§5.1): a firm quote
    /// is an option the maker wrote and gave away for free, and any window past the deadline
    /// is additional free optionality at maker expense.
    #[must_use]
    pub const fn deadline(&self) -> Ts {
        self.deadline
    }

    /// Stored state.
    #[must_use]
    pub const fn state(&self) -> RequestState {
        self.state
    }

    /// Whether the request is expired at `now`. **Derived, never stored** (§5).
    #[must_use]
    pub const fn is_expired_at(&self, now: Ts) -> bool {
        matches!(self.state, RequestState::Open) && now.0 >= self.deadline.0
    }

    /// Whether a quote or an accept may still be admitted at `now`.
    #[must_use]
    pub const fn is_live_at(&self, now: Ts) -> bool {
        matches!(self.state, RequestState::Open) && now.0 < self.deadline.0
    }

    /// How many legs.
    #[must_use]
    pub const fn n_legs(&self) -> u8 {
        self.n_legs
    }

    /// The legs, in order.
    #[must_use]
    pub fn legs(&self) -> &[Leg] {
        self.legs.get(..usize::from(self.n_legs)).unwrap_or(&[])
    }

    /// One leg, if it exists.
    #[must_use]
    pub fn leg(&self, leg: crate::types::LegId) -> Option<&Leg> {
        self.legs().get(usize::from(leg.0))
    }

    pub(crate) fn leg_mut(&mut self, leg: crate::types::LegId) -> Option<&mut Leg> {
        if usize::from(leg.0) >= usize::from(self.n_legs) {
            return None;
        }
        self.legs.get_mut(usize::from(leg.0))
    }

    /// The requester's claim, if it still holds one.
    #[must_use]
    pub const fn claim(&self) -> Option<ResIdx> {
        self.claim
    }

    pub(crate) const fn set_claim(&mut self, claim: Option<ResIdx>) {
        self.claim = claim;
    }

    pub(crate) const fn set_state(&mut self, state: RequestState) {
        self.state = state;
    }
}

/// The quote that currently wins a leg, and at what price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// The winning quote.
    pub quote: QuoteIdx,
    /// The price it fills at — always the price of the side the requester is buying.
    pub price: Price,
}
