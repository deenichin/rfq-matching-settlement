//! The event set (skeleton).
//!
//! Events are the engine's only output. `apply` performs no I/O — not logging, not
//! metrics, not `println!` (CLAUDE 8) — and writes into a caller-provided buffer whose
//! headroom is checked in the CHECK phase (CLAUDE 9). The publisher does all I/O, and if
//! it dies the engine keeps applying: the audit trail is best-effort, the state machine is
//! authoritative (SPEC §13).
//!
//! [`Event::SubmitIntent`] is the **only** path from the engine to custody (SPEC §13.1).
//! It is an event rather than a call because settlement is never invoked inside the commit
//! phase: a fallible custody call there would violate CLAUDE 18 in v1 and be impossible in
//! v2, where settlement is a network round trip.

/// Something the engine did, for whoever is listening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A request is open: contract description, side, size and deadline, fanned out to
    /// makers. **Never the limit price** (SPEC §5.2) — a revealed reserve shades quotes
    /// toward the limit rather than toward the maker's best price. Without this event no
    /// maker learns a request exists and the venue is not an RFQ. Payload: S2.
    RequestOpened,
    /// The best eligible selection changed, published to the requester on quote arrival —
    /// the only point at which the engine emits it. The feed is eventually consistent by
    /// construction: normalisation emits nothing, so an expiry surfaces on the next
    /// command. Safe because accept binds at-or-better (SPEC §7.1.1). Payload: S2.
    BestSelectionChanged,
    /// A quote lost at selection and its reservation was released. Makers are never left
    /// inferring the fate of their capital from silence (SPEC §7.2). Payload: S2.
    QuoteRejected,
    /// A quote died of expiry. Emitted **only** from the accept commit phase, where the
    /// count is bounded by `MAX_LEGS × MAX_QUOTES_PER_LEG`; expiry outside that path is
    /// silent, because normalisation's count is unbounded (SPEC §4.3). Payload: S2.
    QuoteExpired,
    /// The one engine-to-custody path: a bundle and its nonce, picked up by the settlement
    /// adapter (SPEC §7.2, §13.1). Payload: S2/S3.
    SubmitIntent,
}
