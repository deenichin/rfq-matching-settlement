//! The command set.
//!
//! Everything that mutates engine state arrives here, on one bounded channel consumed by
//! one thread (SPEC §13). What is **absent** is a design statement: there is no `Deposit`,
//! `RequestWithdrawal` or `ExecuteWithdrawal`. Those are custody's own API. The engine
//! cannot move money and has never heard of a balance it did not receive through the
//! indexer (SPEC §13.1).
//!
//! Nothing here names a **reservation**. Participants name a quote or a request; the claim
//! backing it is the engine's bookkeeping, not an address a client can hold. That is why
//! [`Command::OpenQuote`] creates the quote *and* its claim in one command rather than
//! leaving a client to reserve against a handle it has no way to learn.
//!
//! This set is what the S1 ledger supports, and each variant is the ancestor of the S2
//! command that will subsume it:
//!
//! | Here | Becomes | Adds |
//! |---|---|---|
//! | `CreditAccount` | an indexer-produced mirror update (S6) | confirmation depth, dedup |
//! | `OpenQuote` | `SubmitQuote` (S2) | leg, side, price, size, the per-leg chain |
//! | `OpenRequest` | `SubmitRequest` (S2) | legs, contracts, deadline, limit prices |
//! | `CloseQuote` | the release half of the quote lifecycle (S2) | `Consumed`/`Released`, `QuoteRejected` |
//! | `CommitQuote` | one step of `AcceptRequest`'s commit phase (S2) | selection, atomicity across legs |
//!
//! `RejectRequest`, `AcceptRequest`, `CancelQuote`, `PollSettlement`, `ReportOracleStatus`
//! and `SettleEscrow` arrive with the stages that can apply them. A named variant nothing
//! can execute is a stub, and a command log over an uninhabited enum proves nothing.
//!
//! Authorisation is designed and not enforced: §16 excludes signatures, so "requester only"
//! and "oracle adapter only" are the gateway's boundary, not the engine's.

use crate::account::AccountIdx;
use crate::quote::QuoteIdx;
use crate::request::ReqIdx;
use crate::types::{Amount, Dur};

/// A state-mutating request to the engine.
///
/// `Copy`, so the command log can hold the exact value that was applied without cloning it
/// out of the money path (CLAUDE 12).
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
    /// Open a quote and reserve `amount` of the maker's balance against it, live for `ttl`
    /// from the sampled `now`.
    ///
    /// A **duration**, not an instant: the expiry is derived from the `now` the engine
    /// samples, which is what makes the command log's recorded instant load-bearing for
    /// replay. A participant-supplied absolute timestamp would be advisory and never
    /// trusted (§4.1).
    OpenQuote {
        /// The maker.
        maker: AccountIdx,
        /// The maker's contribution.
        amount: Amount,
        /// How long the quote binds. `MAX_QUOTE_TTL` is enforced in S2, where a quote has
        /// a price to bind at.
        ttl: Dur,
    },
    /// Open a request and reserve the requester's claim against it, live for `ttl`.
    OpenRequest {
        /// The requester.
        requester: AccountIdx,
        /// The requester's reservation.
        amount: Amount,
        /// How long the request stands.
        ttl: Dur,
    },
    /// A quote leaves the book: its claim is released and its slot freed.
    CloseQuote {
        /// The quote. A stale handle is refused, not silently resolved to the slot's new
        /// occupant (§15.3).
        quote: QuoteIdx,
    },
    /// Move a quote's claim `reserved → committed` into a request's committed list
    /// (§2.4, §7.2).
    CommitQuote {
        /// The quote whose claim moves.
        quote: QuoteIdx,
        /// The request whose committed list receives it.
        request: ReqIdx,
    },
}
