//! The event set (SPEC §5.2, §7.1.1, §7.2).
//!
//! Events are the engine's only output. `apply` performs no I/O — not logging, not metrics,
//! not `println!` (CLAUDE 8) — and writes into a caller-provided buffer whose headroom is
//! checked in the CHECK phase (CLAUDE 9). The publisher does all I/O, and if it dies the
//! engine keeps applying: the audit trail is best-effort, the state machine is
//! authoritative (§13).
//!
//! [`Event::SubmitIntent`] is the **only** path from the engine to custody (§13.1). It is an
//! event rather than a call because settlement is never invoked inside the commit phase: a
//! fallible custody call there would violate CLAUDE 18 in v1 and be impossible in v2, where
//! settlement is a network round trip. It therefore carries the **whole bundle** — custody
//! cannot reach back into the engine to fetch what it is missing.

use crate::account::AccountIdx;
use crate::config::MAX_LEGS;
use crate::contract::ContractIdx;
use crate::quote::QuoteIdx;
use crate::request::{Nonce, ReqIdx};
use crate::types::{LegId, Price, Side, Size, Ts};

/// One leg of a request, as broadcast to makers.
///
/// **No limit price.** Makers receive the terms they need to price and nothing more: a
/// revealed reserve shades quotes toward the limit rather than toward the maker's true best
/// price (§5.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenLeg {
    /// The contract. The publisher resolves this to the verbatim description through the
    /// gateway; the engine never holds a byte of it.
    pub contract: ContractIdx,
    /// The side the requester is buying, so a maker knows which side they would take.
    pub side: Side,
    /// How much.
    pub size: Size,
}

/// One leg of a settlement bundle, as handed to custody.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntentLeg {
    /// The contract the escrow forms on.
    pub contract: ContractIdx,
    /// The side the requester bought — what the payout mapping consumes (§10.3).
    pub side: Side,
    /// The size.
    pub size: Size,
    /// The winning maker.
    pub maker: AccountIdx,
    /// The fill price. Both contributions derive from it exactly: `size × price` and
    /// `size × (UNIT − price)`, which sum to the notional with no division (§2.1).
    pub fill_price: Price,
    /// The winning quote's expiry. Custody revalidates it against **its own clock** (§9.1),
    /// which is why the value travels with the bundle rather than being looked up.
    pub quote_expiry: Ts,
}

/// Why a quote was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteRejectReason {
    /// A better quote won the leg at accept (§7.2).
    Outbid,
    /// The maker replaced this quote with a better one of their own (§6).
    Replaced,
    /// The requester withdrew the request (§11).
    RequestRejected,
}

/// Something the engine did, for whoever is listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A request is open. Fanned out to makers — **this is the step that makes the venue an
    /// RFQ rather than a private negotiation**: without it no maker learns a request exists
    /// (§5.2).
    RequestOpened {
        /// Which request.
        request: ReqIdx,
        /// When quoting closes.
        deadline: Ts,
        /// The legs, without their limit prices.
        legs: [OpenLeg; MAX_LEGS],
        /// How many are real.
        n_legs: u8,
    },
    /// The best eligible selection on one leg changed, published to the requester on **quote
    /// arrival** — the only point at which the engine emits it (§7.1.1).
    ///
    /// The feed is eventually consistent by design. When the best quote expires and nothing
    /// new arrives, no event is published: normalisation emits nothing, so the change
    /// surfaces on the next command touching that request. That is safe rather than merely
    /// tolerated, because an accept carries the prices the requester saw and fills
    /// at-or-better — a stale view produces `PresentationStale`, never a bad fill.
    ///
    /// An ineligible quote is never selected and never published, so this can never carry a
    /// price above the leg's limit.
    BestSelectionChanged {
        /// Which request.
        request: ReqIdx,
        /// Which leg.
        leg: LegId,
        /// The new best price.
        price: Price,
    },
    /// A quote lost and its reservation was released. Makers are never left inferring the
    /// fate of their capital from silence (§7.2).
    QuoteRejected {
        /// The quote.
        quote: QuoteIdx,
        /// Its maker, so the publisher can route the notice.
        maker: AccountIdx,
        /// Why.
        reason: QuoteRejectReason,
    },
    /// A quote died of expiry.
    ///
    /// Emitted **only** from the accept commit phase, where the count is bounded by
    /// `MAX_LEGS × MAX_QUOTES_PER_LEG`. Expiry outside that path is silent, because
    /// normalisation's count is unbounded (§4.3).
    QuoteExpired {
        /// The quote.
        quote: QuoteIdx,
        /// Its maker.
        maker: AccountIdx,
    },
    /// The one engine-to-custody path: a complete bundle and its nonce (§7.2, §13.1).
    ///
    /// Escrow does not exist yet. It appears when settlement confirms (§8, §9.1); until then
    /// both sides' capital is `committed` and may not be released on a guess (§8.3).
    SubmitIntent {
        /// The request being settled.
        request: ReqIdx,
        /// `(ReqIdx, generation)` — unique and deterministic without hashing (§8.1).
        nonce: Nonce,
        /// The requester, who is debited `Σ size × fill_price`.
        requester: AccountIdx,
        /// The legs.
        legs: [IntentLeg; MAX_LEGS],
        /// How many are real.
        n_legs: u8,
    },
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
