//! Reservations: the core's soft claims against custody (SPEC §2.2, §4.3).
//!
//! A reservation is a promise that a unit will be available. It moves no money — custody
//! has no concept of it — and it is safe only because custody enforces a withdrawal
//! timelock longer than the maximum quote lifetime (§9.3). That is the load-bearing
//! invariant of the whole design.
//!
//! **A claim is linked into exactly one chain, and the type says so.** While `reserved` it
//! sits on its account's expiry-ordered chain; while `committed` it sits on its request's
//! committed list. [`ClaimLinks`] is an enum over those two states rather than a struct
//! with a `state` flag, so a committed claim has **no `expires_at` field at all**. That is
//! what makes §2.4's "may not be released on a guess" structurally true: `release_expired`
//! walks the expiry chain and evaluates an expiry predicate, and neither operation can be
//! written against a committed claim. The impossibility is in the type, not in a guard.

use crate::account::{AccountIdx, Link};
use crate::quote::QuoteIdx;
use crate::request::ReqIdx;
use crate::slab::Handle;
use crate::types::{Amount, Ts};

/// A generation-carrying handle to a reservation.
///
/// Generation-carrying because it is handed out: a caller holding a `ResIdx` across a
/// release-and-reallocate must be refused, not silently pointed at the slot's new
/// occupant (SPEC §15.3).
pub type ResIdx = Handle<Reservation>;

/// What a claim is attached to.
///
/// The enum exists because **requester-side claims have no quote** (§4.3). A maker's claim
/// hangs off the quote that reserved it; the requester's `Σ size × limit_price` is reserved
/// at `SubmitRequest`, before any price exists and before any quote arrives, so it can only
/// hang off the request itself. Defining `committed` as "capital attached to a `Consumed`
/// quote" would cover only the maker half of every trade (§2.4).
///
/// Both variants carry a generation and are checked on dereference (§15.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResOwner {
    /// A maker's claim, backing one live quote.
    Quote(QuoteIdx),
    /// The requester's claim, backing the request itself.
    Request(ReqIdx),
}

/// Which chain a claim is on, and the links threading it.
///
/// Two variants, never both, and never neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimLinks {
    /// On the account's expiry-ordered chain, ordered ascending by `expires_at`.
    ///
    /// Doubly linked: early release when a quote wins or loses at accept is an O(1) unlink
    /// from the middle of the chain (§4.3), which a singly-linked list cannot do.
    Reserved { expires_at: Ts, prev: Link, next: Link },
    /// On the request's committed list — unordered, and with **no expiry**.
    ///
    /// Unordered because nothing ever asks for the earliest: the list is walked whole, when
    /// settlement confirms or definitively fails. It therefore needs no tail pointer, and
    /// insertion is at the head. The asymmetry with the expiry chain, which does keep a
    /// tail so insertion can walk back from it, is the ordering requirement showing through.
    Committed { request: ReqIdx, prev: Link, next: Link },
}

/// One claim on one account's balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    account: AccountIdx,
    amount: Amount,
    owner: ResOwner,
    links: ClaimLinks,
}

impl Reservation {
    /// A fresh claim, linked nowhere yet. The ledger links it before anyone can see it.
    pub(crate) const fn reserved(
        account: AccountIdx,
        amount: Amount,
        owner: ResOwner,
        expires_at: Ts,
    ) -> Self {
        Self {
            account,
            amount,
            owner,
            links: ClaimLinks::Reserved { expires_at, prev: Link::NIL, next: Link::NIL },
        }
    }

    /// Whose balance this claim is against.
    #[must_use]
    pub const fn account(&self) -> AccountIdx {
        self.account
    }

    /// How much is claimed.
    #[must_use]
    pub const fn amount(&self) -> Amount {
        self.amount
    }

    /// What the claim is attached to (§15.3).
    #[must_use]
    pub const fn owner(&self) -> ResOwner {
        self.owner
    }

    /// When this claim expires, or `None` if it is committed.
    ///
    /// `None` is not "unknown" — a committed claim has no expiry, and its only exits are
    /// settlement confirming or definitively failing (§2.4).
    #[must_use]
    pub const fn expires_at(&self) -> Option<Ts> {
        match self.links {
            ClaimLinks::Reserved { expires_at, .. } => Some(expires_at),
            ClaimLinks::Committed { .. } => None,
        }
    }

    /// Whether this claim is committed.
    #[must_use]
    pub const fn is_committed(&self) -> bool {
        matches!(self.links, ClaimLinks::Committed { .. })
    }

    /// The request whose committed list holds this claim, or `None` if it is reserved.
    #[must_use]
    pub const fn committed_to(&self) -> Option<ReqIdx> {
        match self.links {
            ClaimLinks::Reserved { .. } => None,
            ClaimLinks::Committed { request, .. } => Some(request),
        }
    }

    /// Whether this claim is dead at `now`. Half-open: a claim expiring at exactly `now`
    /// is dead (SPEC §4.2).
    ///
    /// Only answerable for a reserved claim; the caller reaches this through the expiry
    /// chain, which committed claims are not on.
    pub(crate) const fn is_expired_at(&self, now: Ts) -> Option<bool> {
        match self.links {
            ClaimLinks::Reserved { expires_at, .. } => Some(now.0 >= expires_at.0),
            ClaimLinks::Committed { .. } => None,
        }
    }

    pub(crate) const fn links(&self) -> ClaimLinks {
        self.links
    }

    pub(crate) const fn set_links(&mut self, links: ClaimLinks) {
        self.links = links;
    }

    /// The chain neighbours, whichever chain this claim is on.
    pub(crate) const fn neighbours(&self) -> (Link, Link) {
        match self.links {
            ClaimLinks::Reserved { prev, next, .. } | ClaimLinks::Committed { prev, next, .. } => {
                (prev, next)
            }
        }
    }

    pub(crate) const fn set_prev(&mut self, link: Link) {
        match &mut self.links {
            ClaimLinks::Reserved { prev, .. } | ClaimLinks::Committed { prev, .. } => *prev = link,
        }
    }

    pub(crate) const fn set_next(&mut self, link: Link) {
        match &mut self.links {
            ClaimLinks::Reserved { next, .. } | ClaimLinks::Committed { next, .. } => *next = link,
        }
    }
}
