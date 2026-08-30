//! S4 gate: PLAN (a)–(f), the three-outcome settlement boundary.
//!
//! A synchronous call has two outcomes. A transaction submitted to a network you do not
//! control has three, because between submission and inclusion there is an interval in which
//! no local answer exists. Every guess made in that interval is wrong — release and the maker
//! requotes the same capital before the transaction lands; record the escrow and it reverts;
//! hold forever and the node never received it.
//!
//! The chain mock's queue is advanced by the test and by nothing else: no timers, no threads,
//! no sleeps, so every interleaving is reachable by construction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_chain::custody::SettleError;
use rfq_core::account::AccountIdx;
use rfq_core::clock::TestClock;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::ContractIdx;
use rfq_core::event::{Event, QuoteRejectReason};
use rfq_core::quote::QuoteState;
use rfq_core::request::{ReqIdx, RequestState};
use rfq_core::reservation::ResOwner;
use rfq_core::settlement::TxStatus;
use rfq_core::types::{Amount, Dur, LegId, Price, Side, Size, Ts};
use rfq_core::engine::EngineError;
use rfq_runtime::harness::{Harness, HarnessError};

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

const LIMIT_A: Price = Price(650_000);
const LIMIT_B: Price = Price(500_000);
const LIMIT_C: Price = Price(400_000);
const FILL_A: Price = Price(610_000);
const FILL_B: Price = Price(450_000);
const FILL_C: Price = Price(380_000);

/// Short enough that the settling deadline falls while the winning quotes are still live —
/// otherwise a retry after the deadline reverts on expiry and the test would be measuring
/// the wrong revert.
const SETTLING_TIME: Dur = Dur(5_000);

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
        max_settling_time: SETTLING_TIME,
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
fn requester_reservation() -> Amount {
    Amount(
        requester_contribution(LIMIT_A).0
            + requester_contribution(LIMIT_B).0
            + requester_contribution(LIMIT_C).0,
    )
}
fn requester_fill() -> Amount {
    Amount(
        requester_contribution(FILL_A).0
            + requester_contribution(FILL_B).0
            + requester_contribution(FILL_C).0,
    )
}

/// A market that has been accepted, with one bundle waiting to be sent.
///
/// Everyone is funded to **exactly** their contribution (CLAUDE 38), so a claim released
/// twice overdraws somebody instead of being absorbed.
fn accepted_market(config: Config) -> (TestHarness, ReqIdx) {
    let mut harness =
        Harness::new(config, TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000)), TestClock::at(Ts(1_000))).unwrap();
    harness.deposit(REQUESTER, requester_reservation()).unwrap();
    harness.deposit(ALPHA, maker_contribution(FILL_A)).unwrap();
    harness.deposit(BETA, maker_contribution(FILL_B)).unwrap();
    harness.deposit(GAMMA, maker_contribution(FILL_C)).unwrap();
    for contract in [SEPTEMBER, OCTOBER, NOVEMBER] {
        harness.apply(Command::RegisterContract { contract, event_date: EVENT_DATE }).unwrap();
    }

    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT_B };
    legs[2] = LegSpec { contract: NOVEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_C };
    harness
        .apply(Command::SubmitRequest { requester: REQUESTER, deadline: DEADLINE, legs, n_legs: 3 })
        .unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;

    harness.set_both_clocks(Ts(1_100));
    for (maker, leg, price) in [(ALPHA, 0_u8, FILL_A), (BETA, 1, FILL_B), (GAMMA, 2, FILL_C)] {
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
    expected[2] = ExpectedFill { leg: LegId(2), price: FILL_C };
    harness.apply(Command::AcceptRequest { request, expected, n_legs: 3 }).unwrap();
    (harness, request)
}

fn nonce_of(harness: &TestHarness, request: ReqIdx) -> rfq_core::request::Nonce {
    harness.engine().ledger().request(request).unwrap().state().nonce().expect("Settling")
}

/// Every committed claim on this request, as (owner, amount).
fn committed_claims(harness: &TestHarness, request: ReqIdx) -> Vec<(ResOwner, Amount)> {
    harness
        .engine()
        .ledger()
        .reservations()
        .filter(|(_, claim)| claim.committed_to() == Some(request))
        .map(|(_, claim)| (claim.owner(), claim.amount()))
        .collect()
}

// ═══════════════════════════ (a) Pending → Settled → Escrowed ═══════════════════════════

#[test]
fn a_settled_nonce_moves_the_request_to_escrowed_and_discharges_the_claims() {
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);

    // Submitted is not included. Between the two there is no answer, and the nonce says so.
    harness.submit_pending();
    assert_eq!(harness.custody().status(nonce), TxStatus::Pending);
    assert_eq!(harness.custody().ledger().escrows().count(), 0);

    // Polling a pending nonce moves nothing.
    let before = format!("{:?}", harness.engine().ledger());
    assert_eq!(harness.poll_settlement(request).unwrap(), TxStatus::Pending);
    assert_eq!(format!("{:?}", harness.engine().ledger()), before, "Pending moved state");
    assert_eq!(
        harness.engine().ledger().account(REQUESTER).unwrap().committed(),
        requester_fill(),
        "the claims are still held"
    );

    // The chain includes it, announces the resolution, and the indexer delivers it. Nobody
    // polls: a terminal answer is the chain's to report.
    let included = harness.include_all();
    assert_eq!(included.len(), 1);
    assert!(included[0].outcome.is_ok());
    assert_eq!(included[0].status, TxStatus::Settled);
    assert_eq!(harness.custody().ledger().escrows().count(), 3);
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed
    );

    // The committed capital has left the core's books — it is escrowed now, and escrows are
    // custody's.
    assert!(committed_claims(&harness, request).is_empty());
    for account in [REQUESTER, ALPHA, BETA, GAMMA] {
        let entry = harness.engine().ledger().account(account).unwrap();
        assert_eq!(entry.committed(), Amount::ZERO);
        assert_eq!(entry.reserved(), Amount::ZERO);
    }

    // Which closes the coverage window S3 left knowingly open (§15.6).
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));

    // A poll on a terminal request is refused, and changes nothing. The log entry that
    // already moved it cannot be replayed either — dedup saw it.
    let after = format!("{:?}", harness.engine().ledger());
    assert!(harness.poll_settlement(request).is_err());
    assert_eq!(format!("{:?}", harness.engine().ledger()), after);
    harness.indexer_mut().rewind();
    assert_eq!(harness.pump_indexer(), 0);
    assert_eq!(format!("{:?}", harness.engine().ledger()), after);
}

// ═══════════════════ (b) Pending → Reverted → SettlementFailed ═══════════════════

/// A configuration that admits a non-zero mirror lag.
///
/// With every lag term at zero, insufficient-funds-at-settlement is **unreachable by
/// construction** (§9.3): no participant can withdraw out from under their own live claim,
/// in either ordering. Staleness is what makes it reachable, because staleness is what lets
/// the engine admit against capital already gone — so a venue that wants to exhibit it has
/// to admit it has some, and the timelock widens to cover it.
fn laggy_config() -> Config {
    let indexer_lag = Dur(120_000);
    let base = config();
    let claim_window = Dur(base.max_request_ttl.0 + base.max_settling_time.0);
    Config {
        max_indexer_lag: indexer_lag,
        withdrawal_delay: Dur(claim_window.0 + indexer_lag.0 + 1),
        ..base
    }
}

#[test]
fn a_reverted_nonce_fails_the_settlement_and_returns_every_claim_to_free() {
    // The revert is driven by **clock divergence**, not by missing funds. Venue time and
    // chain time are separate (§9.1), and a quote the engine believes live can be expired at
    // the custody layer — which is the one revert reachable under v1 defaults, since §9.3's
    // inequality makes insufficient-funds unreachable and the log delivers a settlement's
    // balance changes and its resolution together, so no lag can separate them.
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);
    let committed_before = committed_claims(&harness, request);
    assert_eq!(committed_before.len(), 4, "three makers and the requester");

    // The chain runs ahead, past every quote's expiry. The engine is right by its own clock
    // and the chain is right by its own; the transaction is judged by the chain's.
    harness.custody_clock_mut().set(Ts(25_000));
    let divergence = harness.custody_now().saturating_sub(harness.engine_now());
    assert_ne!(divergence, Dur::ZERO, "the two clocks must actually disagree");

    let quotes: Vec<_> = harness.engine().ledger().quotes().map(|(handle, _)| handle).collect();
    harness.submit_pending();
    let included = harness.include_all();
    assert_eq!(included[0].outcome, Err(SettleError::QuoteExpired { leg: 0 }));
    assert_eq!(included[0].status, TxStatus::Reverted);
    assert!(!harness.custody().ledger().nonce_used(nonce), "a revert consumes nothing");
    assert_eq!(harness.custody().ledger().escrows().count(), 0, "no escrow formed");

    // The chain announced the revert and the indexer delivered it.
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::SettlementFailed
    );

    // Every committed claim is back in free capital, both sides.
    assert!(committed_claims(&harness, request).is_empty());
    for account in [REQUESTER, ALPHA, BETA, GAMMA] {
        let entry = harness.engine().ledger().account(account).unwrap();
        assert_eq!(entry.committed(), Amount::ZERO, "account {account:?}");
        assert_eq!(entry.reserved(), Amount::ZERO);
    }

    // Every maker was told their winning quote is unfilled and their capital is back. They
    // are never left inferring the fate of their capital from silence (§7.2).
    let notified: Vec<AccountIdx> = harness
        .emitted()
        .iter()
        .filter_map(|event| match event {
            Event::QuoteRejected { maker, reason: QuoteRejectReason::SettlementFailed, .. } => {
                Some(*maker)
            }
            _ => None,
        })
        .collect();
    assert_eq!(notified.len(), 3);
    for maker in [ALPHA, BETA, GAMMA] {
        assert!(notified.contains(&maker), "{maker:?} was not told");
    }
    assert!(quotes.iter().all(|handle| harness.engine().ledger().quote(*handle).is_none()));

    // Nobody lost anything: the basket aborted whole and every balance is where it started.
    assert_eq!(harness.custody().ledger().balance(ALPHA), maker_contribution(FILL_A));
    assert_eq!(harness.custody().ledger().balance(BETA), maker_contribution(FILL_B));
    assert_eq!(harness.custody().ledger().balance(GAMMA), maker_contribution(FILL_C));
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));

    // And it is terminal: the request cannot be re-accepted.
    assert!(harness.poll_settlement(request).is_err());
}

#[test]
fn insufficient_funds_at_settlement_is_unreachable_without_lag() {
    // §9.3's consequence, and the reason the revert above is driven by clock divergence
    // rather than by missing funds. Under v1 defaults no participant can withdraw out from
    // under their own live claim: claim-then-withdraw leaves the claim strictly dead first,
    // and withdraw-then-claim drops availability before the claim is admitted, so only what
    // the remainder covers is ever promised.
    let config = config();
    let maker_window = config.max_quote_ttl;
    let requester_window = Dur(config.max_request_ttl.0 + config.max_settling_time.0);
    for window in [maker_window, requester_window] {
        assert!(
            config.withdrawal_delay > window,
            "a claim binding for {window:?} outlives a withdrawal landing after {:?}",
            config.withdrawal_delay
        );
    }
    assert_eq!(config.max_indexer_lag, Dur::ZERO);
    assert_eq!(config.confirmations, 0);
    assert_eq!(config.max_settlement_inclusion_time, Dur::ZERO);

    // And with nothing stale, the settlement finds every contribution exactly where the
    // engine promised it would be.
    let (mut harness, _request) = accepted_market(config);
    harness.submit_pending();
    let included = harness.include_all();
    assert!(included[0].outcome.is_ok(), "with no lag there is nothing to be stale about");
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

// ═══════════════════ (c) Unknown for N polls, then Settled ═══════════════════

#[test]
fn an_unknown_nonce_holds_every_claim_and_commits_nothing_twice() {
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);
    let committed_before = committed_claims(&harness, request);
    assert_eq!(committed_before.len(), 4, "three makers and the requester");

    // The send never arrived. `Unknown` is not an answer and must never be treated as one.
    harness.submit_pending();
    assert!(harness.lose_next_submission());
    assert_eq!(harness.custody().status(nonce), TxStatus::Unknown);

    for _ in 0..5 {
        assert_eq!(harness.poll_settlement(request).unwrap(), TxStatus::Unknown);
        assert!(matches!(
            harness.engine().ledger().request(request).unwrap().state(),
            RequestState::Settling { .. }
        ));
        // Every claim, unchanged, poll after poll. Releasing here is the duplication path:
        // the maker requotes the same capital and the original transaction lands.
        assert_eq!(committed_claims(&harness, request), committed_before);
        // §15.3 holds *throughout* Settling, not only at its endpoints (gate f).
        assert_eq!(harness.engine().ledger().check_invariants(), Ok(()));
    }

    // The retry is what turns "unknown" from a catastrophe into a delay.
    harness.resubmit(request).unwrap();
    let included = harness.include_all();
    assert!(included[0].outcome.is_ok());
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed
    );
    assert_eq!(harness.custody().ledger().escrows().count(), 3, "committed exactly once");
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

// ═══════════════════ (d) the deadline with the nonce unconsumed ═══════════════════

#[test]
fn reaching_the_settling_deadline_alerts_and_releases_nothing() {
    // Stuck-but-consistent beats fast-but-wrong when the alternative is losing money (§8.3).
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);
    let committed_before = committed_claims(&harness, request);
    harness.submit_pending();
    assert!(harness.lose_next_submission());

    // Before the deadline: no alert, nothing moves.
    assert_eq!(harness.poll_settlement(request).unwrap(), TxStatus::Unknown);
    assert!(!harness.emitted().iter().any(|e| matches!(e, Event::SettlementStalled { .. })));

    harness.set_both_clocks(Ts(1_200).checked_add(SETTLING_TIME).unwrap());
    assert_eq!(harness.poll_settlement(request).unwrap(), TxStatus::Unknown);

    let alerts: Vec<_> = harness
        .emitted()
        .iter()
        .filter_map(|event| match event {
            Event::SettlementStalled { nonce, status, .. } => Some((*nonce, *status)),
            _ => None,
        })
        .collect();
    assert_eq!(alerts, vec![(nonce, TxStatus::Unknown)], "an alert, not a state change");

    // The request is still Settling and every claim is still held. The nonce is unconsumed,
    // so the transaction may yet land.
    assert!(matches!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Settling { .. }
    ));
    assert_eq!(committed_claims(&harness, request), committed_before);
    assert!(!harness.custody().ledger().nonce_used(nonce));
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));

    // And it still settles afterwards, which is what makes holding the right answer.
    harness.resubmit(request).unwrap();
    harness.include_all();
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed
    );
    assert_eq!(harness.custody().ledger().escrows().count(), 3);
}

// ═══════════════════ (e) submit, lose the ack, resubmit ═══════════════════

#[test]
fn a_retry_before_inclusion_is_applied_exactly_once() {
    // The easy half: the original is still in the queue, so the retry queues behind it and
    // the second inclusion bounces off the nonce the first consumed.
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);

    harness.submit_pending();
    harness.resubmit(request).unwrap(); // the ack was lost; the engine tries again
    assert_eq!(harness.custody().status(nonce), TxStatus::Pending);

    let included = harness.include_all();
    assert_eq!(included.len(), 2, "both submissions were included");
    assert!(included[0].outcome.is_ok());
    assert_eq!(included[1].outcome, Err(SettleError::NonceReused));
    assert_eq!(included[1].status, TxStatus::Settled, "the nonce did not change its mind");

    assert_eq!(harness.custody().ledger().escrows().count(), 3, "applied exactly once");
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed,
        "and the engine was told once, by the chain"
    );
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

#[test]
fn a_retry_after_inclusion_reverts_on_its_own_nonce_and_that_is_not_a_failure() {
    // The half that matters, and the one a before-inclusion test cannot reach.
    //
    // The engine submits, the transaction *is included*, and the acknowledgement is lost.
    // The engine correctly retries — that is what an idempotent nonce is for. The chain
    // refuses the retry because the nonce is already consumed. A naive implementation reports
    // `Reverted`, the engine concludes the settlement failed and releases the committed
    // claims, for a settlement that actually succeeded.
    //
    // Every component told the truth. The retry genuinely did revert. What makes it a
    // catastrophe is reading `Reverted` as a property of the submission rather than of the
    // nonce — and conservation cannot detect the result, because each layer stays internally
    // consistent while the model comes apart.
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);

    harness.submit_pending();
    let first = harness.include_all();
    assert!(first[0].outcome.is_ok());
    assert_eq!(harness.custody().status(nonce), TxStatus::Settled);
    let escrows_after_first = harness.custody().ledger().escrows().count();
    assert_eq!(escrows_after_first, 3);

    // The ack was lost, so the engine retries an already-included transaction.
    harness.resubmit(request).unwrap();
    let retry = harness.include_all();
    assert_eq!(retry.len(), 1);
    assert_eq!(
        retry[0].outcome,
        Err(SettleError::NonceReused),
        "the submission genuinely reverted"
    );
    assert_eq!(
        retry[0].status,
        TxStatus::Settled,
        "and the nonce genuinely settled — only one of those is about the trade"
    );
    assert_eq!(harness.custody().status(nonce), TxStatus::Settled);

    // The chain announced the nonce's fate, not the submission's, so the engine was told
    // the truth — and the retry's own revert announced nothing at all.
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed,
        "reading the retry's revert as failure would have released capital that is escrowed"
    );

    // Applied exactly once, and both layers still agree about who owns what.
    assert_eq!(harness.custody().ledger().escrows().count(), escrows_after_first);
    assert!(committed_claims(&harness, request).is_empty());
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

#[test]
fn a_retry_after_a_revert_finds_reverted_and_a_retry_after_a_settle_finds_settled() {
    // `NonceReused` is one cause — the nonce is spent — with two readings, and the reading
    // lives in the status beside it rather than in a second error variant. Both directions
    // are asserted here, because "the status separates them" claimed in one direction only
    // is half a claim.
    let laggy = Config { max_requests: 4, ..laggy_config() };

    // Direction one: the nonce settled. The retry reverts and the nonce says Settled.
    let (mut harness, request) = accepted_market(laggy);
    harness.submit_pending();
    harness.include_all();
    harness.resubmit(request).unwrap();
    let retry = harness.include_all();
    assert_eq!(retry[0].outcome, Err(SettleError::NonceReused));
    assert_eq!(retry[0].status, TxStatus::Settled);

    // Direction two: the nonce reverted. The retry reverts too, and the nonce says Reverted —
    // a different fact about the trade, reached through an identical-looking error.
    let (mut harness, second) = accepted_market(config());
    harness.custody_clock_mut().set(Ts(25_000));
    harness.submit_pending();
    let included = harness.include_all();
    assert_eq!(included[0].status, TxStatus::Reverted, "the precondition this needs");

    harness.resubmit(second).unwrap();
    let retry = harness.include_all();
    assert_eq!(
        retry[0].outcome,
        Err(SettleError::NonceReused),
        "the same error as a retry of a success"
    );
    assert_eq!(
        retry[0].status,
        TxStatus::Reverted,
        "and a different answer, which is where the difference belongs"
    );
    // And the retry did not re-arm it: a reverted nonce is terminal, so no escrow forms.
    assert_eq!(harness.custody().ledger().escrows().count(), 0);
}

#[test]
fn a_nonce_whose_generation_has_moved_on_names_nothing() {
    // `(ReqIdx, req_generation)` is the nonce (§8.1), and the generation is not decoration.
    // `PollSettlement` may be sent by anyone, so anyone can name a slot with the wrong
    // generation — and that must resolve to nothing rather than to whatever occupies the
    // slot now. Without the check, a nonce from a freed request would move its successor.
    let (mut harness, request) = accepted_market(config());
    let nonce = nonce_of(&harness, request);
    let before = format!("{:?}", harness.engine().ledger());

    for generation in [nonce.generation.wrapping_add(1), nonce.generation.wrapping_sub(1), 99] {
        let forged = rfq_core::request::Nonce { request: nonce.request, generation };
        assert_eq!(
            harness.apply(Command::PollSettlement { nonce: forged, status: TxStatus::Settled }),
            Err(HarnessError::Engine(EngineError::StaleNonce)),
            "generation {generation} must name nothing"
        );
    }
    // A settled answer for a nonce that does not exist moved nothing: the request is still
    // Settling and every claim is still held.
    assert_eq!(format!("{:?}", harness.engine().ledger()), before);
    assert!(matches!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Settling { .. }
    ));

    // A slot that has never been occupied names nothing either.
    assert_eq!(
        harness.apply(Command::PollSettlement {
            nonce: rfq_core::request::Nonce { request: 99, generation: 0 },
            status: TxStatus::Settled,
        }),
        Err(HarnessError::Engine(EngineError::StaleNonce))
    );

    // And the real nonce still works, so the check is discriminating rather than blanket.
    harness.submit_pending();
    harness.include_all();
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed
    );
}

// ═══════════════════ (f) invariant 3 throughout Settling ═══════════════════

#[test]
fn a_committed_claim_names_a_consumed_quote_at_every_step_of_settling() {
    // §15.3 through the whole of `Settling`, not only at its endpoints: a committed entry
    // references exactly one `Consumed` quote on a request in `Settling`, or that request
    // itself (§2.4). `committed` capital legally references a quote that is no longer
    // standing, which is precisely why the reserved form of the invariant would be wrong here.
    let (mut harness, request) = accepted_market(config());

    let check = |harness: &TestHarness| {
        assert_eq!(harness.engine().ledger().check_invariants(), Ok(()));
        let claims = committed_claims(harness, request);
        assert_eq!(claims.len(), 4);
        let mut makers = 0;
        for (owner, amount) in claims {
            assert_ne!(amount, Amount::ZERO);
            match owner {
                ResOwner::Quote(quote) => {
                    let record = harness.engine().ledger().quote(quote).expect("must resolve");
                    assert_eq!(record.state(), QuoteState::Consumed);
                    assert_eq!(record.request(), request);
                    makers += 1;
                }
                ResOwner::Request(owner) => assert_eq!(owner, request),
            }
        }
        assert_eq!(makers, 3, "one Consumed quote per leg");
        assert!(matches!(
            harness.engine().ledger().request(request).unwrap().state(),
            RequestState::Settling { .. }
        ));
    };

    check(&harness);
    harness.submit_pending();
    check(&harness);
    harness.poll_settlement(request).unwrap();
    check(&harness);
    harness.set_both_clocks(Ts(1_200).checked_add(SETTLING_TIME).unwrap());
    harness.poll_settlement(request).unwrap();
    check(&harness);

    // Only the chain's terminal answer ends it.
    harness.include_all();
    assert_eq!(
        harness.engine().ledger().request(request).unwrap().state(),
        RequestState::Escrowed
    );
    assert_eq!(harness.engine().ledger().check_invariants(), Ok(()));
}
