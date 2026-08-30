//! The command set.
//!
//! Everything that mutates engine state arrives here, on one bounded channel consumed by
//! one thread (SPEC §13). What is **absent** is a design statement: there is no `Deposit`,
//! `RequestWithdrawal` or `ExecuteWithdrawal`. Those are custody's own API. The engine
//! cannot move money and has never heard of a balance it did not receive through the
//! indexer (SPEC §13.1).
//!
//! Nothing here names a **reservation**. Participants name a quote or a request; the claim
//! backing it is the engine's bookkeeping, not an address a client can hold.
//!
//! Nothing here carries a **description** either. External identifiers — account keys,
//! contract wording, resolution sources — exist only at the gateway, which converts them to
//! dense indices before a command is ever built (§3, CLAUDE 11).
//!
//! `ReportOracleStatus` and `SettleEscrow` (S5) arrive with the stages that can apply them.
//! A named variant nothing can execute is a stub.
//!
//! Authorisation is designed and not enforced: §16 excludes signatures, so "requester only"
//! and "oracle adapter only" are the gateway's boundary, not the engine's.

use crate::account::AccountIdx;
use crate::config::MAX_LEGS;
use crate::contract::ContractIdx;
use crate::quote::QuoteIdx;
use crate::request::ReqIdx;
use crate::settlement::TxStatus;
use crate::types::{Amount, LegId, Price, Side, Size, Ts};

/// One leg of a `SubmitRequest`: what to trade, which side, how much, and the most the
/// requester will pay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegSpec {
    /// The contract, already resolved to an index by the gateway.
    pub contract: ContractIdx,
    /// The side the **requester** is buying. The maker takes the opposite (§2.1).
    pub side: Side,
    /// How many contracts. A fill is all of it or none of it.
    pub size: Size,
    /// The limit. Never broadcast, never checked at admission, enforced at selection (§5.2).
    pub limit: Price,
}

/// The requester's view of one leg at the moment they accepted (§7.1.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpectedFill {
    /// Which leg.
    pub leg: LegId,
    /// The price the requester was shown.
    pub price: Price,
}

/// A state-mutating request to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// A chain event moved an account's balance; update the mirror (§2.3, §12).
    ///
    /// Produced by the indexer, never by a participant. The core never *computes* a
    /// balance — it only learns one.
    CreditAccount {
        /// Whose balance moved.
        account: AccountIdx,
        /// The new mirrored free balance.
        free: Amount,
    },
    /// Record a contract the gateway has just assigned an index to (§5.3).
    ///
    /// The description that *is* the identity stays at the gateway; the core learns only the
    /// index and the event date it needs for the `ContractTooNear` check.
    RegisterContract {
        /// The index the gateway assigned by byte equality over description, event date and
        /// resolution source.
        contract: ContractIdx,
        /// When the event settles.
        event_date: Ts,
    },
    /// Open a request and reserve `Σ size × limit` of the requester's balance against it
    /// (§5.2), before any price exists.
    SubmitRequest {
        /// Who is asking.
        requester: AccountIdx,
        /// When quoting closes. There is no post-deadline acceptance window (§5.1).
        deadline: Ts,
        /// The legs. Fixed-size because `MAX_LEGS` is a compile-time storage bound (§3).
        legs: [LegSpec; MAX_LEGS],
        /// How many of them are real.
        n_legs: u8,
    },
    /// The requester withdraws their request, releasing every standing quote on it (§11).
    RejectRequest {
        /// Which request.
        request: ReqIdx,
    },
    /// A maker's firm offer on one leg, reserving `leg.size × (UNIT − price)` (§6).
    ///
    /// **Never refused on price.** A quote above the leg's limit is admitted, reserves
    /// capital normally, and loses at selection.
    SubmitQuote {
        /// The maker.
        maker: AccountIdx,
        /// Which request.
        request: ReqIdx,
        /// Which leg.
        leg: LegId,
        /// The price of the side the requester is buying.
        price: Price,
        /// How much the maker will fill. Must cover the leg in full.
        size: Size,
        /// When the offer stops binding. Absolute, and the maker's own exposure control.
        expires_at: Ts,
    },
    /// Present as a **rejected** transition, not as an absent one (§6, §14).
    ///
    /// Quotes are irrevocable until expiry in v1, so this always fails — but it fails with
    /// its own variant, and enabling cancellation later is a policy change rather than a
    /// redesign.
    CancelQuote {
        /// The quote the maker wishes they had not written.
        quote: QuoteIdx,
    },
    /// Report what a poller observed about a settling request's nonce (§8).
    ///
    /// An ordinary command from an external actor, on the same queue as everything else.
    /// The engine never calls custody to ask: the status is *carried in*, because the only
    /// path from custody back to the engine is a fact translated into a command (§13.1).
    ///
    /// Anyone may send it — the indexer, in practice. There is nothing to authorise: the
    /// command carries no discretion, and a wrong status is a lying poller, which is the
    /// same trust boundary the oracle sits behind.
    PollSettlement {
        /// Which request.
        request: ReqIdx,
        /// The fate of that request's nonce, as observed.
        status: TxStatus,
    },
    /// The requester accepts, carrying the per-leg prices they were shown (§7.1.1).
    ///
    /// Filled **at or better** than every expected price. A strictly better fill is accepted
    /// silently; a worse one rejects the whole request with `PresentationStale`.
    AcceptRequest {
        /// Which request.
        request: ReqIdx,
        /// The requester's view, per leg.
        expected: [ExpectedFill; MAX_LEGS],
        /// How many legs the view covers. Must equal the request's leg count.
        n_legs: u8,
    },
}
