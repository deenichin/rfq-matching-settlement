//! The engine: the single-writer state machine.
//!
//! Holds requests, quotes, contracts, reservations, claims and the read-only balance mirror.
//! It holds no reference to custody and cannot read a balance from it; it sees `EscrowId` and
//! its own mirror, nothing else (SPEC §13.1).
//!
//! The engine does **not** own a clock. `apply(cmd, now)` samples time exactly once at the
//! call site (CLAUDE 2), so the runtime samples it and passes the value in. Re-reading the
//! clock mid-command is therefore unreachable rather than merely forbidden — which is also
//! what makes the command log replayable.
//!
//! `apply` performs **no I/O** — not logging, not metrics, not `println!` (CLAUDE 8) — and
//! allocates nothing on any path (CLAUDE 9).

use crate::account::AccountIdx;
use crate::command::{Command, ExpectedFill, LegSpec};
use crate::config::{Config, ConfigError, MAX_LEGS};
use crate::contract::{ContractIdx, OracleStatus};
use crate::escrow::EscrowId;
use crate::event::{Event, EventBuffer, EventBufferFull, IntentLeg, OpenLeg, QuoteRejectReason};
use crate::ledger::{Ledger, LedgerError};
use crate::quote::{Quote, QuoteIdx, QuoteState};
use crate::request::{Leg, Nonce, ReqIdx, Request, RequestState, Selection};
use crate::reservation::{ResOwner, Reservation};
use crate::selection::{NoQuoteReason, best_on_leg, standing_quote_of, walk_leg};
use crate::settlement::TxStatus;
use crate::types::{Amount, LegId, Price, Size, Ts, UNIT};

/// A rejected command. Each variant names the specific cause (CLAUDE 24); there is no
/// generic `InvalidRequest`, because the failure-mode notes are diffed against these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The ledger refused. The inner variant names which rule was violated.
    Ledger(LedgerError),
    /// The command would emit more events than the caller's buffer has headroom for.
    ///
    /// Checked in CHECK, so the commit phase never has to. Reachable only by construction,
    /// never by quote volume: the accept commit phase's event count is statically bounded by
    /// `MAX_LEGS × MAX_QUOTES_PER_LEG` (§4.3, §6).
    EventBufferFull,
    /// Timestamp arithmetic overflowed.
    ExpiryOverflow,
    /// A money product or sum overflowed. A rejection, never a wrap (CLAUDE 14).
    AmountOverflow,

    // ── SubmitRequest (§5.2) ──
    /// `deadline − now > MAX_REQUEST_TTL`.
    DeadlineTooFar,
    /// `deadline <= now`: a request nobody could quote.
    DeadlineInThePast,
    /// A leg's contract has `now >= event_date − MIN_HORIZON`, so escrow could form on a
    /// contract whose stall grace has already elapsed — a free capital round-trip against
    /// makers (§5.2).
    ContractTooNear,
    /// A leg names a contract index the gateway never registered.
    UnknownContract,
    /// More legs than `config.max_legs` allows.
    TooManyLegs,
    /// A request with no legs.
    NoLegs,
    /// A price outside `[0, UNIT]` (§2.1).
    PriceOutOfRange,
    /// A leg of zero size, which would reserve nothing and fill nothing.
    ZeroSize,

    // ── SubmitQuote (§6) ──
    /// No such request.
    UnknownRequest,
    /// The request is not `Open`.
    RequestNotOpen,
    /// `now >= request.deadline`. There is no post-deadline window (§5.1).
    RequestDeadlinePassed,
    /// No such leg on this request.
    UnknownLeg,
    /// `now >= expires_at`: a quote that is dead on arrival.
    QuoteExpiryInThePast,
    /// `expires_at − now > MAX_QUOTE_TTL`.
    QuoteTtlTooLong,
    /// `size < leg.size`. Full size or nothing (§7.1) — and this admission check is what
    /// makes [`NoQuoteReason::SizeTooSmall`] unreachable at selection.
    QuoteTooSmall,
    /// A maker's replacement is priced worse for the requester than their standing quote.
    ///
    /// Without this, replacement is a cancel primitive in disguise: requote at
    /// `price == UNIT`, which reserves zero, and the maker has withdrawn liquidity they
    /// promised was irrevocable (§6).
    WorseReplacement,
    /// The leg already holds `MAX_QUOTES_PER_LEG` live quotes.
    LegQuoteLimitReached,

    // ── CancelQuote (§6, §14) ──
    /// Quotes are irrevocable until expiry in v1. A **rejected transition**, not an absent
    /// one: the maker's exposure control is the expiry they chose, and enabling cancellation
    /// is a policy change rather than a redesign.
    QuotesAreIrrevocable,
    /// No such quote.
    UnknownQuote,

    // ── AcceptRequest (§7) ──
    /// The accept's view covers a different number of legs than the request has.
    LegCountMismatch,
    /// A poll named a nonce whose request slot has been freed and reissued, so it names a
    /// request that no longer exists (§8.1).
    StaleNonce,
    /// A poll named a request that is not `Settling`. There is no nonce to report on, and a
    /// terminal request must not be moved again (§8.1's monotonicity, one level up).
    RequestNotSettling,
    /// The oracle tried to walk its status backwards, or to overwrite a `Final` (§10.1).
    ///
    /// Load-bearing rather than hygiene: a retraction re-opens the stall exit that §10.4
    /// closes, and an overwrite makes one contract pay identical positions opposite results
    /// decided by settlement order — which no invariant in §15 can see.
    OracleStatusRegression,
    /// The contract has no outcome yet: the oracle has not spoken and the stall grace has
    /// not elapsed (§10.2). Nothing moves, and the escrow stays locked.
    OutcomeNotYet,
    /// A leg has no fill, so the whole basket aborts (§7.2).
    NoEligibleQuote {
        /// Which leg.
        leg: u8,
        /// Which of §7.2's four causes.
        reason: NoQuoteReason,
    },
    /// A leg would fill worse than the requester was shown (§7.1.1).
    ///
    /// A normal outcome, not an error: the accept window is short and maker-controlled. The
    /// client re-reads the feed and re-accepts.
    PresentationStale {
        /// Which leg.
        leg: u8,
        /// What the requester expected.
        expected: Price,
        /// What it would actually fill at.
        actual: Price,
    },
    /// The requester's reservation does not cover `Σ` contributions.
    ///
    /// Unreachable by construction — eligibility requires `fill <= limit` on every leg, and
    /// the reservation is `Σ size × limit` — and checked anyway, because §7.2 lists it and a
    /// CHECK that cannot fail still documents what the commit phase is entitled to assume.
    ReservationShortfall,
}

impl From<LedgerError> for EngineError {
    fn from(error: LedgerError) -> Self {
        Self::Ledger(error)
    }
}

impl From<EventBufferFull> for EngineError {
    fn from(_: EventBufferFull) -> Self {
        Self::EventBufferFull
    }
}

/// The single-writer state machine.
#[derive(Debug, PartialEq, Eq)]
pub struct Engine {
    config: Config,
    ledger: Ledger,
    /// Arrival order for quotes — the selection tiebreak (§7.1). Engine state, not a clock
    /// reading, so it reproduces exactly under replay.
    next_arrival: u64,
}

impl Engine {
    /// Validate the configuration and construct the engine.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`]. A venue whose withdrawal timelock does not cover the maximum
    /// quote lifetime plus mirror lag must not start (§9.3).
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let ledger = Ledger::new(&config);
        Ok(Self { config, ledger, next_arrival: 0 })
    }

    /// The venue policy this engine was started with.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// The claim ledger and every record in it, read-only.
    #[must_use]
    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Apply one command at the instant `now`.
    ///
    /// `now` is sampled **once**, by the caller, and every predicate evaluated during this
    /// command uses that one value (§4.1, CLAUDE 2) — so legs of a multi-leg accept cannot
    /// disagree about what time it is.
    ///
    /// 1. **NORMALISE** — `release_expired` for every account the command touches. Mutates
    ///    state, depends only on `(accounts, now)`, never on the command's content or on
    ///    whether it will be accepted. Emits **no events** (§4.3, CLAUDE 9).
    /// 2. **PLAN / CHECK / COMMIT** — per §7.2.
    ///
    /// This is why the no-mutation-on-rejection guarantee is stated relative to the
    /// **post-normalisation** state (§15.4): normalisation is time catching up, not the
    /// command acting.
    ///
    /// # Errors
    ///
    /// [`EngineError`]. A rejection leaves the post-normalisation state untouched.
    ///
    /// # Panics
    ///
    /// In debug builds, if §15.3's claim/owner coherence does not hold after the command.
    /// That is an engine bug, not an input error, and the venue must not continue on it.
    pub fn apply(
        &mut self,
        command: Command,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        debug_assert!(events.is_empty(), "apply is handed a drained buffer");

        // ── NORMALISE ──
        let touched = self.touched_accounts(command);
        for account in touched.into_iter().flatten() {
            // An unknown account has nothing to reclaim; the command's own validation is
            // what reports it, and normalisation must not depend on the command's fate.
            let _ = self.ledger.release_expired(account, now);
        }
        debug_assert!(events.is_empty(), "normalisation emits no events (SPEC §4.3)");

        // ── PLAN / CHECK / COMMIT ──
        let outcome = match command {
            Command::CreditAccount { account, free } => {
                self.ledger.apply_mirror_update(account, free)?;
                Ok(())
            }
            Command::RegisterContract { contract, event_date } => {
                self.ledger.register_contract(contract, event_date)?;
                Ok(())
            }
            Command::SubmitRequest { requester, deadline, legs, n_legs } => {
                self.submit_request(requester, deadline, &legs, n_legs, now, events)
            }
            Command::RejectRequest { request } => self.reject_request(request, now, events),
            Command::SubmitQuote { maker, request, leg, price, size, expires_at } => {
                self.submit_quote(maker, request, leg, price, size, expires_at, now, events)
            }
            Command::CancelQuote { quote } => {
                // A rejected transition, not an absent one (§6, §14). The lookup happens so
                // that an unknown quote and an irrevocable one are distinguishable, which is
                // what makes this a *transition* rather than a blanket refusal.
                if self.ledger.quote(quote).is_none() {
                    return Err(EngineError::UnknownQuote);
                }
                Err(EngineError::QuotesAreIrrevocable)
            }
            Command::AcceptRequest { request, expected, n_legs } => {
                self.accept_request(request, &expected, n_legs, now, events)
            }
            Command::PollSettlement { nonce, status } => {
                self.poll_settlement(nonce, status, now, events)
            }
            Command::ReportOracleStatus { contract, status } => {
                self.report_oracle_status(contract, status, events)
            }
            Command::SettleEscrow { escrow, contract } => {
                self.settle_escrow(escrow, contract, now, events)
            }
        };

        // §15.3's state-coherence half is a whole-command property: a commit phase that
        // moves several claims passes through states no single field write can avoid.
        if cfg!(debug_assertions)
            && let Err(violation) = self.ledger.check_claim_state_coherence()
        {
            panic!("claim/owner coherence violated after a command (SPEC §15.3): {violation:?}");
        }
        outcome
    }

    /// Run normalisation for one account, as `apply` does before every command.
    ///
    /// Exposed so a test can take the **post-normalisation** snapshot that SPEC §15.4 states
    /// the no-mutation guarantee against. Taking it before normalisation would compare
    /// against a state the design never claimed, and the invariant would then be weakened to
    /// accommodate the difference — which is the trap §4.3 names.
    pub fn normalise(&mut self, account: AccountIdx, now: Ts) {
        let _ = self.ledger.release_expired(account, now);
    }

    /// Which accounts this command touches, and therefore which normalisation reclaims from.
    ///
    /// Derived from the command's *addressing*, not from whether it will succeed. Returns a
    /// fixed-size array so nothing allocates; unused slots are `None`.
    fn touched_accounts(&self, command: Command) -> [Option<AccountIdx>; 2] {
        let mut touched: [Option<AccountIdx>; 2] = [None, None];
        match command {
            Command::CreditAccount { account, .. } => touched[0] = Some(account),
            Command::RegisterContract { .. }
            | Command::ReportOracleStatus { .. }
            | Command::SettleEscrow { .. } => {}
            Command::SubmitRequest { requester, .. } => touched[0] = Some(requester),
            Command::SubmitQuote { maker, request, .. } => {
                touched[0] = Some(maker);
                touched[1] = self.ledger.request(request).map(Request::requester);
            }
            Command::RejectRequest { request }
            | Command::AcceptRequest { request, .. }
             => {
                touched[0] = self.ledger.request(request).map(Request::requester);
            }
            Command::PollSettlement { nonce, .. } => {
                touched[0] = self
                    .resolve_nonce(nonce)
                    .and_then(|request| self.ledger.request(request))
                    .map(Request::requester);
            }
            Command::CancelQuote { quote } => {
                touched[0] = self.ledger.quote(quote).map(Quote::maker);
            }
        }
        touched
    }

    // ────────────────────────────── SubmitRequest (§5.2) ──────────────────────────────

    fn submit_request(
        &mut self,
        requester: AccountIdx,
        deadline: Ts,
        legs: &[LegSpec; MAX_LEGS],
        n_legs: u8,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        if n_legs == 0 {
            return Err(EngineError::NoLegs);
        }
        if usize::from(n_legs) > usize::from(self.config.max_legs) {
            return Err(EngineError::TooManyLegs);
        }
        if deadline <= now {
            return Err(EngineError::DeadlineInThePast);
        }
        if deadline.saturating_sub(now) > self.config.max_request_ttl {
            return Err(EngineError::DeadlineTooFar);
        }

        let mut stored: [Leg; MAX_LEGS] = [Leg::default(); MAX_LEGS];
        let mut open: [OpenLeg; MAX_LEGS] = [OpenLeg::default(); MAX_LEGS];
        let mut reservation = Amount::ZERO;

        for index in 0..usize::from(n_legs) {
            let spec = legs.get(index).copied().ok_or(EngineError::TooManyLegs)?;
            if spec.size == Size(0) {
                return Err(EngineError::ZeroSize);
            }
            if spec.limit > UNIT {
                return Err(EngineError::PriceOutOfRange);
            }
            let contract =
                self.ledger.contract(spec.contract).ok_or(EngineError::UnknownContract)?;
            // §5.2 with §9.3's companion assertion: together they guarantee that a request
            // accepted at its last legal instant still forms escrow strictly before the
            // event date.
            let horizon = contract
                .event_date()
                .0
                .checked_sub(self.config.min_horizon.0)
                .ok_or(EngineError::ContractTooNear)?;
            if now.0 >= horizon {
                return Err(EngineError::ContractTooNear);
            }

            // The reservation is `Σ size × limit`. Without a limit price the only safe
            // reservation is worst case — full notional per leg — which is capital-brutal
            // for no benefit (§5.2).
            let leg_reservation =
                spec.size.checked_mul(spec.limit).ok_or(EngineError::AmountOverflow)?;
            reservation =
                reservation.checked_add(leg_reservation).ok_or(EngineError::AmountOverflow)?;

            if let Some(slot) = stored.get_mut(index) {
                *slot = Leg::new(spec.contract, spec.side, spec.size, spec.limit);
            }
            if let Some(slot) = open.get_mut(index) {
                // Contract, side and size. **No limit price** (§5.2).
                *slot = OpenLeg { contract: spec.contract, side: spec.side, size: spec.size };
            }
        }

        events.headroom(1)?;

        // The requester's claim expires with the request: after the deadline the request is
        // `Expired` (derived) and the capital is reclaimed by release-on-access (§5).
        let record = Request::new(requester, deadline, stored, n_legs);
        let (request, _claim) = self.ledger.open_request_reserving(record, reservation)?;

        // ── COMMIT ──
        events.push(Event::RequestOpened { request, deadline, legs: open, n_legs });
        Ok(())
    }

    // ────────────────────────────── RejectRequest (§11) ──────────────────────────────

    fn reject_request(
        &mut self,
        request: ReqIdx,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        let record = self.ledger.request(request).ok_or(EngineError::UnknownRequest)?;
        if !matches!(record.state(), RequestState::Open) {
            return Err(EngineError::RequestNotOpen);
        }
        let claim = record.claim();
        let n_legs = record.n_legs();
        // Worst case: every live quote on every leg is notified.
        let worst_case = usize::from(n_legs)
            .checked_mul(usize::from(self.config.max_quotes_per_leg))
            .ok_or(EngineError::AmountOverflow)?;
        events.headroom(worst_case)?;

        // ── COMMIT ──
        // An explicit rejection releases every standing quote, so a requester cannot lock
        // maker capital and walk away — the grief costs them the full response deadline,
        // not a keystroke (§11).
        for index in 0..n_legs {
            self.release_leg(request, LegId(index), None, now, QuoteRejectReason::RequestRejected, events);
        }
        let mut commit = self.ledger.commit_phase();
        if let Some(claim) = claim {
            commit.release(claim);
        }
        if let Some(record) = commit.request_mut(request) {
            record.set_state(RequestState::Rejected);
        }
        Ok(())
    }

    // ────────────────────────────── SubmitQuote (§6) ──────────────────────────────

    #[allow(clippy::too_many_arguments)] // The command's own field list, plus `now`.
    fn submit_quote(
        &mut self,
        maker: AccountIdx,
        request: ReqIdx,
        leg_id: LegId,
        price: Price,
        size: Size,
        expires_at: Ts,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        // Every admission check of §6, and **no price-based rejection among them**. A quote
        // above the leg's limit is admitted, reserves capital normally, and simply loses at
        // selection. Rejecting on price would make the rejection a free oracle: a maker
        // bisects downward from UNIT, and every rejection costs nothing because a rejected
        // quote reserves nothing.
        let record = self.ledger.request(request).ok_or(EngineError::UnknownRequest)?;
        if !matches!(record.state(), RequestState::Open) {
            return Err(EngineError::RequestNotOpen);
        }
        if now >= record.deadline() {
            return Err(EngineError::RequestDeadlinePassed);
        }
        let leg = *record.leg(leg_id).ok_or(EngineError::UnknownLeg)?;
        if now >= expires_at {
            return Err(EngineError::QuoteExpiryInThePast);
        }
        if expires_at.saturating_sub(now) > self.config.max_quote_ttl {
            return Err(EngineError::QuoteTtlTooLong);
        }
        if price > UNIT {
            return Err(EngineError::PriceOutOfRange);
        }
        if size < leg.size() {
            return Err(EngineError::QuoteTooSmall);
        }

        // One live quote per maker per leg, and replacement may only improve (§6).
        let standing = standing_quote_of(&self.ledger, &leg, maker);
        if let Some(standing) = standing {
            let existing =
                self.ledger.quote(standing).ok_or(EngineError::UnknownQuote)?.price();
            // "At least as good for the requester" — lower is better on both sides, because
            // the price is always the price of the side being bought (§2.1).
            if price > existing {
                return Err(EngineError::WorseReplacement);
            }
        } else if leg.quote_count() >= self.config.max_quotes_per_leg {
            return Err(EngineError::LegQuoteLimitReached);
        }

        // Reservation is against the **fillable** amount, `leg.size`, not the quoted size: a
        // quote offering more than the leg needs is admissible, but only the leg's size can
        // ever fill, so reserving against the quoted size would over-lock capital for
        // nothing (§6).
        let contribution =
            leg.size().maker_contribution(price).ok_or(EngineError::AmountOverflow)?;

        // A replacement, then a new best: at most two events.
        events.headroom(2)?;

        // ── COMMIT ── (the replacement release, then the admission, then the publication)
        if let Some(standing) = standing {
            let claim = self.ledger.quote(standing).and_then(Quote::claim);
            let mut commit = self.ledger.commit_phase();
            if let Some(claim) = claim {
                commit.release(claim);
            }
            if let Some(quote) = commit.quote_mut(standing) {
                quote.set_state(QuoteState::Released);
            }
            events.push(Event::QuoteRejected {
                quote: standing,
                maker,
                reason: QuoteRejectReason::Replaced,
            });
            self.unlink_quote(request, leg_id, standing);
            self.ledger.commit_phase().close_quote(standing);
        }

        let arrival = self.next_arrival;
        self.next_arrival = self.next_arrival.saturating_add(1);
        let quote_record =
            Quote::new(maker, request, leg_id, price, size, expires_at, arrival);
        // The last fallible step; a failed insert mutates nothing, and a replacement that
        // gets this far has already released its predecessor, which is the transition §6
        // describes rather than a partial application.
        let (quote, _claim) = self.ledger.open_quote_reserving(quote_record, contribution)?;
        self.link_quote(request, leg_id, quote);

        // §7.1.1: published on quote arrival — the only point at which the engine emits it.
        self.publish_selection(request, leg_id, now, events);
        Ok(())
    }

    // ────────────────────────────── AcceptRequest (§7.2) ──────────────────────────────

    fn accept_request(
        &mut self,
        request: ReqIdx,
        expected: &[ExpectedFill; MAX_LEGS],
        n_legs: u8,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ═══ 1. PLAN — pure, no mutation. `plan_accept` takes `&self`, so the claim that
        //            nothing is touched is enforced by the signature. ═══
        let record = *self.ledger.request(request).ok_or(EngineError::UnknownRequest)?;
        if !matches!(record.state(), RequestState::Open) {
            return Err(EngineError::RequestNotOpen);
        }
        if now >= record.deadline() {
            return Err(EngineError::RequestDeadlinePassed);
        }
        if n_legs != record.n_legs() {
            return Err(EngineError::LegCountMismatch);
        }
        let (plan, requester_total) = self.plan_accept(&record, expected, now)?;

        // ═══ 2. CHECK — basket level. ═══
        let requester_claim = record.claim().ok_or(EngineError::ReservationShortfall)?;
        let reserved = self
            .ledger
            .reservation(requester_claim)
            .map(Reservation::amount)
            .ok_or(EngineError::ReservationShortfall)?;
        if requester_total > reserved {
            return Err(EngineError::ReservationShortfall);
        }
        // Worst case: every quote on every leg is notified, plus the intent. Statically
        // bounded because `n_legs <= MAX_LEGS` and each leg holds at most
        // `MAX_QUOTES_PER_LEG` — so `EventBufferFull` is reachable only by construction.
        let worst_case = usize::from(record.n_legs())
            .checked_mul(usize::from(self.config.max_quotes_per_leg))
            .and_then(|total| total.checked_add(1))
            .ok_or(EngineError::AmountOverflow)?;
        events.headroom(worst_case)?;
        // No slab check: the commit phase below only moves and frees claims. It inserts
        // nothing, so there is nothing to exhaust.
        let intent = self.build_intent(&record, &plan)?;

        // ── COMMIT ── infallible. No `?`, no fallible call, no allocation past this line:
        //              `commit_phase()` exposes nothing that can fail.
        for index in 0..record.n_legs() {
            let leg_id = LegId(index);
            let winner = plan.get(usize::from(index)).copied().flatten();
            self.release_leg(request, leg_id, winner, now, QuoteRejectReason::Outbid, events);

            // Move the winning maker's contribution reserved → committed. Explicit, because
            // nothing else in the system performs it and the global invariant counts
            // `committed` (§7.2).
            if let Some(winner) = winner {
                let claim = self.ledger.quote(winner.quote).and_then(Quote::claim);
                let amount = claim
                    .and_then(|claim| self.ledger.reservation(claim))
                    .map_or(Amount::ZERO, Reservation::amount);
                let mut commit = self.ledger.commit_phase();
                if let Some(quote) = commit.quote_mut(winner.quote) {
                    quote.set_state(QuoteState::Consumed);
                }
                if let Some(claim) = claim {
                    commit.commit(claim, request, amount);
                }
            }
        }

        // Move the requester's Σ contributions reserved → committed, and release the
        // over-reservation (limit − fill) in the same transition. Two steps in §7.2's text,
        // one transition here, because a claim that is neither reserved nor committed for an
        // instant is a bucket the money model does not have.
        let mut commit = self.ledger.commit_phase();
        commit.commit(requester_claim, request, requester_total);

        let nonce = Nonce::of(request);
        let deadline = now.saturating_add(self.config.max_settling_time);
        if let Some(record) = commit.request_mut(request) {
            record.set_state(RequestState::Settling { nonce, deadline });
        }

        // Settlement is never called here. Commit performs local, infallible bookkeeping and
        // emits an intent; the submission is a subsequent command produced by the publisher.
        // Escrow does not exist at the end of this.
        events.push(Event::SubmitIntent {
            request,
            nonce,
            requester: record.requester(),
            legs: intent,
            n_legs: record.n_legs(),
        });
        Ok(())
    }

    /// The PLAN phase: select a winner for every leg and total the requester's contribution.
    ///
    /// `&self`, so "pure, no mutation" is a property of the signature rather than a promise
    /// in a comment. Any leg with no eligible quote aborts the whole request before a single
    /// byte has moved — which is why multi-leg atomicity needs no distributed protocol:
    /// "provisionally matched" is a local variable in here, never a stored state and never a
    /// message to a counterparty (§7.2).
    fn plan_accept(
        &self,
        record: &Request,
        expected: &[ExpectedFill; MAX_LEGS],
        now: Ts,
    ) -> Result<([Option<Selection>; MAX_LEGS], Amount), EngineError> {
        let mut plan: [Option<Selection>; MAX_LEGS] = [None; MAX_LEGS];
        let mut requester_total = Amount::ZERO;

        for index in 0..usize::from(record.n_legs()) {
            let leg = record.legs().get(index).ok_or(EngineError::UnknownLeg)?;
            let leg_id = u8::try_from(index).unwrap_or(u8::MAX);
            let selection = best_on_leg(&self.ledger, leg, now)
                .map_err(|reason| EngineError::NoEligibleQuote { leg: leg_id, reason })?;

            // §7.1.1's binding: at or better than every expected price, per leg. A strictly
            // better fill is accepted silently; a worse one rejects the whole request. The
            // requester can never be worsened by latency, only by a stale accept that fails
            // safely.
            let view = expected.get(index).copied().ok_or(EngineError::LegCountMismatch)?;
            if view.leg != LegId(leg_id) {
                return Err(EngineError::LegCountMismatch);
            }
            if selection.price > view.price {
                return Err(EngineError::PresentationStale {
                    leg: leg_id,
                    expected: view.price,
                    actual: selection.price,
                });
            }

            let contribution = leg
                .size()
                .requester_contribution(selection.price)
                .ok_or(EngineError::AmountOverflow)?;
            requester_total =
                requester_total.checked_add(contribution).ok_or(EngineError::AmountOverflow)?;
            if let Some(slot) = plan.get_mut(index) {
                *slot = Some(selection);
            }
        }

        Ok((plan, requester_total))
    }

    /// Assemble the settlement bundle from the plan.
    ///
    /// The bundle is complete by construction: custody cannot reach back into the engine for
    /// anything it is missing (§13.1), so every value it validates against — the quote's
    /// expiry above all, which it rechecks against **its own clock** (§9.1) — travels with it.
    fn build_intent(
        &self,
        record: &Request,
        plan: &[Option<Selection>; MAX_LEGS],
    ) -> Result<[IntentLeg; MAX_LEGS], EngineError> {
        let mut intent: [IntentLeg; MAX_LEGS] = [IntentLeg::default(); MAX_LEGS];
        for index in 0..usize::from(record.n_legs()) {
            let (Some(Some(selection)), Some(leg)) = (plan.get(index), record.legs().get(index))
            else {
                return Err(EngineError::LegCountMismatch);
            };
            let winner =
                self.ledger.quote(selection.quote).ok_or(EngineError::UnknownQuote)?;
            if let Some(slot) = intent.get_mut(index) {
                *slot = IntentLeg {
                    contract: leg.contract(),
                    side: leg.side(),
                    size: leg.size(),
                    maker: winner.maker(),
                    fill_price: selection.price,
                    quote_expiry: winner.expires_at(),
                };
            }
        }
        Ok(intent)
    }

    // ────────────────────────────── PollSettlement (§8) ──────────────────────────────

    /// Act on what a poller observed about a settling request's nonce.
    ///
    /// Three outcomes, not two, and only two of them are answers:
    ///
    /// - `Settled` — the escrows exist. The committed capital has left the core's books.
    /// - `Reverted` — the trade did not happen. Every committed claim goes back to `free`.
    /// - `Unknown` or `Pending` — **no local answer exists, so nothing moves.** Past the
    ///   settling deadline this raises an operator alert and polling continues. Releasing
    ///   here is the duplication path: the maker requotes the same capital and the original
    ///   transaction lands (§8.1, §8.3).
    ///
    /// `Reverted` is trusted because it is the fate of the *nonce*, not of a submission. A
    /// retry bouncing off its own consumed nonce reverts while the nonce stays `Settled`, so
    /// that revert never reaches here (§8.1).
    fn poll_settlement(
        &mut self,
        reported: Nonce,
        status: TxStatus,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        let request = self.resolve_nonce(reported).ok_or(EngineError::StaleNonce)?;
        let record = *self.ledger.request(request).ok_or(EngineError::UnknownRequest)?;
        let RequestState::Settling { nonce, deadline } = record.state() else {
            return Err(EngineError::RequestNotSettling);
        };
        if nonce != reported {
            // The slot is live and the generation matched, but the request has since been
            // accepted again — impossible today, since accept happens once, and refused
            // rather than assumed.
            return Err(EngineError::StaleNonce);
        }

        match status {
            TxStatus::Unknown | TxStatus::Pending => {
                if now < deadline {
                    return Ok(());
                }
                events.headroom(1)?;
                // ── COMMIT ──
                events.push(Event::SettlementStalled { request, nonce, status });
                Ok(())
            }
            TxStatus::Settled => {
                events.headroom(1)?;
                // ── COMMIT ──
                let mut commit = self.ledger.commit_phase();
                commit.discharge_committed(request, |_, _| {});
                if let Some(record) = commit.request_mut(request) {
                    record.set_state(RequestState::Escrowed);
                }
                self.close_consumed_quotes(request);
                events.push(Event::RequestEscrowed { request, nonce });
                Ok(())
            }
            TxStatus::Reverted => {
                // One notice per released maker claim, plus the request-level event. Bounded
                // by `MAX_LEGS + 1` because a request has at most one winning quote per leg.
                let worst_case = usize::from(record.n_legs())
                    .checked_add(1)
                    .ok_or(EngineError::AmountOverflow)?;
                events.headroom(worst_case)?;

                // ── COMMIT ──
                let mut notices: [Option<(QuoteIdx, AccountIdx)>; MAX_LEGS] = [None; MAX_LEGS];
                let mut count = 0_usize;
                let quotes = &mut notices;
                self.ledger.commit_phase().discharge_committed(request, |owner, _| {
                    if let ResOwner::Quote(quote) = owner
                        && let Some(slot) = quotes.get_mut(count)
                    {
                        *slot = Some((quote, AccountIdx(0)));
                        count = count.saturating_add(1);
                    }
                });
                for entry in notices.iter_mut().flatten() {
                    if let Some(maker) = self.ledger.quote(entry.0).map(Quote::maker) {
                        entry.1 = maker;
                    }
                }
                for (quote, maker) in notices.into_iter().flatten() {
                    events.push(Event::QuoteRejected {
                        quote,
                        maker,
                        reason: QuoteRejectReason::SettlementFailed,
                    });
                }
                self.close_consumed_quotes(request);
                if let Some(record) = self.ledger.commit_phase().request_mut(request) {
                    record.set_state(RequestState::SettlementFailed);
                }
                events.push(Event::RequestSettlementFailed { request, nonce });
                Ok(())
            }
        }
    }

    /// The request a nonce names, if the slot is still occupied by the same generation.
    ///
    /// `(ReqIdx, req_generation)` is the nonce (§8.1). A request slot reused after its
    /// predecessor was freed yields a different generation, so a nonce from the previous
    /// occupant resolves to nothing rather than to its successor — which is the whole reason
    /// the generation is in the nonce.
    fn resolve_nonce(&self, nonce: Nonce) -> Option<ReqIdx> {
        let handle = self.ledger.request_handle_at(nonce.request)?;
        (handle.generation() == nonce.generation).then_some(handle)
    }

    /// Free the slots of every `Consumed` quote on a request whose claims have been
    /// discharged.
    ///
    /// A `Consumed` quote's slot was kept alive only because a committed claim named it as
    /// its owner and that owner must resolve (§15.3). Once the claim is gone the slot has no
    /// reader, and keeping it would grow occupancy by message history rather than by live
    /// capital.
    fn close_consumed_quotes(&mut self, request: ReqIdx) {
        let mut doomed: [Option<QuoteIdx>; MAX_LEGS] = [None; MAX_LEGS];
        let mut count = 0_usize;
        for (handle, quote) in self.ledger.quotes() {
            if quote.request() == request
                && matches!(quote.state(), QuoteState::Consumed)
                && quote.claim().is_none()
                && let Some(slot) = doomed.get_mut(count)
            {
                *slot = Some(handle);
                count = count.saturating_add(1);
            }
        }
        for handle in doomed.into_iter().flatten() {
            self.ledger.commit_phase().close_quote(handle);
        }
    }

    // ────────────────────────────── resolution (§10) ──────────────────────────────

    /// Record what the oracle says, if it is an admissible successor.
    fn report_oracle_status(
        &mut self,
        contract: ContractIdx,
        status: OracleStatus,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        let record = *self.ledger.contract(contract).ok_or(EngineError::UnknownContract)?;
        if !record.may_report(status) {
            return Err(EngineError::OracleStatusRegression);
        }
        let resolves = matches!(status, OracleStatus::Final(_));
        events.headroom(usize::from(resolves))?;

        // ── COMMIT ──
        // One field. No escrows are touched: one contract may back thousands of them, and
        // fanning out would be unbounded work in one critical section (§10.2).
        self.ledger.set_oracle_status(contract, status);
        if let OracleStatus::Final(outcome) = status {
            events.push(Event::ContractResolved { contract, outcome });
        }
        Ok(())
    }

    /// Ask custody to pay out one escrow, if its contract has an outcome.
    ///
    /// O(1) and idempotent: a second command emits a second intent, and re-settlement is a
    /// no-op at the custody layer, so replay is harmless (§9.2).
    fn settle_escrow(
        &mut self,
        escrow: EscrowId,
        contract: ContractIdx,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        // ── PLAN / CHECK ──
        let record = *self.ledger.contract(contract).ok_or(EngineError::UnknownContract)?;
        let outcome = record
            .outcome(now, self.config.stall_grace)
            .map_err(|_| EngineError::OutcomeNotYet)?;
        events.headroom(1)?;

        // ── COMMIT ──
        events.push(Event::SettleIntent { escrow, contract, outcome });
        Ok(())
    }

    // ────────────────────────────── shared commit-phase helpers ──────────────────────────

    /// Release every quote on `leg` except `keep`, notifying each maker.
    ///
    /// Infallible: only `commit_phase` operations and field writes. A quote that was already
    /// dead at `now` is reported as `QuoteExpired` rather than `QuoteRejected`, because the
    /// maker's capital was freed by time and not by losing — and this is the **only** path
    /// that emits `QuoteExpired`, since normalisation's count is unbounded (§4.3).
    fn release_leg(
        &mut self,
        request: ReqIdx,
        leg_id: LegId,
        keep: Option<Selection>,
        now: Ts,
        reason: QuoteRejectReason,
        events: &mut EventBuffer,
    ) {
        let Some(leg) = self.ledger.request(request).and_then(|record| record.leg(leg_id))
        else {
            debug_assert!(false, "releasing a leg that does not exist");
            return;
        };
        let leg = *leg;

        let mut doomed: [Option<QuoteIdx>; crate::config::MAX_QUOTES_PER_LEG] =
            [None; crate::config::MAX_QUOTES_PER_LEG];
        let mut count = 0_usize;
        walk_leg(&self.ledger, &leg, |handle| {
            if keep.is_some_and(|keep| keep.quote == handle) {
                return;
            }
            if let Some(slot) = doomed.get_mut(count) {
                *slot = Some(handle);
            }
            count = count.saturating_add(1);
        });
        debug_assert!(count <= crate::config::MAX_QUOTES_PER_LEG, "leg chain exceeded its bound");

        for handle in doomed.into_iter().flatten() {
            let Some(quote) = self.ledger.quote(handle) else { continue };
            let maker = quote.maker();
            let expired = !quote.is_live_at(now);
            let claim = quote.claim();

            let mut commit = self.ledger.commit_phase();
            if let Some(claim) = claim {
                commit.release(claim);
            }
            if let Some(quote) = commit.quote_mut(handle) {
                quote.set_state(QuoteState::Released);
            }
            events.push(if expired {
                Event::QuoteExpired { quote: handle, maker }
            } else {
                Event::QuoteRejected { quote: handle, maker, reason }
            });
            self.unlink_quote(request, leg_id, handle);
            self.ledger.commit_phase().close_quote(handle);
        }
    }

    /// Link a quote at the head of its leg's chain.
    fn link_quote(&mut self, request: ReqIdx, leg_id: LegId, quote: QuoteIdx) {
        let head = self
            .ledger
            .request(request)
            .and_then(|record| record.leg(leg_id))
            .map_or(crate::account::Link::NIL, |leg| leg.quotes_head);
        if let Some(record) = self.ledger.quote_mut(quote) {
            record.next_on_leg = head;
        }
        if let Some(leg) = self.ledger.request_mut(request).and_then(|r| r.leg_mut(leg_id)) {
            leg.quotes_head = crate::account::Link::to(quote.index());
            leg.quote_count = leg.quote_count.saturating_add(1);
        }
    }

    /// Unlink a quote from its leg's chain. O(k) over one leg, bounded by
    /// `MAX_QUOTES_PER_LEG`: the chain is singly linked because it is walked whole at
    /// selection and at commit anyway.
    fn unlink_quote(&mut self, request: ReqIdx, leg_id: LegId, quote: QuoteIdx) {
        let Some(leg) = self.ledger.request(request).and_then(|record| record.leg(leg_id))
        else {
            return;
        };
        let head = leg.quotes_head;
        let target = quote.index();

        if head.index() == Some(target) {
            let next = self.ledger.quote(quote).map_or(crate::account::Link::NIL, |q| q.next_on_leg);
            if let Some(leg) = self.ledger.request_mut(request).and_then(|r| r.leg_mut(leg_id)) {
                leg.quotes_head = next;
                leg.quote_count = leg.quote_count.saturating_sub(1);
            }
            return;
        }

        let mut cursor = head;
        while let Some(index) = cursor.index() {
            let Some(handle) = self.ledger.quote_handle_at(index) else { break };
            let Some(current) = self.ledger.quote(handle) else { break };
            let next = current.next_on_leg;
            if next.index() == Some(target) {
                let after =
                    self.ledger.quote(quote).map_or(crate::account::Link::NIL, |q| q.next_on_leg);
                if let Some(previous) = self.ledger.quote_mut(handle) {
                    previous.next_on_leg = after;
                }
                if let Some(leg) =
                    self.ledger.request_mut(request).and_then(|r| r.leg_mut(leg_id))
                {
                    leg.quote_count = leg.quote_count.saturating_sub(1);
                }
                return;
            }
            cursor = next;
        }
    }

    /// Recompute the best selection for a leg and publish it if it changed (§7.1.1).
    ///
    /// Called on quote arrival only. An ineligible quote is never selected, so this can
    /// never publish a price above the leg's limit.
    fn publish_selection(
        &mut self,
        request: ReqIdx,
        leg_id: LegId,
        now: Ts,
        events: &mut EventBuffer,
    ) {
        let Some(leg) = self.ledger.request(request).and_then(|record| record.leg(leg_id))
        else {
            return;
        };
        let leg = *leg;
        let Ok(best) = best_on_leg(&self.ledger, &leg, now) else { return };
        if leg.published() == Some(best.price) {
            return;
        }
        if let Some(leg) = self.ledger.request_mut(request).and_then(|r| r.leg_mut(leg_id)) {
            leg.published = Some(best.price);
        }
        events.push(Event::BestSelectionChanged { request, leg: leg_id, price: best.price });
    }
}
