//! The command set (skeleton).
//!
//! Everything that mutates engine state arrives here, on one bounded channel consumed by
//! one thread (SPEC §13). Payloads land in the stage that implements each transition; the
//! set itself is named now because it is the engine's whole external surface, and because
//! what is *absent* is a design statement: there is no `Deposit`, `RequestWithdrawal` or
//! `ExecuteWithdrawal` here. Those are custody's own API. The engine cannot move money and
//! has never heard of a balance it did not receive through the indexer (SPEC §13.1).
//!
//! Authorisation is designed and not enforced: §16 excludes signatures, so "requester
//! only" and "oracle adapter only" are the gateway's boundary, not the engine's.

/// A state-mutating request to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open a request against one or more legs, reserving `Σ size × limit_price`
    /// (SPEC §5.2). Payload: S2.
    SubmitRequest,
    /// Requester withdraws their own request, releasing every standing quote on it
    /// (SPEC §11). Payload: S2.
    RejectRequest,
    /// Requester accepts, carrying the per-leg prices they were shown; fills at-or-better
    /// or rejects with `PresentationStale` (SPEC §7.1.1). Payload: S2.
    AcceptRequest,
    /// Maker quotes one leg, reserving `leg.size × (UNIT − price)` (SPEC §6). Payload: S2.
    SubmitQuote,
    /// Present as a **rejected** transition, not as an absent one (SPEC §6, §14): quotes
    /// are irrevocable until expiry in v1, and enabling cancellation is then a policy
    /// change rather than a redesign. Payload: S2.
    CancelQuote,
    /// Ask the settlement layer what became of a nonce, and advance the request out of
    /// `Settling` only on a definitive answer (SPEC §8.1). Payload: S4.
    PollSettlement,
    /// Oracle adapter reports `Silent | InProgress | Final(o)`, monotonically
    /// (SPEC §10.1). Payload: S5.
    ReportOracleStatus,
    /// Pay out one escrow, O(1) and idempotent (SPEC §10.2). Payload: S5.
    SettleEscrow,
}
