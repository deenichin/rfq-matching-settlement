//! Contracts, as the core sees them (SPEC §5.3).
//!
//! **The core never sees a description.** Identity is byte equality over the full
//! description plus event date plus resolution source, and that comparison happens at the
//! gateway, which owns `description_bytes → ContractIdx` and hands the core an index. No
//! hashing, no string comparison, nothing reachable from `apply` that could allocate
//! (SPEC §3, CLAUDE 11).
//!
//! Why byte equality and not a hash: a non-cryptographic hash would be strictly worse here,
//! because a collision would let a trade formed on one contract resolve under another's
//! outcome, and contract identity is an adversarial surface (§11). A cryptographic hash
//! would buy nothing over byte equality while adding a dependency. Nothing in v1 puts a
//! contract id on a wire or a chain, so there is nothing to compress.

use crate::types::{Dur, Side, Ts};

/// How a binary contract resolved.
///
/// `Void` is an **outcome value, not a state**, so ambiguity needs no special path through
/// the machine (§10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The event happened.
    Yes,
    /// It did not.
    No,
    /// It cannot be decided. Each side's own contribution is returned (§10.3).
    Void,
}

/// What the oracle has told the venue, and nothing more (§10.1).
///
/// The engine models **no** proposal, dispute, bonding or voting. Those belong to whatever
/// oracle the venue integrates — an optimistic oracle, a trusted signer, a committee — and
/// importing their lifecycle would couple this state machine to a system it does not control
/// and cannot fix. Three values is the entire interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleStatus {
    /// Nothing has happened.
    Silent,
    /// Proposed and/or contested — working, but not final.
    InProgress,
    /// Decided. Terminal and immutable.
    Final(Outcome),
}

/// The engine's contract state. **Two values** (§10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractState {
    /// No outcome yet.
    Unresolved,
    /// Decided.
    Resolved(Outcome),
}

/// The outcome is not yet derivable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotYet;

/// A dense contract index, assigned by the gateway on first sight.
///
/// **No generation, and deliberately.** SPEC §3's table calls this a slab, but nothing ever
/// frees a contract — escrows resolve against it for months after the trade — so an index
/// is never reused and a stale one cannot exist. That is the same argument that makes
/// [`AccountIdx`](crate::account::AccountIdx) generation-free, and a generation here would
/// be ceremony that proves nothing. Flagged rather than assumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractIdx(pub u32);

/// What the core knows about a contract.
///
/// Two fields' worth in S2: the index it is addressed by, and the date the event settles.
/// Resolution state — `Unresolved | Resolved(Outcome)` and the oracle status — arrives in
/// S5 (§10.1). The description, the resolution source and every byte of wording stay at the
/// gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contract {
    event_date: Ts,
    state: ContractState,
    oracle_status: OracleStatus,
}

impl Contract {
    /// A contract settling at `event_date`, with nothing reported about it yet.
    #[must_use]
    pub const fn new(event_date: Ts) -> Self {
        Self {
            event_date,
            state: ContractState::Unresolved,
            oracle_status: OracleStatus::Silent,
        }
    }

    /// The engine's resolution state.
    #[must_use]
    pub const fn state(&self) -> ContractState {
        self.state
    }

    /// What the oracle has last reported.
    #[must_use]
    pub const fn oracle_status(&self) -> OracleStatus {
        self.oracle_status
    }

    /// Whether `next` is an admissible successor to the current status.
    ///
    /// Monotonic: `Silent → InProgress → Final(o)`, and `Final` is terminal and immutable.
    /// This is not hygiene. A retraction to `Silent` re-opens the stall exit that §10.4
    /// exists to close, turning contestation into a costless option to cancel a trade
    /// already lost. An overwrite of `Final` is worse and **invisible to every invariant in
    /// §15**: escrows already settled keep their payout, while unsettled escrows on the same
    /// contract pay the other side, so one contract pays identical positions opposite
    /// results decided by who sent `SettleEscrow` first. No unit is duplicated — each escrow
    /// pays its own notional — so conservation cannot detect it.
    ///
    /// It is the same rule as nonce monotonicity (§8.1) one layer down: a status that can
    /// regress lets a later, less-informed observation overwrite an earlier, better-informed
    /// one.
    #[must_use]
    pub const fn may_report(&self, next: OracleStatus) -> bool {
        // Read as a rank: `Silent` 0, `InProgress` 1, `Final` 2. A report is admissible only
        // if it moves strictly forwards, which makes `Final` terminal against every status
        // including itself and makes a retraction to `Silent` impossible from anywhere.
        const fn rank(status: OracleStatus) -> u8 {
            match status {
                OracleStatus::Silent => 0,
                OracleStatus::InProgress => 1,
                OracleStatus::Final(_) => 2,
            }
        }
        rank(next) > rank(self.oracle_status)
    }

    pub(crate) const fn set_oracle_status(&mut self, status: OracleStatus) {
        self.oracle_status = status;
        if let OracleStatus::Final(outcome) = status {
            self.state = ContractState::Resolved(outcome);
        }
    }

    /// The outcome, **derived at the point of use and never stored as finalised** (§10.2).
    ///
    /// The stall exit conditions on `Silent`, not on the absence of a proposal. `InProgress`
    /// never times out into `Void`, which preserves §10.4's property without the engine
    /// knowing anything about how the oracle reaches finality — otherwise a party who is
    /// losing contests a correct outcome, waits out the grace period, and takes a free
    /// unwind.
    ///
    /// No timer sets a resolved flag. Time gates *admissibility*; an explicit command moves
    /// the money, which is faithful to the layer being mocked: chains have no timers, and a
    /// contract cannot pay spontaneously.
    ///
    /// # Errors
    ///
    /// [`NotYet`] while the outcome is undecidable.
    pub fn outcome(&self, now: Ts, stall_grace: Dur) -> Result<Outcome, NotYet> {
        if let ContractState::Resolved(outcome) = self.state {
            return Ok(outcome);
        }
        if matches!(self.oracle_status, OracleStatus::Silent)
            && let Some(stall_exit) = self.event_date.checked_add(stall_grace)
            && now > stall_exit
        {
            // Triggerable only by time, never by a participant, and STALL_GRACE is long
            // relative to any plausible honest delay (§10.4).
            return Ok(Outcome::Void);
        }
        Err(NotYet)
    }

    /// When the event this contract is about occurs.
    ///
    /// Gates admission: `SubmitRequest` is refused if any leg's contract has
    /// `now >= event_date − MIN_HORIZON` (§5.2), so escrow can never form on a contract
    /// whose stall grace has already elapsed.
    #[must_use]
    pub const fn event_date(&self) -> Ts {
        self.event_date
    }
}

/// Who a settled escrow pays, given the outcome and the side the **requester** bought.
///
/// There is no implicit buyer or seller: who wins on `Yes` is a property of the leg, not a
/// convention (§10.3). The requester wins exactly when the outcome matches the side they
/// bought, which is the whole mapping in one line.
///
/// `None` is `Void`, and `Void` is **not a 50/50 split**: refunding each side's own
/// contribution restores the exact pre-trade allocation, while splitting the notional moves
/// money between the parties and is a redistribution disguised as neutrality.
#[must_use]
pub const fn payout_goes_to_requester(outcome: Outcome, requester_side: Side) -> Option<bool> {
    match (outcome, requester_side) {
        (Outcome::Yes, Side::Yes) | (Outcome::No, Side::No) => Some(true),
        (Outcome::Yes, Side::No) | (Outcome::No, Side::Yes) => Some(false),
        (Outcome::Void, _) => None,
    }
}
