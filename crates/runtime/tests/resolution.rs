//! S5 gate: PLAN (a)–(g), the resolution boundary.
//!
//! The engine models **no** proposal, dispute, bonding or voting. Its contract state is two
//! values and its interface to the oracle is one status type and one command; the propose /
//! window / contest / escalate machinery lives mocked in `chain`, where it belongs. Importing
//! it would couple this state machine to a system the venue does not control and cannot fix.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_chain::custody::CustodyError;
use rfq_core::account::AccountIdx;
use rfq_core::clock::TestClock;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::{ContractIdx, ContractState, OracleStatus, Outcome};
use rfq_core::engine::EngineError;
use rfq_core::escrow::EscrowId;
use rfq_core::event::Event;
use rfq_core::request::ReqIdx;
use rfq_core::types::{Amount, Dur, LegId, Price, Side, Size, Ts};
use rfq_runtime::harness::{Harness, HarnessError};

const REQUESTER: AccountIdx = AccountIdx(0);
const ALPHA: AccountIdx = AccountIdx(1);
const BETA: AccountIdx = AccountIdx(2);
/// The design's one trusted component: a single unbonded key (§10.4).
const ESCALATION: AccountIdx = AccountIdx(3);

const SEPTEMBER: ContractIdx = ContractIdx(0);
const OCTOBER: ContractIdx = ContractIdx(1);
const EVENT_DATE: Ts = Ts(50_000_000);
const DEADLINE: Ts = Ts(100_000);
const SIZE: Size = Size(100);

const LIMIT: Price = Price(650_000);
const FILL_A: Price = Price(610_000);
const FILL_B: Price = Price(450_000);
const STALL_GRACE: Dur = Dur(86_400_000);
const CHALLENGE_WINDOW: Dur = Dur(7_200_000);

fn config() -> Config {
    Config {
        max_accounts: 4,
        max_reservations: 32,
        max_requests: 4,
        max_quotes: 32,
        max_contracts: 4,
        max_escrows: 16,
        max_legs: 4,
        max_quotes_per_leg: 4,
        max_quote_ttl: Dur(30_000),
        stall_grace: STALL_GRACE,
        challenge_window: CHALLENGE_WINDOW,
        escalation_authority: ESCALATION,
        ..Config::default()
    }
}

type TestHarness = Harness<TestClock, TestClock>;

fn maker_contribution(price: Price) -> Amount {
    SIZE.maker_contribution(price).unwrap()
}
fn requester_contribution(price: Price) -> Amount {
    SIZE.requester_contribution(price).unwrap()
}
fn notional() -> Amount {
    SIZE.notional().unwrap()
}

/// **The calendar spread**: leg 0 is `Yes` on September, leg 1 is `No` on October. Mixed
/// sides, so one settlement exercises both directions of the payout mapping (§10.3).
///
/// Returns the harness, the request, and the two escrows in leg order. Everyone is funded to
/// exactly their contribution (CLAUDE 38).
fn escrowed_spread() -> (TestHarness, ReqIdx, [EscrowId; 2]) {
    let mut harness = Harness::new(
        config(),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
    )
    .unwrap();
    let reservation = Amount(requester_contribution(LIMIT).0 * 2);
    harness.deposit(REQUESTER, reservation).unwrap();
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(BETA, maker_contribution(FILL_B)).unwrap();
    for contract in [SEPTEMBER, OCTOBER] {
        harness.apply(Command::RegisterContract { contract, event_date: EVENT_DATE }).unwrap();
    }

    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT };
    harness
        .apply(Command::SubmitRequest { requester: REQUESTER, deadline: DEADLINE, legs, n_legs: 2 })
        .unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;

    harness.set_both_clocks(Ts(1_100));
    for (maker, leg, price) in [(ALPHA, 0_u8, FILL_A), (BETA, 1, FILL_B)] {
        harness
            .apply(Command::SubmitQuote {
                maker,
                request,
                leg: LegId(leg),
                price,
                size: SIZE,
                expires_at: Ts(20_000),
            })
            .unwrap();
    }
    harness.set_both_clocks(Ts(1_200));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    expected[1] = ExpectedFill { leg: LegId(1), price: FILL_B };
    harness.apply(Command::AcceptRequest { request, expected, n_legs: 2 }).unwrap();

    harness.submit_pending();
    // Inclusion announces the resolution, the indexer delivers it, and the request reaches
    // Escrowed without anybody asking.
    harness.include_all();
    assert_eq!(harness.locked_escrows().count(), 2);
    let escrows = [harness.escrows()[0], harness.escrows()[1]];
    (harness, request, escrows)
}

/// Move every clock past the event date, so a stall exit becomes reachable in principle.
fn advance_past_the_event(harness: &mut TestHarness) {
    harness.set_both_clocks(Ts(EVENT_DATE.0 + STALL_GRACE.0 + 1));
    harness.oracle_mut().clock_mut().set(Ts(EVENT_DATE.0 + STALL_GRACE.0 + 1));
}

// ═══════════════════════ (a) Final(Yes) → settle → winner paid ═══════════════════════

#[test]
fn a_finalised_contract_pays_its_winner_and_the_escrow_is_spent() {
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));

    // The oracle proposes and the window closes uncontested.
    harness.oracle_mut().propose(SEPTEMBER, Outcome::Yes).unwrap();
    assert_eq!(harness.report_oracle_status(SEPTEMBER).unwrap(), OracleStatus::InProgress);
    harness.oracle_mut().clock_mut().advance(CHALLENGE_WINDOW);
    assert_eq!(harness.oracle_mut().finalise(SEPTEMBER).unwrap(), Outcome::Yes);
    assert_eq!(
        harness.report_oracle_status(SEPTEMBER).unwrap(),
        OracleStatus::Final(Outcome::Yes)
    );

    // Resolution writes one field and touches no escrows. One contract may back thousands,
    // and fanning out would be unbounded work in one critical section (§10.2).
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Resolved(Outcome::Yes)
    );
    assert_eq!(harness.locked_escrows().count(), 2, "no escrow was touched by resolving");

    let before = harness.custody().ledger().balance(REQUESTER);
    harness
        .apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER })
        .unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(true)]);

    // Leg 0 is `Yes` and the outcome is `Yes`, so the requester takes the whole notional.
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(before.0 + notional().0)
    );
    assert_eq!(harness.locked_escrows().count(), 1, "only that escrow was spent");
    harness.assert_settlement_invariants();
}

#[test]
fn settling_the_same_escrow_twice_is_a_no_op() {
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::Yes, ESCALATION).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }).unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(true)]);
    let paid = harness.custody().ledger().balance(REQUESTER);

    // Re-settlement is a no-op, so replay is harmless (§9.2). The command is admissible —
    // it is O(1) and idempotent — and the money does not move a second time.
    harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }).unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(false)]);
    assert_eq!(harness.custody().ledger().balance(REQUESTER), paid, "paid exactly once");
    harness.assert_settlement_invariants();
}

#[test]
fn an_escrow_cannot_be_settled_under_another_contracts_outcome() {
    // Contract-identity confusion arriving through the back door (§11). The engine holds an
    // EscrowId and nothing else about it, so custody is the only place the pairing can be
    // checked — and it is checked.
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::Yes, ESCALATION).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    // escrows[1] rests on OCTOBER, which has no outcome. Naming SEPTEMBER would borrow one.
    harness.apply(Command::SettleEscrow { escrow: escrows[1], contract: SEPTEMBER }).unwrap();
    assert_eq!(
        harness.apply_payouts(),
        vec![Err(CustodyError::EscrowContractMismatch)],
        "an escrow may only be paid under the contract it rests on"
    );
    assert_eq!(harness.locked_escrows().count(), 2, "and nothing moved");
    harness.assert_settlement_invariants();
}

// ═══════════════════════ (b) settle while InProgress → NotYet ═══════════════════════

#[test]
fn an_escrow_on_a_contract_in_progress_cannot_be_settled_and_nothing_moves() {
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().propose(SEPTEMBER, Outcome::Yes).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    let balances = [
        harness.custody().ledger().balance(REQUESTER),
        harness.custody().ledger().balance(ALPHA),
    ];
    assert_eq!(
        harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }),
        Err(HarnessError::Engine(EngineError::OutcomeNotYet))
    );

    // Time gates admissibility; an explicit command moves the money, and there was no
    // admissible command here (§10.2).
    assert!(harness.pending_payouts().is_empty(), "no intent was emitted");
    assert_eq!(harness.custody().ledger().balance(REQUESTER), balances[0]);
    assert_eq!(harness.custody().ledger().balance(ALPHA), balances[1]);
    assert_eq!(harness.locked_escrows().count(), 2);
    harness.assert_settlement_invariants();
}

// ═══════════════════════ (c) contested → escalation → settle ═══════════════════════

#[test]
fn a_contested_contract_ends_only_when_the_escalation_authority_rules() {
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().propose(SEPTEMBER, Outcome::Yes).unwrap();
    harness.oracle_mut().contest(SEPTEMBER).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    // The window closes and it still cannot finalise on its own. Contesting buys a delay,
    // not an outcome.
    harness.oracle_mut().clock_mut().advance(Dur(CHALLENGE_WINDOW.0 * 2));
    assert_eq!(
        harness.oracle_mut().finalise(SEPTEMBER),
        Err(rfq_chain::oracle::OracleError::Contested)
    );
    assert_eq!(
        harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }),
        Err(HarnessError::Engine(EngineError::OutcomeNotYet))
    );

    // Nobody else can end it either — the authority is a single designated id (§10.4).
    assert_eq!(
        harness.oracle_mut().escalate(SEPTEMBER, Outcome::No, ALPHA),
        Err(rfq_chain::oracle::OracleError::NotTheEscalationAuthority)
    );
    assert_eq!(harness.locked_escrows().count(), 2, "and the escrows stay locked");

    // The authority rules, and settlement becomes admissible.
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::No, ESCALATION).unwrap();
    assert_eq!(
        harness.report_oracle_status(SEPTEMBER).unwrap(),
        OracleStatus::Final(Outcome::No)
    );
    let maker_before = harness.custody().ledger().balance(ALPHA);
    harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }).unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(true)]);
    // Leg 0 is `Yes` and the outcome is `No`, so the maker takes it.
    assert_eq!(
        harness.custody().ledger().balance(ALPHA),
        Amount(maker_before.0 + notional().0)
    );
    harness.assert_settlement_invariants();
}

// ═══════════════════ (d0) payout maps through the leg's side ═══════════════════

#[test]
fn one_outcome_pays_the_requester_on_a_yes_leg_and_the_maker_on_a_no_leg() {
    // The point of the mixed-side spread: both directions of the payout mapping in one
    // settlement. There is no implicit buyer or seller — who wins on `Yes` is a property of
    // the leg (§10.3).
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));

    // Both contracts resolve `Yes`.
    for contract in [SEPTEMBER, OCTOBER] {
        harness.oracle_mut().escalate(contract, Outcome::Yes, ESCALATION).unwrap();
        harness.report_oracle_status(contract).unwrap();
    }

    let requester_before = harness.custody().ledger().balance(REQUESTER);
    let alpha_before = harness.custody().ledger().balance(ALPHA);
    let beta_before = harness.custody().ledger().balance(BETA);

    harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }).unwrap();
    harness.apply(Command::SettleEscrow { escrow: escrows[1], contract: OCTOBER }).unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(true), Ok(true)]);

    // Leg 0: requester bought `Yes`, outcome `Yes` → the requester takes the notional.
    // Leg 1: requester bought `No`, outcome `Yes` → the maker takes it. Same outcome value,
    // opposite recipients, decided entirely by the side stored on the escrow.
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + notional().0),
        "the Yes leg pays the requester"
    );
    assert_eq!(
        harness.custody().ledger().balance(BETA),
        Amount(beta_before.0 + notional().0),
        "the No leg pays the maker"
    );
    assert_eq!(harness.custody().ledger().balance(ALPHA), alpha_before, "Alpha lost their leg");
    assert_eq!(harness.locked_escrows().count(), 0);
    harness.assert_settlement_invariants();
}

// ═══════════════════ (d) Silent past the grace period → Void ═══════════════════

#[test]
fn a_silent_oracle_past_the_grace_period_returns_each_side_its_own_contribution() {
    let (mut harness, _request, escrows) = escrowed_spread();

    // Before the grace period elapses there is no outcome, silence or not.
    harness.set_both_clocks(Ts(EVENT_DATE.0 + STALL_GRACE.0));
    assert_eq!(
        harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }),
        Err(HarnessError::Engine(EngineError::OutcomeNotYet)),
        "the stall exit is half-open at the far end too"
    );

    advance_past_the_event(&mut harness);
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().oracle_status(),
        OracleStatus::Silent,
        "the precondition: the stall exit conditions on Silent"
    );

    let requester_before = harness.custody().ledger().balance(REQUESTER);
    let alpha_before = harness.custody().ledger().balance(ALPHA);
    harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }).unwrap();
    assert_eq!(harness.apply_payouts(), vec![Ok(true)]);

    // Asserted **by amount**, not by outcome label. A 50/50 split also "resolves to Void"
    // and moves money between the parties; returning each side its own contribution restores
    // the exact pre-trade allocation (§10.3).
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + requester_contribution(FILL_A).0),
        "the requester gets back exactly what they put in"
    );
    assert_eq!(
        harness.custody().ledger().balance(ALPHA),
        Amount(alpha_before.0 + maker_contribution(FILL_A).0),
        "and the maker exactly what they put in"
    );
    // Which is not the same number, so a 50/50 split would have failed here.
    assert_ne!(
        requester_contribution(FILL_A),
        maker_contribution(FILL_A),
        "the two contributions must differ, or this test cannot tell a refund from a split"
    );
    let half = Amount(notional().0 / 2);
    assert_ne!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + half.0),
        "a split would have paid this instead"
    );
    harness.assert_settlement_invariants();
}

// ═══════════════════ (e) InProgress held indefinitely ═══════════════════

#[test]
fn an_oracle_parked_in_progress_never_times_out_into_void() {
    // The stall exit is unreachable once the oracle reports `InProgress`, so contesting
    // cannot buy a free unwind — otherwise a party who is losing contests a correct outcome,
    // waits out the grace period, and cancels a trade they have already lost (§10.4).
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().propose(SEPTEMBER, Outcome::Yes).unwrap();
    harness.oracle_mut().contest(SEPTEMBER).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    // Far past the grace period, and still nothing.
    advance_past_the_event(&mut harness);
    for _ in 0..3 {
        assert_eq!(
            harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }),
            Err(HarnessError::Engine(EngineError::OutcomeNotYet))
        );
        harness.set_both_clocks(Ts(harness.engine_now().0 + STALL_GRACE.0));
    }

    // The escrows stay locked. That is an oracle-liveness risk, not an engine defect: the
    // engine's second requirement on any oracle is to eventually report `Final` or remain
    // `Silent`, and the escalation authority is the named boundary that resolves a parked one.
    assert_eq!(harness.locked_escrows().count(), 2);
    harness.assert_settlement_invariants();
}

// ═══════════════════ (f) monotonicity: no regression ═══════════════════

#[test]
fn the_oracle_cannot_walk_its_status_backwards() {
    let (mut harness, _request, escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.apply(Command::ReportOracleStatus {
        contract: SEPTEMBER,
        status: OracleStatus::InProgress,
    })
    .unwrap();

    assert_eq!(
        harness.apply(Command::ReportOracleStatus {
            contract: SEPTEMBER,
            status: OracleStatus::Silent,
        }),
        Err(HarnessError::Engine(EngineError::OracleStatusRegression))
    );

    // And the harm it prevents: the stall exit stays unreachable. Without monotonicity a
    // contest-then-retract re-opens the free unwind, which is the whole reason the previous
    // test's property holds.
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().oracle_status(),
        OracleStatus::InProgress,
        "the retraction did not take"
    );
    advance_past_the_event(&mut harness);
    assert_eq!(
        harness.apply(Command::SettleEscrow { escrow: escrows[0], contract: SEPTEMBER }),
        Err(HarnessError::Engine(EngineError::OutcomeNotYet)),
        "a retraction that took would have opened the stall exit here"
    );
    assert_eq!(harness.locked_escrows().count(), 2);
}

// ═══════════════════ (g) Final is immutable ═══════════════════

/// Two **identical positions on one contract** — a request whose two legs both buy `Yes` on
/// September, filled by two different makers.
///
/// Nothing requires legs to name distinct contracts (§16), which is what makes this
/// expressible and what makes the demonstration below exact.
fn two_identical_positions_on_one_contract() -> (TestHarness, [EscrowId; 2]) {
    let mut harness = Harness::new(
        config(),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
    )
    .unwrap();
    harness.deposit(REQUESTER, Amount(requester_contribution(LIMIT).0 * 2)).unwrap();
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(BETA, maker_contribution(FILL_A)).unwrap();
    harness.apply(Command::RegisterContract { contract: SEPTEMBER, event_date: EVENT_DATE }).unwrap();

    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    legs[1] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    harness
        .apply(Command::SubmitRequest { requester: REQUESTER, deadline: DEADLINE, legs, n_legs: 2 })
        .unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;

    harness.set_both_clocks(Ts(1_100));
    for (maker, leg) in [(ALPHA, 0_u8), (BETA, 1)] {
        harness
            .apply(Command::SubmitQuote {
                maker,
                request,
                leg: LegId(leg),
                price: FILL_A,
                size: SIZE,
                expires_at: Ts(20_000),
            })
            .unwrap();
    }
    harness.set_both_clocks(Ts(1_200));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    expected[1] = ExpectedFill { leg: LegId(1), price: FILL_A };
    harness.apply(Command::AcceptRequest { request, expected, n_legs: 2 }).unwrap();
    harness.submit_pending();
    harness.include_all();
    let escrows = [harness.escrows()[0], harness.escrows()[1]];
    (harness, escrows)
}

#[test]
fn final_is_immutable_and_the_failure_it_prevents_is_invisible_to_every_invariant() {
    let (mut harness, escrows) = two_identical_positions_on_one_contract();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::Yes, ESCALATION).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    // The engine refuses the overwrite, and keeps the answer it already had.
    assert_eq!(
        harness.apply(Command::ReportOracleStatus {
            contract: SEPTEMBER,
            status: OracleStatus::Final(Outcome::No),
        }),
        Err(HarnessError::Engine(EngineError::OracleStatusRegression))
    );
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Resolved(Outcome::Yes)
    );

    // Now construct the failure at the custody layer, where the engine's rule does not
    // reach — because that is the only way to show what the rule is for. Both escrows are
    // the same position on the same contract: same side, same size, same price, different
    // makers. Settle the first under `Yes` and the second under `No`, exactly as an
    // overwritten outcome between the two commands would have.
    let requester_before = harness.custody().ledger().balance(REQUESTER);
    let alpha_before = harness.custody().ledger().balance(ALPHA);
    let beta_before = harness.custody().ledger().balance(BETA);

    harness.custody_mut().ledger_mut().settle_escrow(escrows[0], SEPTEMBER, Outcome::Yes).unwrap();
    harness.custody_mut().ledger_mut().settle_escrow(escrows[1], SEPTEMBER, Outcome::No).unwrap();

    // One contract has paid identical positions opposite results, decided by nothing but who
    // sent `SettleEscrow` first. Alpha wrote the same trade as Beta and got the opposite
    // answer.
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + notional().0),
        "the requester won the first escrow"
    );
    assert_eq!(harness.custody().ledger().balance(ALPHA), alpha_before, "and Alpha lost it");
    assert_eq!(
        harness.custody().ledger().balance(BETA),
        Amount(beta_before.0 + notional().0),
        "while Beta, who wrote the identical trade, won"
    );

    // **And conservation still holds.** No unit was duplicated — each escrow paid its own
    // notional — so no invariant in §15 can see this. Each layer stayed internally
    // consistent while the model came apart, which is the same shape as §8.1's lost
    // acknowledgement one level up. Monotonicity is the only thing standing in the way,
    // which is why it is a rule and not hygiene.
    assert_eq!(harness.check_conservation_only(), Ok(()));
    assert_eq!(harness.locked_escrows().count(), 0);
}

#[test]
fn a_final_contract_refuses_every_later_report() {
    let (mut harness, _request, _escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::Yes, ESCALATION).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    for status in [
        OracleStatus::Silent,
        OracleStatus::InProgress,
        OracleStatus::Final(Outcome::No),
        OracleStatus::Final(Outcome::Void),
        OracleStatus::Final(Outcome::Yes),
    ] {
        assert_eq!(
            harness.apply(Command::ReportOracleStatus { contract: SEPTEMBER, status }),
            Err(HarnessError::Engine(EngineError::OracleStatusRegression)),
            "Final is terminal, including against itself"
        );
    }
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Resolved(Outcome::Yes)
    );

    // The oracle mock refuses the same thing on its own side, so the engine is never handed
    // a regression to refuse in the first place.
    assert_eq!(
        harness.oracle_mut().escalate(SEPTEMBER, Outcome::No, ESCALATION),
        Err(rfq_chain::oracle::OracleError::AlreadyFinal)
    );
}

#[test]
fn resolution_is_reported_once_and_reaches_the_publisher() {
    let (mut harness, _request, _escrows) = escrowed_spread();
    harness.set_both_clocks(Ts(2_000));
    harness.oracle_mut().clock_mut().set(Ts(2_000));
    harness.oracle_mut().propose(SEPTEMBER, Outcome::Yes).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();
    harness.oracle_mut().clock_mut().advance(CHALLENGE_WINDOW);
    harness.oracle_mut().finalise(SEPTEMBER).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();

    let resolved: Vec<(ContractIdx, Outcome)> = harness
        .emitted()
        .iter()
        .filter_map(|event| match event {
            Event::ContractResolved { contract, outcome } => Some((*contract, *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        resolved,
        vec![(SEPTEMBER, Outcome::Yes)],
        "InProgress announces nothing; only finality does"
    );
}
