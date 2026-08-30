//! S3 gate: PLAN (a)–(e), driven through the harness.
//!
//! The harness is the only structure that can see both systems, so it is where conservation
//! and claim coverage are asserted — after every command, in every test here. Neither is a
//! core assertion and neither can be: the engine cannot compute a global sum without reading
//! custody, and reading custody is exactly what the seam forbids (SPEC §13.1).
//!
//! Accounts are funded to **exactly** their contribution (CLAUDE 38). Slack absorbs a claim
//! released twice or held once too often, and coverage would then pass regardless of
//! correctness.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_chain::custody::{CustodyError, SettleError};
use rfq_core::account::AccountIdx;
use rfq_core::clock::TestClock;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::{ContractIdx, Outcome};
use rfq_core::request::{ReqIdx, RequestState};
use rfq_core::types::{Amount, Dur, LegId, Price, Side, Size, Ts};
use rfq_runtime::harness::{CrossSystemViolation, Harness, HarnessError};

const REQUESTER: AccountIdx = AccountIdx(0);
const ALPHA: AccountIdx = AccountIdx(1);
const BETA: AccountIdx = AccountIdx(2);
const GAMMA: AccountIdx = AccountIdx(3);

const SEPTEMBER: ContractIdx = ContractIdx(0);
const OCTOBER: ContractIdx = ContractIdx(1);
const NOVEMBER: ContractIdx = ContractIdx(2);
const EVENT_DATE: Ts = Ts(50_000_000);
const DEADLINE: Ts = Ts(100_000);
const SIZE: Size = Size(100);

/// Limits, and the prices the makers actually quote.
const LIMIT_A: Price = Price(650_000);
const LIMIT_B: Price = Price(500_000);
const LIMIT_C: Price = Price(400_000);
const FILL_A: Price = Price(610_000);
const FILL_B: Price = Price(450_000);
const FILL_C: Price = Price(380_000);

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
        ..Config::default()
    }
}

/// The same venue, admitting a non-zero indexer lag — and widening the timelock to cover it,
/// because §9.3's inequality includes the lag terms.
///
/// Needed because with every lag term at zero the theorem below holds absolutely: no
/// participant can withdraw out from under their own live claim, so a withdrawal can never
/// land inside a settlement window. Staleness is the only thing that reopens it, and a venue
/// that wants to exhibit staleness has to admit it has some.
fn laggy_config() -> Config {
    let base = config();
    let indexer_lag = Dur(400_000);
    let claim_window = Dur(base.max_request_ttl.0 + base.max_settling_time.0);
    Config {
        max_indexer_lag: indexer_lag,
        withdrawal_delay: Dur(claim_window.0 + indexer_lag.0 + 1),
        ..base
    }
}

type TestHarness = Harness<TestClock, TestClock>;

fn maker_contribution(price: Price) -> Amount {
    SIZE.maker_contribution(price).unwrap()
}

fn requester_contribution(price: Price) -> Amount {
    SIZE.requester_contribution(price).unwrap()
}

/// The requester's reservation is `Σ size × limit`, taken before any price exists.
fn requester_reservation() -> Amount {
    Amount(
        requester_contribution(LIMIT_A).0
            + requester_contribution(LIMIT_B).0
            + requester_contribution(LIMIT_C).0,
    )
}

/// The requester's actual fill, `Σ size × fill_price`.
fn requester_fill() -> Amount {
    Amount(
        requester_contribution(FILL_A).0
            + requester_contribution(FILL_B).0
            + requester_contribution(FILL_C).0,
    )
}

/// A harness with everyone funded to **exactly** what they will spend.
///
/// The requester is funded to their reservation, not their fill: they must be able to open
/// the request at all, and the over-reservation is released at commit. Each maker is funded
/// to exactly the one contribution they will make.
fn funded_harness() -> TestHarness {
    let mut harness =
        Harness::new(config(), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000))).unwrap();
    harness.deposit(REQUESTER, requester_reservation()).unwrap();
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(BETA, maker_contribution(FILL_B)).unwrap();
    harness.deposit(GAMMA, maker_contribution(FILL_C)).unwrap();
    for contract in [SEPTEMBER, OCTOBER, NOVEMBER] {
        harness.apply(Command::RegisterContract { contract, event_date: EVENT_DATE }).unwrap();
    }
    harness
}

fn three_leg_request() -> Command {
    three_leg_request_with_deadline(DEADLINE)
}

fn three_leg_request_with_deadline(deadline: Ts) -> Command {
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT_B };
    legs[2] = LegSpec { contract: NOVEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_C };
    Command::SubmitRequest { requester: REQUESTER, deadline, legs, n_legs: 3 }
}

fn accept() -> [ExpectedFill; MAX_LEGS] {
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    expected[1] = ExpectedFill { leg: LegId(1), price: FILL_B };
    expected[2] = ExpectedFill { leg: LegId(2), price: FILL_C };
    expected
}

/// Open the request, quote all three legs, and accept. Leaves one bundle pending.
fn matched_market(harness: &mut TestHarness) -> ReqIdx {
    harness.apply(three_leg_request()).unwrap();
    let request = harness
        .engine()
        .ledger()
        .requests()
        .next()
        .map(|(handle, _)| handle)
        .expect("the request was opened");

    harness.engine_clock_mut().advance(Dur(100));
    for (maker, leg, price) in
        [(ALPHA, 0_u8, FILL_A), (BETA, 1, FILL_B), (GAMMA, 2, FILL_C)]
    {
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

    harness.engine_clock_mut().advance(Dur(100));
    harness
        .apply(Command::AcceptRequest { request, expected: accept(), n_legs: 3 })
        .unwrap();
    request
}

// ═══════════════════════════════ (a) all funds present ═══════════════════════════════

#[test]
fn a_bundle_whose_funds_are_all_present_settles() {
    let mut harness = funded_harness();
    let request = matched_market(&mut harness);
    assert_eq!(harness.pending_bundles().len(), 1, "accept emitted exactly one intent");

    // Before settlement: capital is `committed` in the engine and still in custody's
    // balances. Escrow does not exist yet.
    assert_eq!(
        harness.engine().ledger().account(REQUESTER).unwrap().committed(),
        requester_fill()
    );
    assert_eq!(harness.custody().ledger().escrows().count(), 0);

    let outcomes = harness.settle_pending();
    assert_eq!(outcomes.len(), 1);
    let receipt = outcomes[0].expect("all funds are present");
    assert_eq!(receipt.n_escrows, 3, "one escrow per leg");

    // Every maker is now empty — funded to exactly their contribution, so a claim released
    // twice would have overdrawn somebody rather than being absorbed.
    for maker in [ALPHA, BETA, GAMMA] {
        assert_eq!(harness.custody().ledger().balance(maker), Amount::ZERO);
    }
    // The requester keeps only the over-reservation that was released at commit.
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_reservation().0 - requester_fill().0)
    );

    // Each escrow holds both contributions separately, and they sum to the notional.
    let escrows: Vec<_> =
        harness.custody().ledger().escrows().map(|(_, escrow)| *escrow).collect();
    assert_eq!(escrows.len(), 3);
    for (escrow, price) in escrows.iter().zip([FILL_A, FILL_B, FILL_C]) {
        assert_eq!(escrow.requester_contribution(), requester_contribution(price));
        assert_eq!(escrow.maker_contribution(), maker_contribution(price));
        assert_eq!(escrow.notional(), SIZE.notional().unwrap());
        assert!(escrow.is_locked());
    }
    // The requester's side per leg survives into the escrow — the payout mapping consumes
    // it, and there is no implicit buyer or seller.
    assert_eq!(escrows[0].side(), Side::Yes);
    assert_eq!(escrows[1].side(), Side::No);
    assert_eq!(escrows[2].side(), Side::Yes);

    // The nonce is consumed, and it is the request's.
    let RequestState::Settling { nonce, .. } =
        harness.engine().ledger().request(request).unwrap().state()
    else {
        panic!("Settling");
    };
    assert!(harness.custody().ledger().nonce_used(nonce));

    harness.assert_cross_system_invariants();
}

#[test]
fn a_resubmitted_bundle_bounces_off_its_own_nonce() {
    // One nonce per bundle, not per leg. A retry bouncing off its own nonce is evidence the
    // original succeeded (§8.1) — reading it as failure is the duplication path, which is
    // S4's subject. Here it is enough that the second inclusion changes nothing.
    let mut harness = funded_harness();
    matched_market(&mut harness);
    let bundle = harness.pending_bundles()[0];
    harness.settle_pending()[0].expect("first inclusion settles");

    let escrows_after_first = harness.custody().ledger().escrows().count();
    assert_eq!(harness.custody_mut().settle(&bundle), Err(SettleError::NonceReused));
    assert_eq!(
        harness.custody().ledger().escrows().count(),
        escrows_after_first,
        "a reused nonce forms no second escrow"
    );
    harness.assert_cross_system_invariants();
}

// ═════════════════ (b) a withdrawal executed before the accept ═════════════════

/// A market in which the requester's withdrawal is due to land mid-settlement.
///
/// Reaching this state needs a **stale mirror**, and nothing else will do. With every lag
/// term at zero the §9.3 inequality makes it unreachable: a withdrawal executes
/// `WITHDRAWAL_DELAY` after it is requested, and that is strictly longer than any claim can
/// bind, so requesting one first drops availability before the claim is admitted and
/// requesting one later lands after the claim is dead. A lagging indexer breaks the second
/// half — the engine keeps admitting against a balance custody has already promised away.
///
/// Returns the market with one bundle accepted and the withdrawal maturing at `MATURES_AT`.
fn market_with_a_requester_withdrawal_in_flight() -> (TestHarness, ReqIdx, Ts) {
    let config = laggy_config();
    let mut harness =
        Harness::new(config, TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000))).unwrap();
    harness.deposit(REQUESTER, requester_reservation()).unwrap();
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(BETA, maker_contribution(FILL_B)).unwrap();
    harness.deposit(GAMMA, maker_contribution(FILL_C)).unwrap();
    for contract in [SEPTEMBER, OCTOBER, NOVEMBER] {
        harness.apply(Command::RegisterContract { contract, event_date: EVENT_DATE }).unwrap();
    }

    // The requester asks for their money back. Availability drops in custody at once; the
    // balance stays present for the timelock's whole duration.
    harness.stall_indexer();
    let matures_at = harness.request_withdrawal(REQUESTER, requester_reservation()).unwrap();
    assert_eq!(harness.custody().ledger().available(REQUESTER), Amount::ZERO);
    assert_eq!(harness.custody().ledger().balance(REQUESTER), requester_reservation());
    assert_eq!(
        harness.engine().ledger().account(REQUESTER).unwrap().free(),
        requester_reservation(),
        "the engine has not heard, which is the only reason what follows is possible"
    );

    // Much later — but still before maturity — the request is opened, quoted and accepted.
    harness.set_both_clocks(Ts(500_000));
    harness.apply(three_leg_request_with_deadline(Ts(780_000))).unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;

    harness.set_both_clocks(Ts(760_000));
    for (maker, leg, price) in [(ALPHA, 0_u8, FILL_A), (BETA, 1, FILL_B), (GAMMA, 2, FILL_C)] {
        harness
            .apply(Command::SubmitQuote {
                maker,
                request,
                leg: LegId(leg),
                price,
                size: SIZE,
                expires_at: Ts(785_000),
            })
            .unwrap();
    }
    harness.set_both_clocks(Ts(760_500));
    harness
        .apply(Command::AcceptRequest { request, expected: accept(), n_legs: 3 })
        .unwrap();
    assert!(matures_at > harness.custody_now(), "the withdrawal has not landed yet");
    (harness, request, matures_at)
}

#[test]
fn a_withdrawal_executed_before_the_accept_is_caught_by_the_pre_check() {
    let (mut harness, _request, matures_at) = market_with_a_requester_withdrawal_in_flight();

    // The withdrawal matures and lands before anyone submits.
    harness.set_both_clocks(matures_at);
    assert_eq!(harness.execute_withdrawal(REQUESTER).unwrap(), requester_reservation());
    assert_eq!(harness.custody().ledger().balance(REQUESTER), Amount::ZERO);

    let bundle = harness.pending_bundles()[0];
    assert_eq!(
        harness.custody().precheck(&bundle),
        Err(SettleError::InsufficientFunds { account: REQUESTER })
    );
    let outcomes = harness.settle_pending();
    assert_eq!(outcomes[0], Err(SettleError::InsufficientFunds { account: REQUESTER }));

    // Nothing committed: no escrow, no maker debited, nonce unconsumed so the basket can be
    // retried if the money comes back.
    assert_eq!(harness.custody().ledger().escrows().count(), 0);
    assert_eq!(harness.custody().ledger().balance(ALPHA), maker_contribution(FILL_A));
    assert_eq!(harness.custody().ledger().balance(BETA), maker_contribution(FILL_B));
    assert_eq!(harness.custody().ledger().balance(GAMMA), maker_contribution(FILL_C));
    assert!(!harness.custody().ledger().nonce_used(bundle.nonce));

    // Conservation, escrow contributions and mirror agreement all hold. Claim coverage does
    // not, and that is the design: the requester's capital is still committed to a basket the
    // engine has not yet learned reverted. `PollSettlement` on a `Reverted` nonce is the
    // exit (§8, §15.6).
    harness.assert_settlement_invariants();
    assert_eq!(
        harness.check_cross_system_invariants(),
        Err(CrossSystemViolation::ClaimCoverageBroken(REQUESTER))
    );
}

// ═════════════ (c) the withdrawal lands between pre-check and settle ═════════════

#[test]
fn a_withdrawal_landing_between_the_pre_check_and_the_settle_reverts_the_whole_basket() {
    // The pre-check is an optimisation with **no correctness role**. Checking then
    // submitting is TOCTOU: the window between check and inclusion is exactly where a
    // withdrawal lands, and the authoritative validation is inside the transaction (§8.2).
    let (mut harness, _request, matures_at) = market_with_a_requester_withdrawal_in_flight();
    let bundle = harness.pending_bundles()[0];

    // At this instant the withdrawal has not matured and the pre-check passes: the money is
    // still present, and every quote is live at custody's clock too.
    assert_eq!(harness.custody().precheck(&bundle), Ok(()));
    assert_eq!(harness.custody().ledger().balance(REQUESTER), requester_reservation());

    // The clock reaches maturity, and the withdrawal lands *inside* the transaction — after
    // entry, before validation.
    harness.set_both_clocks(matures_at);
    harness.custody_mut().on_settle_entry(Box::new(|ledger, now| {
        ledger.execute_matured_withdrawals(now);
    }));

    let outcomes = harness.settle_pending();
    assert_eq!(
        outcomes[0],
        Err(SettleError::InsufficientFunds { account: REQUESTER }),
        "settle must revalidate rather than trust the pre-check"
    );

    // Wholesale revert: not one leg settled, not one maker debited, and the nonce unconsumed.
    // Multi-leg atomicity here is inherited from the transaction, not built.
    assert_eq!(harness.custody().ledger().escrows().count(), 0);
    assert_eq!(harness.custody().ledger().balance(ALPHA), maker_contribution(FILL_A));
    assert_eq!(harness.custody().ledger().balance(BETA), maker_contribution(FILL_B));
    assert_eq!(harness.custody().ledger().balance(GAMMA), maker_contribution(FILL_C));
    assert!(!harness.custody().ledger().nonce_used(bundle.nonce));
    harness.assert_settlement_invariants();
    assert_eq!(
        harness.check_cross_system_invariants(),
        Err(CrossSystemViolation::ClaimCoverageBroken(REQUESTER)),
        "the committed basket is unbacked until the engine learns the settlement reverted"
    );
}

#[test]
fn no_participant_can_withdraw_out_from_under_their_own_live_claim() {
    // The theorem §9.3's inequality produces, for **both** claim windows. A maker's capital
    // is claimed for the life of a quote; a requester's from SubmitRequest until settlement
    // resolves. The inequality takes a maximum over the two, so neither side can do it.
    //
    // Two orderings exhaust it:
    //
    //   claim then withdraw — the claim expires at `T_claim + window` and the withdrawal
    //     lands at `T_w + DELAY` with `T_w >= T_claim`. Since DELAY > window, the claim is
    //     strictly dead first.
    //   withdraw then claim — availability has already dropped, so the engine only admits a
    //     claim the *remaining* balance covers, and that is exactly what survives execution.
    let config = config();
    let delay = config.withdrawal_delay;
    let windows = [
        ("maker", config.max_quote_ttl),
        ("requester", Dur(config.max_request_ttl.0 + config.max_settling_time.0)),
    ];

    // Case one, at the worst instant: the withdrawal requested the same millisecond as the
    // claim, which is the latest it can be while still preceding it.
    for (side, window) in windows {
        assert!(delay > window, "the {side} window is not covered by the timelock");
        for claim_at in [0_u64, 1, 999, 1_000] {
            let claim_dies = claim_at + window.0;
            let withdrawal_lands = claim_at + delay.0;
            assert!(
                claim_dies < withdrawal_lands,
                "a {side} claim written at {claim_at} outlives the withdrawal beside it"
            );
        }
    }

    // Case two is enforced by admission, and the harness can watch it happen — for the maker
    // side, where a fresh claim is admitted against the mirror.
    let mut harness = funded_harness();
    let stake = maker_contribution(FILL_A);
    harness.request_withdrawal(ALPHA, stake).unwrap();
    assert_eq!(
        harness.engine().ledger().account(ALPHA).unwrap().free(),
        Amount::ZERO,
        "the engine will not lend against money already on its way out"
    );
    harness.apply(three_leg_request()).unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;
    harness.engine_clock_mut().advance(Dur(100));
    assert!(
        harness
            .apply(Command::SubmitQuote {
                maker: ALPHA,
                request,
                leg: LegId(0),
                price: FILL_A,
                size: SIZE,
                expires_at: Ts(20_000),
            })
            .is_err(),
        "a maker with a withdrawal pending cannot write a quote the remainder cannot cover"
    );

    // And the requester side: with their withdrawal pending they cannot open a new request
    // against the same money either.
    harness.request_withdrawal(REQUESTER, requester_reservation()).unwrap();
    assert!(
        harness.apply(three_leg_request()).is_err(),
        "a requester with a withdrawal pending cannot open a request the remainder cannot cover"
    );
    harness.assert_cross_system_invariants();
}

// ═══════════════════════════════ (d) the timelock ═══════════════════════════════

#[test]
fn requesting_a_withdrawal_drops_availability_immediately_and_the_balance_not_at_all() {
    // Property one of three. Admission uses availability — forward-looking, never lend
    // against money already on its way out. Settlement uses balance — present-tense, is the
    // money here for this transaction (§9.1).
    let mut harness = funded_harness();
    let stake = maker_contribution(FILL_A);
    assert_eq!(harness.custody().ledger().available(ALPHA), stake);

    let matures_at = harness.request_withdrawal(ALPHA, stake).unwrap();
    assert_eq!(harness.custody().ledger().available(ALPHA), Amount::ZERO, "immediately");
    assert_eq!(harness.custody().ledger().balance(ALPHA), stake, "and not before maturity");
    assert_eq!(matures_at, Ts(1_000).checked_add(config().withdrawal_delay).unwrap());

    // A second request is refused rather than merged; merging would have to pick a maturity.
    assert_eq!(
        harness.request_withdrawal(ALPHA, Amount(1)),
        Err(HarnessError::Custody(CustodyError::WithdrawalAlreadyPending))
    );
    harness.assert_cross_system_invariants();
}

#[test]
fn execution_occurs_at_exactly_the_delay_and_consults_nothing_else() {
    // Property two. Execution is unconditional at maturity: **regardless of quote state**.
    // A timelock that could be extended by outstanding obligations would be a lock nobody
    // could reason about, and the four-term inequality would be guaranteeing nothing.
    let mut harness = funded_harness();
    let request_command = three_leg_request();
    harness.apply(request_command).unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;
    harness.engine_clock_mut().advance(Dur(100));
    // Alpha has a live quote standing when they ask to withdraw.
    harness
        .apply(Command::SubmitQuote {
            maker: ALPHA,
            request,
            leg: LegId(0),
            price: FILL_A,
            size: SIZE,
            expires_at: Ts(20_000),
        })
        .unwrap();
    let stake = maker_contribution(FILL_A);

    // Withdrawing needs available funds, and the engine's claim does not reduce custody's
    // availability — a reservation is a soft claim the chain has no concept of (§1).
    // Maturity is measured from custody's clock, which is the only clock that matters here.
    let matures_at = harness.request_withdrawal(ALPHA, stake).unwrap();

    harness.set_both_clocks(Ts(matures_at.0 - 1));
    assert_eq!(
        harness.execute_withdrawal(ALPHA),
        Err(HarnessError::Custody(CustodyError::WithdrawalNotMatured)),
        "one millisecond early"
    );
    assert_eq!(harness.custody().ledger().balance(ALPHA), stake);

    harness.set_both_clocks(matures_at);
    assert_eq!(harness.execute_withdrawal(ALPHA), Ok(stake), "at exactly T+delay");
    assert_eq!(harness.custody().ledger().balance(ALPHA), Amount::ZERO);

    // The quote neither delayed the withdrawal nor was consulted about it. By the time the
    // timelock expires the quote is long dead — which is the §9.3 inequality doing its job,
    // and the reason this can never leave the maker's capital unbacked.
    assert!(harness.custody_now() > Ts(20_000), "the quote's expiry is behind us");
    harness.assert_cross_system_invariants();
}

#[test]
fn a_pending_withdrawal_does_not_kill_a_basket_already_in_flight() {
    // The harm the balance-versus-availability distinction prevents. If settlement validated
    // availability, `RequestWithdrawal` would instantly kill every basket in flight — last
    // look reintroduced through custody, and invisible, because the timelock would still
    // appear to function while every settlement quietly failed (§9.1).
    let mut harness = funded_harness();
    matched_market(&mut harness);
    let bundle = harness.pending_bundles()[0];

    // Alpha asks for everything back. Custody has no concept of the engine's claim, so this
    // is admissible — and the money stays present until the timelock expires.
    let stake = maker_contribution(FILL_A);
    harness.request_withdrawal(ALPHA, stake).unwrap();
    assert_eq!(harness.custody().ledger().available(ALPHA), Amount::ZERO);
    assert_eq!(harness.custody().ledger().balance(ALPHA), stake, "still present");

    // The pre-check and the settlement both look at the balance, so the basket is unharmed.
    assert_eq!(harness.custody().precheck(&bundle), Ok(()));
    let outcomes = harness.settle_pending();
    let receipt = outcomes[0].expect("a pending withdrawal must not kill a basket in flight");
    assert_eq!(receipt.n_escrows, 3);
    assert_eq!(harness.custody().ledger().balance(ALPHA), Amount::ZERO, "into escrow");
    harness.assert_settlement_invariants();
}

#[test]
fn a_withdrawal_executes_at_maturity_even_with_capital_locked_in_escrow() {
    // Property two again, against the other kind of outstanding obligation. Execution is
    // unconditional at maturity: it consults no quote and no escrow. A timelock that could
    // be extended by obligations would be a lock nobody could reason about, and the
    // four-term inequality would be guaranteeing nothing about when money is safe to spend.
    let mut harness = funded_harness();
    matched_market(&mut harness);
    harness.settle_pending()[0].expect("settles");
    assert_eq!(harness.locked_escrows().count(), 3, "Alpha has capital locked in escrow");
    assert!(
        harness.locked_escrows().any(|(_, escrow)| escrow.maker() == ALPHA),
        "the account under test really does have an outstanding escrow"
    );

    // Alpha is paid again and asks to withdraw it.
    let fresh = Amount(1_000);
    harness.deposit(ALPHA, fresh).unwrap();
    let matures_at = harness.request_withdrawal(ALPHA, fresh).unwrap();

    harness.set_both_clocks(Ts(matures_at.0 - 1));
    assert_eq!(
        harness.execute_withdrawal(ALPHA),
        Err(HarnessError::Custody(CustodyError::WithdrawalNotMatured))
    );

    harness.set_both_clocks(matures_at);
    assert_eq!(
        harness.execute_withdrawal(ALPHA),
        Ok(fresh),
        "the locked escrow neither delayed the withdrawal nor was consulted"
    );
    assert_eq!(harness.custody().ledger().balance(ALPHA), Amount::ZERO);
    assert_eq!(harness.locked_escrows().count(), 3, "and the escrow is untouched");
    harness.assert_settlement_invariants();
}

#[test]
fn a_quote_admitted_just_before_a_withdrawal_must_die_before_it_executes() {
    // Property three, and the one the four-term inequality exists to guarantee: a quote
    // admitted at `T − ε` with the maximum lifetime has settled or expired **strictly
    // before** the withdrawal lands. This is what makes soft reservation safe.
    let config = config();
    let quote_ttl = config.max_quote_ttl;
    let delay = config.withdrawal_delay;

    // The worst case the venue admits: a quote taken out one instant before the withdrawal
    // request, binding for the longest lifetime policy allows.
    let admitted_at = Ts(1_000);
    let requested_at = admitted_at.checked_add(Dur(1)).unwrap();
    let quote_dies_at = admitted_at.checked_add(quote_ttl).unwrap();
    let withdrawal_lands_at = requested_at.checked_add(delay).unwrap();

    assert!(
        quote_dies_at < withdrawal_lands_at,
        "a quote binding until {quote_dies_at:?} outlives a withdrawal landing at \
         {withdrawal_lands_at:?}; the §9.3 inequality is not doing its job"
    );

    // And the margin is exactly the inequality's slack, so the test is measuring the
    // inequality and not a coincidence of the default numbers.
    let slack = delay.0 - quote_ttl.0;
    assert!(slack > 0, "WITHDRAWAL_DELAY must exceed MAX_QUOTE_TTL");
    assert_eq!(withdrawal_lands_at.0 - quote_dies_at.0, slack + 1);
}

// ═══════════ (e) a configuration that violates the inequality fails to start ═══════════

#[test]
fn a_venue_whose_timelock_does_not_cover_the_lag_terms_fails_to_start() {
    // With every lag term zero the inequality reduces to `delay > max_quote_ttl` and a test
    // of the four-term form would pass without exercising three of its four terms
    // (CLAUDE 39). So a non-zero lag term is what does the violating.
    // The claim windows are shrunk to fit under a 40s timelock so the lag terms are the only
    // thing that can push the sum over it.
    let base = Config {
        withdrawal_delay: Dur(40_000),
        max_quote_ttl: Dur(30_000),
        max_request_ttl: Dur(20_000),
        max_settling_time: Dur(5_000),
        ..config()
    };
    assert_eq!(base.max_indexer_lag, Dur::ZERO);
    assert!(
        Harness::new(base, TestClock::at(Ts(0)), TestClock::at(Ts(0)), TestClock::at(Ts(0))).is_ok(),
        "the two-term form is satisfied, which is the precondition this test needs"
    );

    let violating = Config { max_indexer_lag: Dur(15_000), ..base };
    assert_ne!(violating.max_indexer_lag, Dur::ZERO, "the lag term under test must be non-zero");
    assert!(matches!(
        Harness::new(violating, TestClock::at(Ts(0)), TestClock::at(Ts(0)), TestClock::at(Ts(0))),
        Err(rfq_core::config::ConfigError::WithdrawalDelayTooShort)
    ));

    // And the harm it prevents, not just the variant: under that configuration the engine's
    // view can be stale for longer than the timelock leaves the money present, so a claim
    // admitted against the mirror can outlive the balance backing it.
    assert!(
        violating.max_quote_ttl.0 + violating.max_indexer_lag.0 > violating.withdrawal_delay.0,
        "the refused configuration is one where a claim can outlive the money behind it"
    );

    // Each lag term is individually load-bearing.
    for candidate in [
        Config { confirmations: 12, block_time: Dur(1_000), ..base },
        Config { max_settlement_inclusion_time: Dur(11_000), ..base },
    ] {
        assert!(matches!(
            Harness::new(candidate, TestClock::at(Ts(0)), TestClock::at(Ts(0)), TestClock::at(Ts(0))),
            Err(rfq_core::config::ConfigError::WithdrawalDelayTooShort)
        ));
    }
}

// ═══════════════════════════ cross-system invariants ═══════════════════════════

#[test]
fn conservation_counts_locked_escrows_only() {
    // A `Settled` escrow has already paid out; its notional is back in someone's balance.
    // Summing every escrow regardless of state makes the first payout read as newly created
    // money and the invariant fails on a correct system — which is worse than not having it,
    // because the natural repair is to weaken the assertion (CLAUDE 41).
    let mut harness = funded_harness();
    matched_market(&mut harness);
    harness.settle_pending()[0].expect("settles");
    harness.assert_cross_system_invariants();

    let locked = harness.locked_escrows().count();
    assert_eq!(locked, 3);

    // Pay one out as a void: each side's own contribution returned, restoring the exact
    // pre-trade allocation. Splitting the notional would move money between the parties and
    // is a redistribution disguised as neutrality (§10.3).
    let first = harness.escrows()[0];
    let requester_before = harness.custody().ledger().balance(REQUESTER);
    let maker_before = harness.custody().ledger().balance(ALPHA);
    assert!(harness.custody_mut().ledger_mut().settle_escrow(first, SEPTEMBER, Outcome::Void).unwrap());

    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + requester_contribution(FILL_A).0)
    );
    assert_eq!(
        harness.custody().ledger().balance(ALPHA),
        Amount(maker_before.0 + maker_contribution(FILL_A).0),
        "not half each — each side's own contribution"
    );

    // Conservation still holds with that escrow no longer counted: its notional is back in
    // the two balances, so counting it as well would read as newly created money.
    assert_eq!(harness.locked_escrows().count(), 2);
    assert_eq!(harness.check_conservation_for_test(), Ok(()));

    // Re-settlement is a no-op, so replay is harmless (§9.2).
    assert_eq!(harness.custody_mut().ledger_mut().settle_escrow(first, SEPTEMBER, Outcome::Void), Ok(false));
    assert_eq!(harness.check_conservation_for_test(), Ok(()));
}

#[test]
fn claim_coverage_is_a_cross_system_assertion_and_the_harness_can_break_it() {
    // The assertion has teeth only if the state it forbids is reachable. Custody executing a
    // withdrawal out from under a live claim is exactly the failure the §9.3 timelock
    // prevents — so it is reachable here only because the test bypasses the timelock's
    // purpose by advancing the chain clock past it.
    let mut harness = funded_harness();
    harness.apply(three_leg_request()).unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;
    harness.engine_clock_mut().advance(Dur(100));
    harness
        .apply(Command::SubmitQuote {
            maker: ALPHA,
            request,
            leg: LegId(0),
            price: FILL_A,
            size: SIZE,
            expires_at: Ts(20_000),
        })
        .unwrap();
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));

    harness.request_withdrawal(ALPHA, maker_contribution(FILL_A)).unwrap();

    // Custody's clock alone runs to maturity: the chain executes the withdrawal while the
    // venue still believes Alpha's quote is live. That divergence is not something v1
    // produces — it is injected here, because the state coverage exists to catch is
    // otherwise unreachable, and an assertion nobody can make fail is decorative.
    harness.custody_clock_mut().advance(config().withdrawal_delay);
    harness.execute_withdrawal(ALPHA).unwrap();

    assert_eq!(
        harness.check_cross_system_invariants(),
        Err(CrossSystemViolation::ClaimCoverageBroken(ALPHA)),
        "coverage must notice that the engine has promised capital custody does not hold"
    );
}

#[test]
fn a_settlement_debits_each_account_once_for_every_leg_it_wins() {
    // Two legs to one maker. Checking each leg independently against the full balance would
    // pass on a balance that covers either one alone, so the debits are accumulated per
    // account before any is checked.
    let mut harness =
        Harness::new(config(), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000))).unwrap();
    let two_legs = Amount(maker_contribution(FILL_A).0 + maker_contribution(FILL_B).0);
    harness.deposit(REQUESTER, requester_reservation()).unwrap();
    // Alpha is funded for exactly ONE of the two legs it is about to win.
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(GAMMA, maker_contribution(FILL_C)).unwrap();
    for contract in [SEPTEMBER, OCTOBER, NOVEMBER] {
        harness.apply(Command::RegisterContract { contract, event_date: EVENT_DATE }).unwrap();
    }
    harness.apply(three_leg_request()).unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;
    harness.engine_clock_mut().advance(Dur(100));
    for (maker, leg, price) in [(ALPHA, 0_u8, FILL_A), (ALPHA, 1, FILL_B), (GAMMA, 2, FILL_C)] {
        // The engine admits both: its own mirror says Alpha can cover each in turn only
        // because the first leg's claim is already counted, so it refuses the second.
        let _ = harness.apply(Command::SubmitQuote {
            maker,
            request,
            leg: LegId(leg),
            price,
            size: SIZE,
            expires_at: Ts(20_000),
        });
    }
    assert!(two_legs > maker_contribution(FILL_A), "the two legs really do cost more than one");

    // The engine's own claim coverage refused the second quote, so the basket cannot form —
    // which is the admission half of the same protection.
    harness.engine_clock_mut().advance(Dur(100));
    let outcome = harness.apply(Command::AcceptRequest { request, expected: accept(), n_legs: 3 });
    assert!(outcome.is_err(), "leg 1 has no quote, so the basket aborts");
    harness.assert_cross_system_invariants();
}
