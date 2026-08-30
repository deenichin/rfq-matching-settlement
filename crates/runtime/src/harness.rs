//! The harness (SPEC §13.1, CLAUDE 8c).
//!
//! Two systems that cannot see each other still need something that can see both. The
//! harness owns one engine and one custody instance, advances **both clocks independently**,
//! and drives the wires between them: engine events to the settlement adapter, and custody
//! balance changes back to the engine's mirror.
//!
//! It is the only place global conservation and claim coverage can be asserted (§2.2),
//! because those assertions span both halves and no function inside either half can compute
//! them. That is why §15's table says "the harness" and not "core".
//!
//! Two constraints, and they are why this type exists rather than a pair of `pub` fields
//! somewhere convenient:
//!
//! - **It is not a back door.** It may read both systems; no engine code path may. If a
//!   production path ever needs something only the harness can see, that is a design error.
//! - **It has no production counterpart.** In v2 its wiring is replaced by real transport
//!   and its cross-system assertions become the reconciler (§12) — a monitoring component
//!   that can *report* divergence, not an oracle of truth that prevents it.

use rfq_chain::bundle::Bundle;
use rfq_chain::custody::{
    Custody, CustodyError, IncludedTx, SettleError, SettleReceipt, SubmitAck,
};
use rfq_chain::escrow::Escrow;
use rfq_chain::indexer::Indexer;
use rfq_chain::log::ChainPayload;
use rfq_chain::oracle::Oracle;
use rfq_core::account::{AccountIdx, MirroredBalance};
use rfq_core::clock::{Clock, SettableClock};
use rfq_core::command::Command;
use rfq_core::config::{Config, ConfigError};
use rfq_core::engine::{Engine, EngineError};
use rfq_core::escrow::EscrowId;
use rfq_core::event::{Event, EventBuffer};
use rfq_core::request::ReqIdx;
use rfq_core::contract::{ContractIdx, OracleStatus, Outcome};
use rfq_core::settlement::TxStatus;
use rfq_core::types::{Amount, Ts};

use crate::settlement;

/// What a cross-system invariant looked like when it broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossSystemViolation {
    /// §15.5 — `Σ custody balances + Σ notional over Locked escrows != deposited − withdrawn`.
    ///
    /// A sum that fell means money was lost; one that rose means it was duplicated.
    ConservationBroken,
    /// §15.6 — `custody.balance(a) < core.reserved(a) + core.committed(a)`.
    ///
    /// The engine has promised capital custody does not hold, which is the failure the §9.3
    /// timelock exists to prevent and the reason a maker cannot quote-and-run.
    ClaimCoverageBroken(AccountIdx),
    /// §15.7 — the engine's mirror disagrees with custody. Exact in v1; in v2 this becomes a
    /// bounded-drift assertion whose bound is the §9.3 lag terms.
    MirrorDisagrees(AccountIdx),
    /// §15.8 — an escrow's two contributions do not sum to its notional.
    EscrowContributionsWrong(EscrowId),
}

/// One engine, one custody, two clocks, and the wires between them.
pub struct Harness<EC: Clock, CC: Clock> {
    engine: Engine,
    /// The **venue's** clock. Held here rather than inside the engine because
    /// `apply(cmd, now)` samples time once at the call site (CLAUDE 2).
    engine_clock: EC,
    /// Custody holds the **chain's** clock itself, and neither system can read the other's.
    custody: Custody<CC>,
    events: EventBuffer,
    /// Bundles the adapter has pulled off the event stream and not yet submitted.
    pending: Vec<Bundle>,
    /// Escrows formed so far, in the order settlement produced them.
    escrows: Vec<EscrowId>,
    /// The oracle. In `chain`, like custody, and reaching the engine only as a status
    /// carried in a command (§10.1, §13.1).
    oracle: Oracle<CC>,
    /// Settlement intents the adapter has picked up and not yet applied.
    payouts: Vec<Event>,
    /// The indexer: cursor, confirmation depth, dedup (§12). The **only** path from custody
    /// back to the engine, and a real one — not a synchronous copy of a balance.
    indexer: Indexer,
    /// Whether the indexer is being left un-pumped, which is what a lagging one looks like.
    indexer_stalled: bool,
    /// Bundles that have been sent and whose nonce has no terminal answer yet.
    ///
    /// The submitter keeps them, which is what makes a retry possible at all: an
    /// acknowledgement can be lost, and the only way to ask again is to still have the thing
    /// you sent (§8.1).
    in_flight: Vec<Bundle>,
    /// Every event the engine has emitted, in order.
    emitted: Vec<Event>,
}

impl<EC: Clock, CC: Clock + core::fmt::Debug> core::fmt::Debug for Harness<EC, CC> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Harness").field("custody", &self.custody).finish_non_exhaustive()
    }
}

impl<EC: Clock, CC: Clock> Harness<EC, CC> {
    /// Validate the configuration and construct both systems.
    ///
    /// Both startup assertions are checked here, and this is the natural place for the
    /// four-term timelock inequality: it relates the engine's `MAX_QUOTE_TTL` to custody's
    /// `WITHDRAWAL_DELAY`, so it is a statement about the pair. Neither system alone can
    /// check it, for the same reason neither alone can check conservation.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`].
    pub fn new(
        config: Config,
        engine_clock: EC,
        custody_clock: CC,
        oracle_clock: CC,
    ) -> Result<Self, ConfigError> {
        let engine = Engine::new(config)?;
        let custody = Custody::new(
            custody_clock,
            config.withdrawal_delay,
            config.max_accounts,
            config.max_escrows,
        );
        // The oracle keeps a clock of its own too. It is a third external system, and giving
        // it the venue's clock would be assuming the very agreement §9.1 says not to assume.
        let oracle =
            Oracle::new(oracle_clock, config.challenge_window, config.escalation_authority);
        Ok(Self {
            engine,
            engine_clock,
            custody,
            events: EventBuffer::with_capacity(64),
            pending: Vec::new(),
            payouts: Vec::new(),
            escrows: Vec::new(),
            indexer: Indexer::new(u64::from(config.confirmations)),
            indexer_stalled: false,
            in_flight: Vec::new(),
            emitted: Vec::new(),
            oracle,
        })
    }

    /// Venue time, sampled once — this is the `now` passed to `apply`.
    pub fn engine_now(&self) -> Ts {
        self.engine_clock.now()
    }

    /// Chain time. Deliberately a separate reading; the two may disagree.
    pub fn custody_now(&self) -> Ts {
        self.custody.now()
    }

    /// The venue's clock, for a test to advance.
    pub const fn engine_clock_mut(&mut self) -> &mut EC {
        &mut self.engine_clock
    }

    /// The chain's clock, for a test to advance — independently, and by a different amount.
    pub const fn custody_clock_mut(&mut self) -> &mut CC {
        self.custody.clock_mut()
    }

    /// Move both clocks to the same instant.
    ///
    /// v1's normal case: custody is in-process and the two clocks happen to agree (§9.1).
    /// Divergence is a thing a test *injects*, not a thing that drifts in — and a test that
    /// moves one clock ten minutes without the other is not modelling lag, it is modelling a
    /// venue whose halves disagree about the hour.
    pub fn set_both_clocks(&mut self, now: Ts)
    where
        EC: SettableClock,
        CC: SettableClock,
    {
        self.engine_clock.set_now(now);
        self.custody.clock_mut().set_now(now);
    }

    /// The engine, read-only.
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Custody, read-only.
    pub const fn custody(&self) -> &Custody<CC> {
        &self.custody
    }

    /// Custody, mutably — for a test to install `on_settle_entry` or move money.
    pub const fn custody_mut(&mut self) -> &mut Custody<CC> {
        &mut self.custody
    }

    /// Escrows formed so far, in settlement order.
    #[must_use]
    pub fn escrows(&self) -> &[EscrowId] {
        &self.escrows
    }

    /// Bundles waiting to be submitted.
    #[must_use]
    pub fn pending_bundles(&self) -> &[Bundle] {
        &self.pending
    }

    /// Put money into custody and tell the engine about it.
    ///
    /// Two steps, in the only order that is honest: custody is credited first, because it is
    /// authoritative, and the engine learns through a command — the same path a chain event
    /// takes through the indexer. The engine never computes a balance.
    ///
    /// # Errors
    ///
    /// [`CustodyError`] from custody, or [`EngineError`] from the mirror update.
    pub fn deposit(
        &mut self,
        account: AccountIdx,
        amount: Amount,
    ) -> Result<(), HarnessError> {
        self.custody.deposit(account, amount).map_err(HarnessError::Custody)?;
        self.pump_indexer();
        Ok(())
    }

    /// Ask custody to release funds, and tell the engine that availability has dropped.
    ///
    /// The mirror moves immediately even though the balance does not: admission must stop
    /// lending against this money now, and settlement must keep finding it until the
    /// timelock expires (§9.1, §9.3).
    ///
    /// # Errors
    ///
    /// [`CustodyError`], or [`EngineError`] from the mirror update.
    pub fn request_withdrawal(
        &mut self,
        account: AccountIdx,
        amount: Amount,
    ) -> Result<Ts, HarnessError> {
        let matures_at =
            self.custody.request_withdrawal(account, amount).map_err(HarnessError::Custody)?;
        self.pump_indexer();
        self.assert_cross_system_invariants();
        Ok(matures_at)
    }

    /// Execute a matured withdrawal and tell the engine.
    ///
    /// # Errors
    ///
    /// [`CustodyError`], or [`EngineError`] from the mirror update.
    pub fn execute_withdrawal(&mut self, account: AccountIdx) -> Result<Amount, HarnessError> {
        let amount =
            self.custody.execute_withdrawal(account).map_err(HarnessError::Custody)?;
        self.pump_indexer();
        // The narrower set: a withdrawal can legitimately land on capital the engine has
        // already committed to a basket in flight, and the exit is that basket's settlement
        // failing (§2.4). Conservation, escrow contributions and mirror agreement hold
        // regardless, and the caller checks coverage where it is entitled to.
        self.assert_settlement_invariants();
        Ok(amount)
    }

    /// Apply one command to the engine, pump the wires, and assert every cross-system
    /// invariant.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Engine`] if the engine refused. The wires are still pumped and the
    /// invariants still asserted, because a rejection normalises and a rejected command must
    /// leave the two systems as consistent as an accepted one.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke. That is a money-state error, and continuing past
    /// one is how a venue loses track of who owns what.
    pub fn apply(&mut self, command: Command) -> Result<(), HarnessError> {
        // Sampled once, here, at the call site (CLAUDE 2).
        let now = self.engine_clock.now();
        self.events.clear();
        let outcome = self.engine.apply(command, now, &mut self.events);

        // The engine → custody wire: the adapter picks `SubmitIntent` off the stream and
        // turns it into a bundle. Nothing else crosses.
        let emitted: Vec<Event> = self.events.drain().collect();
        for event in &emitted {
            if let Some(bundle) = settlement::bundle_from(event) {
                self.pending.push(bundle);
            }
            if matches!(event, Event::SettleIntent { .. }) {
                self.payouts.push(*event);
            }
        }
        self.emitted.extend(emitted);

        self.assert_cross_system_invariants();
        outcome.map_err(HarnessError::Engine)
    }

    /// Hand every bundle the adapter has picked up to custody, in order.
    ///
    /// **Sent, and nothing more.** Nothing is included until the test says so, because the
    /// interval between the two is the one §8.1 is about.
    pub fn submit_pending(&mut self) -> Vec<SubmitAck> {
        let bundles: Vec<Bundle> = self.pending.drain(..).collect();
        bundles
            .into_iter()
            .map(|bundle| {
                self.in_flight.push(bundle);
                self.custody.submit(bundle)
            })
            .collect()
    }

    /// Send a settling request's bundle again.
    ///
    /// Not a test affordance: this is what a settlement adapter does when an acknowledgement
    /// is lost, and §8.1's whole answer depends on it existing. Models the retry after a lost
    /// acknowledgement. The bundle is byte-identical and so is
    /// its nonce, which is the whole point: an idempotent nonce turns "unknown" from a
    /// catastrophe into a delay. Nothing is re-derived from engine state — the submitter
    /// resends what it sent.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Engine`] if the request is not settling, or if no bundle carrying its
    /// nonce was ever sent — a submitter that has lost what it sent cannot retry, which is
    /// the one thing §8.1's answer depends on.
    pub fn resubmit(&mut self, request: ReqIdx) -> Result<SubmitAck, HarnessError> {
        // Found among what the submitter kept, not read out of engine state. A submitter
        // that lost its acknowledgement does not know what the engine believes — that is the
        // whole situation — so it resends the bundle it holds.
        let bundle = *self
            .in_flight
            .iter()
            .find(|bundle| bundle.nonce.request == request.index())
            .ok_or(HarnessError::Engine(EngineError::RequestNotSettling))?;
        Ok(self.custody.submit(bundle))
    }

    /// The oracle, read-only.
    pub const fn oracle(&self) -> &Oracle<CC> {
        &self.oracle
    }

    /// The oracle, mutably — for a test to propose, contest, finalise or escalate.
    pub const fn oracle_mut(&mut self) -> &mut Oracle<CC> {
        &mut self.oracle
    }

    /// Read what the oracle says about a contract and hand it to the engine as a command.
    ///
    /// The oracle adapter, in one line: the engine has no oracle dependency and never polls,
    /// so a status is read on one side and delivered as a command on the other (§10.1).
    ///
    /// # Errors
    ///
    /// [`HarnessError::Engine`] if the engine refused — a regression, most usefully.
    pub fn report_oracle_status(
        &mut self,
        contract: ContractIdx,
    ) -> Result<OracleStatus, HarnessError> {
        let status = self.oracle.status(contract);
        self.custody
            .log_mut()
            .append(ChainPayload::OracleStatusReported { contract, status });
        self.pump_indexer();
        Ok(status)
    }

    /// Someone sends a transaction asking for an escrow to be paid out.
    ///
    /// `SettleEscrow` may be sent by anyone (§10.1), and on a chain that arrives as a log
    /// entry — so it enters the engine the way every other outside fact does, through the
    /// indexer, subject to the same confirmation depth and dedup.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke.
    pub fn request_escrow_settlement(
        &mut self,
        escrow: EscrowId,
        contract: ContractIdx,
        outcome: Outcome,
    ) {
        self.custody
            .log_mut()
            .append(ChainPayload::EscrowSettled { escrow, contract, outcome });
        self.pump_indexer();
    }

    /// Apply every settlement intent the adapter has picked up.
    ///
    /// The second engine-to-custody wire. Returns each escrow's outcome: `true` if it paid,
    /// `false` if it had already been settled.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke.
    pub fn apply_payouts(&mut self) -> Vec<Result<bool, CustodyError>> {
        let intents: Vec<Event> = self.payouts.drain(..).collect();
        let mut outcomes = Vec::with_capacity(intents.len());
        for intent in intents {
            let Event::SettleIntent { escrow, contract, outcome } = intent else { continue };
            outcomes.push(self.custody.ledger_mut().settle_escrow(escrow, contract, outcome));
        }
        self.mirror_all();
        self.assert_settlement_invariants();
        outcomes
    }

    /// Settlement intents waiting to be applied.
    #[must_use]
    pub fn pending_payouts(&self) -> &[Event] {
        &self.payouts
    }

    /// Everything the engine has emitted, in order.
    #[must_use]
    pub fn emitted(&self) -> &[Event] {
        &self.emitted
    }

    /// Throw away the oldest pending submission without including it.
    ///
    /// The send that never arrived. The nonce stays `Unknown`, which is not an answer.
    pub fn lose_next_submission(&mut self) -> bool {
        self.custody.drop_next_submission()
    }

    /// Include everything the chain has been handed, oldest first.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke.
    pub fn include_all(&mut self) -> Vec<IncludedTx> {
        let included = self.custody.include_all();
        for tx in &included {
            if let Ok(receipt) = &tx.outcome {
                for id in receipt.escrows.iter().take(usize::from(receipt.n_escrows)) {
                    self.escrows.push(*id);
                }
            }
        }
        self.mirror_all();
        self.assert_settlement_invariants();
        included
    }

    /// Submit and include in one step, for a test that is not about the interval.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke.
    pub fn settle_pending(&mut self) -> Vec<Result<SettleReceipt, SettleError>> {
        self.submit_pending();
        self.include_all().into_iter().map(|tx| tx.outcome).collect()
    }

    /// Ask about a nonce the way a poller would, and hand the answer to the engine.
    ///
    /// **This cannot move a request out of `Settling`.** A terminal answer arrives through
    /// the log, like every other fact the engine learns; what a poller can observe on its own
    /// is `Unknown` or `Pending`, and neither moves anything. That split is deliberate: the
    /// only path that changes engine state is the one with confirmation depth and dedup on
    /// it, and a direct read can at most raise a stall alert.
    ///
    /// A real poller's read is an RPC, and an RPC is a synchronous call — so modelling it as
    /// one is faithful rather than a shortcut. What matters is that nothing it returns is
    /// trusted to end a settlement.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Engine`] if the engine refused the poll.
    pub fn poll_settlement(&mut self, request: ReqIdx) -> Result<TxStatus, HarnessError> {
        let Some(nonce) =
            self.engine.ledger().request(request).and_then(|record| record.state().nonce())
        else {
            return Err(HarnessError::Engine(EngineError::RequestNotSettling));
        };
        let status = self.custody.status(nonce);
        if status.is_terminal() {
            // Observed, and deliberately not delivered. A terminal answer is the chain's to
            // announce, and it announces it in the log — where confirmation depth and dedup
            // apply. Letting a direct read end a settlement would put the one state change
            // that releases capital on the one path with neither.
            return Ok(status);
        }
        self.apply(Command::PollSettlement { nonce, status })?;
        Ok(status)
    }

    /// Bring the engine's mirror of one account back in line with custody.
    ///
    /// This is the custody → engine wire in its simplest form: a balance change becomes a
    /// `CreditAccount` command. S6 puts a real indexer in the middle — cursor, confirmation
    /// depth, dedup — and that is where the mirror stops being exact and starts being
    /// lagged (§2.3, §12).
    /// Read every confirmed, undelivered log entry and apply the commands it yields.
    ///
    /// This is the custody → engine wire, and it is the real one: cursor, confirmation depth
    /// and dedup, not a synchronous copy of a balance. Everything the engine learns about the
    /// outside world arrives this way.
    ///
    /// Returns how many commands were delivered.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke once the engine has caught up.
    pub fn pump_indexer(&mut self) -> usize {
        if self.indexer_stalled {
            return 0;
        }
        let commands = self.indexer.drain(self.custody.log());
        let delivered = commands.len();
        for command in commands {
            let now = self.engine_clock.now();
            self.events.clear();
            // A command from the indexer can legitimately be refused — an `EscrowSettled`
            // arriving before the resolution it depends on is exactly that — and a refusal
            // is the safety property working, not a wire failure.
            let _ = self.engine.apply(command, now, &mut self.events);
            let emitted: Vec<Event> = self.events.drain().collect();
            for event in &emitted {
                if let Some(bundle) = settlement::bundle_from(event) {
                    self.pending.push(bundle);
                }
                if matches!(event, Event::SettleIntent { .. }) {
                    self.payouts.push(*event);
                }
            }
            self.emitted.extend(emitted);
        }
        self.assert_settlement_invariants();
        delivered
    }

    /// Mine a block, then deliver whatever that made deep enough.
    pub fn advance_block(&mut self) -> usize {
        self.custody.log_mut().advance_block();
        self.pump_indexer()
    }

    /// The indexer, read-only.
    pub const fn indexer(&self) -> &Indexer {
        &self.indexer
    }

    /// The indexer, for a test to rewind its cursor or forget what it has delivered.
    pub const fn indexer_mut(&mut self) -> &mut Indexer {
        &mut self.indexer
    }

    fn mirror_all(&mut self) {
        self.pump_indexer();
    }

    /// Stop pumping the indexer.
    ///
    /// A lagging indexer, modelled as what one actually is: the log keeps growing and
    /// nothing reads it. Staleness is the *only* thing that lets the engine admit against
    /// capital already gone — with the §9.3 inequality holding and every lag term at zero,
    /// no participant can withdraw out from under their own live claim, so an
    /// insufficient-funds settlement is unreachable by construction.
    ///
    /// # Panics
    ///
    /// If the configuration claims zero indexer lag. A harness that exhibits lag a venue
    /// says it does not have is testing a different venue.
    pub fn stall_indexer(&mut self) {
        assert!(
            self.engine.config().max_indexer_lag > rfq_core::types::Dur::ZERO,
            "a venue configured for zero indexer lag must not be made to exhibit any"
        );
        self.indexer_stalled = true;
    }

    /// Resume pumping and catch the engine up.
    ///
    /// # Panics
    ///
    /// If a cross-system invariant broke once the engine has caught up.
    pub fn resume_indexer(&mut self) {
        self.indexer_stalled = false;
        self.pump_indexer();
        self.assert_settlement_invariants();
    }

    /// Every cross-system invariant, run after every command in tests and scenarios
    /// (SPEC §15, invariants 5–8).
    ///
    /// # Panics
    ///
    /// On the first violation, naming it.
    pub fn assert_cross_system_invariants(&self) {
        if let Err(violation) = self.check_cross_system_invariants() {
            panic!("cross-system invariant violated (SPEC §15): {violation:?}");
        }
    }

    /// The invariants that hold at **every** instant, including inside a settlement whose
    /// outcome the engine has not yet been told.
    ///
    /// Claim coverage is not among them, and the omission is specific rather than
    /// convenient. Coverage says the engine has not promised capital custody does not hold.
    /// Between custody reverting a settlement and the engine learning it reverted, the
    /// engine still shows that basket's capital as `committed` while custody has paid some
    /// of it out to a withdrawal — a true violation with a defined exit, and the exit is
    /// `PollSettlement` releasing the committed claims back to `free` (§2.4). Asserting
    /// coverage there would be asserting the absence of a window the design describes.
    ///
    /// # Panics
    ///
    /// On the first violation, naming it.
    pub fn assert_settlement_invariants(&self) {
        let outcome = self
            .check_conservation()
            .and_then(|()| self.check_escrow_contributions())
            .and_then(|()| self.check_mirror_agreement());
        if let Err(violation) = outcome {
            panic!("cross-system invariant violated (SPEC §15): {violation:?}");
        }
    }

    /// The same checks, returned rather than panicked, so a test can name the violation.
    ///
    /// # Errors
    ///
    /// The first [`CrossSystemViolation`] found.
    pub fn check_cross_system_invariants(&self) -> Result<(), CrossSystemViolation> {
        self.check_conservation()?;
        self.check_escrow_contributions()?;
        self.check_claim_coverage()?;
        self.check_mirror_agreement()
    }

    /// §15.5 alone.
    ///
    /// Read-only, and separable because conservation is the one invariant that holds through
    /// every window the others leave open: it is purely custody-side, so nothing the engine
    /// has or has not yet learned can affect it.
    ///
    /// # Errors
    ///
    /// [`CrossSystemViolation::ConservationBroken`].
    pub fn check_conservation_only(&self) -> Result<(), CrossSystemViolation> {
        self.check_conservation()
    }

    /// §15.5 — `Σ custody balances + Σ notional over Locked escrows == deposited − withdrawn`.
    ///
    /// Purely custody-side. `reserved` and `committed` are claims *against* `free`, not
    /// partitions of it, and adding them here would double-count. **Only `Locked` escrows
    /// are counted**: a `Settled` escrow has already paid out and its notional is back in
    /// someone's balance, so summing every escrow would make the first payout read as newly
    /// created money and the invariant would fail on a correct system (CLAUDE 41).
    fn check_conservation(&self) -> Result<(), CrossSystemViolation> {
        let ledger = self.custody.ledger();
        let mut held = Amount::ZERO;
        for index in 0..ledger.account_count() {
            held = held
                .checked_add(ledger.balance(AccountIdx(index)))
                .ok_or(CrossSystemViolation::ConservationBroken)?;
        }
        for (_, escrow) in ledger.escrows() {
            if escrow.is_locked() {
                held = held
                    .checked_add(escrow.notional())
                    .ok_or(CrossSystemViolation::ConservationBroken)?;
            }
        }
        let expected = ledger
            .deposited()
            .checked_sub(ledger.withdrawn())
            .ok_or(CrossSystemViolation::ConservationBroken)?;
        if held != expected {
            return Err(CrossSystemViolation::ConservationBroken);
        }
        Ok(())
    }

    /// §15.8 — each escrow's two contributions sum to its notional, and are stored
    /// separately. Separately matters because the void path returns each side its own
    /// contribution (§10.3).
    fn check_escrow_contributions(&self) -> Result<(), CrossSystemViolation> {
        for (id, escrow) in self.custody.ledger().escrows() {
            let sum = escrow
                .requester_contribution()
                .checked_add(escrow.maker_contribution())
                .ok_or(CrossSystemViolation::EscrowContributionsWrong(id))?;
            let notional = escrow
                .size()
                .notional()
                .ok_or(CrossSystemViolation::EscrowContributionsWrong(id))?;
            if sum != notional {
                return Err(CrossSystemViolation::EscrowContributionsWrong(id));
            }
        }
        Ok(())
    }

    /// §15.6 — every core-side claim is backed by capital custody holds **for that account**:
    ///
    /// ```text
    /// ∀ a:  custody.balance(a) + Σ a's contributions in Locked escrows
    ///           ≥  core.reserved(a) + core.committed(a)
    /// ```
    ///
    /// §2.2 writes this as `custody.free(a) ≥ reserved(a) + committed(a)`, which is the same
    /// statement before any settlement confirms — escrowed contributions are zero then, and
    /// the two forms coincide exactly where §2.2 states it.
    ///
    /// The escrow term is not a weakening. Once a bundle settles, custody has moved the
    /// requester's and each maker's contribution out of their balances and into escrow, while
    /// the engine still shows that capital as `committed` until it learns the settlement
    /// confirmed. The claim is backed throughout — by escrowed money rather than free money —
    /// and the narrower form would fail on a correct system during that window, which is
    /// precisely the situation CLAUDE 41 says not to create: an invariant that fails on a
    /// correct system invites being weakened, and weakening it later to accommodate the
    /// window is worse than stating it correctly now.
    ///
    /// Against **balance**, not availability. A maker may request a withdrawal covering
    /// capital the engine has claimed — custody has no concept of a reservation (§1), and
    /// the §9.3 timelock is what makes that safe. Asserting against availability would fail
    /// the instant anyone requested a withdrawal, again on a correct system.
    fn check_claim_coverage(&self) -> Result<(), CrossSystemViolation> {
        let ledger = self.custody.ledger();
        for index in 0..ledger.account_count() {
            let account = AccountIdx(index);
            let Some(entry) = self.engine.ledger().account(account) else { continue };
            let claimed = entry
                .reserved()
                .checked_add(entry.committed())
                .ok_or(CrossSystemViolation::ClaimCoverageBroken(account))?;
            let mut backing = ledger.balance(account);
            for (_, escrow) in ledger.escrows() {
                if !escrow.is_locked() {
                    continue;
                }
                if escrow.requester() == account {
                    backing = backing
                        .checked_add(escrow.requester_contribution())
                        .ok_or(CrossSystemViolation::ClaimCoverageBroken(account))?;
                }
                if escrow.maker() == account {
                    backing = backing
                        .checked_add(escrow.maker_contribution())
                        .ok_or(CrossSystemViolation::ClaimCoverageBroken(account))?;
                }
            }
            if claimed > backing {
                return Err(CrossSystemViolation::ClaimCoverageBroken(account));
            }
        }
        Ok(())
    }

    /// §15.7 — the engine's mirror against custody.
    ///
    /// The mirror projects **availability**, not balance. Admission is forward-looking and
    /// must never lend against money already on its way out (§9.1), so the number the engine
    /// admits against is `balance − pending withdrawals`. Settlement asks the other question
    /// and reads the balance directly.
    ///
    /// Two forms, and which one applies is a property of the configuration:
    ///
    /// - **No declared lag** — zero confirmations, zero indexer lag — the mirror is exact.
    ///   Anything else is a bug in the wire.
    /// - **Declared lag** — the mirror is a *prefix* of what the chain has published, so
    ///   equality is false on a correct system and asserting it would be asserting the
    ///   absence of the lag the configuration declares. What is asserted instead is that the
    ///   engine has never invented a number: every mirrored value must be one the chain
    ///   actually published for that account, or zero. A phantom credit is precisely a value
    ///   that appears in the mirror and nowhere in the log.
    ///
    /// The time-domain bound §15.7 describes for v2 — drift no wider than the §9.3 lag
    /// terms — is not asserted here, because this harness does not model delivery latency in
    /// time. It models delivery *depth*, and the depth form is the one above.
    fn check_mirror_agreement(&self) -> Result<(), CrossSystemViolation> {
        let ledger = self.custody.ledger();
        let config = self.engine.config();
        let lagless = config.confirmations == 0
            && config.max_indexer_lag == rfq_core::types::Dur::ZERO
            && !self.indexer_stalled;

        for index in 0..ledger.account_count() {
            let account = AccountIdx(index);
            let Some(entry) = self.engine.ledger().account(account) else { continue };
            let mirrored = MirroredBalance::free(entry);

            if lagless {
                if mirrored != ledger.available(account) {
                    return Err(CrossSystemViolation::MirrorDisagrees(account));
                }
                continue;
            }

            // Under lag: the mirror must be something the chain said, not something the
            // engine made up.
            if mirrored == Amount::ZERO {
                continue;
            }
            let published = self.custody.log().entries().iter().any(|event| {
                matches!(
                    event.payload,
                    ChainPayload::BalanceChanged { account: logged, available }
                        if logged == account && available == mirrored
                )
            });
            if !published {
                return Err(CrossSystemViolation::MirrorDisagrees(account));
            }
        }
        Ok(())
    }

    /// Every escrow custody holds, for a scenario trace.
    pub fn locked_escrows(&self) -> impl Iterator<Item = (EscrowId, &Escrow)> {
        self.custody.ledger().escrows().filter(|(_, escrow)| escrow.is_locked())
    }
}

/// A harness operation that one of the two systems refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessError {
    /// The engine refused a command.
    Engine(EngineError),
    /// Custody refused a balance operation.
    Custody(CustodyError),
}
