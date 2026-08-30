//! Requests, as far as S1 needs them.
//!
//! A request is the owner of the requester's claim and the holder of the **committed
//! list**: the one place both sides of a trade in `Settling` are attached (§2.4). Those are
//! the two things the ledger touches, and they are all that exists here. Legs, sides,
//! limit prices, the deadline and the request state machine are S2's; this file grows, it
//! does not get replaced.

use crate::account::Link;
use crate::reservation::ResIdx;
use crate::slab::Handle;

/// A generation-carrying handle to a request.
///
/// The generation is not bookkeeping: `(ReqIdx, generation)` **is** the settlement nonce
/// (§8.1). A request slot reused after its predecessor was freed yields a different
/// generation, so nonces are never reused even though indices are — which is what lets a
/// resubmission bounce off its own nonce instead of forming a second escrow.
pub type ReqIdx = Handle<Request>;

/// The owner side of the requester's claim, and the head of the committed list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The requester's own claim, or `None` if it has been released.
    ///
    /// A generation-carrying handle, not a bare index: invariant 3 checks that the owner
    /// resolves *and points back at the claim*, and a bare index would pass that check
    /// against whatever now occupies a freed slot — precisely the stale-handle case the
    /// assertion exists for (§15.3).
    claim: Option<ResIdx>,
    /// Head of this request's committed list. No tail: the list is unordered, insertion is
    /// at the head, and nothing ever asks for its last element (§4.3).
    pub(crate) committed_head: Link,
}

impl Request {
    /// A request owning nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { claim: None, committed_head: Link::NIL }
    }

    /// The requester's claim, if it still holds one.
    #[must_use]
    pub const fn claim(&self) -> Option<ResIdx> {
        self.claim
    }

    pub(crate) const fn set_claim(&mut self, claim: Option<ResIdx>) {
        self.claim = claim;
    }
}
