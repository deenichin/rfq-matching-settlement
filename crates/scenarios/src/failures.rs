//! Three failure and recovery paths.

#![allow(clippy::print_stdout)]

use rfq_chain::custody::SettleError;
use rfq_core::account::AccountIdx;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::MAX_LEGS;
use rfq_core::contract::{ContractIdx, Outcome};
use rfq_core::event::Event;
use rfq_core::quote::QuoteState;
use rfq_core::request::{ReqIdx, RequestState};
use rfq_core::settlement::TxStatus;
use rfq_core::types::{Amount, Dur, LegId, Price, Side, Ts};

use crate::happy::{ALPHA, BETA, DELTA, GAMMA, REQUESTER, SEPTEMBER, SIZE, START};
use crate::stage::{Stage, note, seconds_after, section};
use crate::trace::{self, Cast};
use crate::{contract_config, register_contracts};

const OCTOBER: ContractIdx = ContractIdx(1);
const NOVEMBER: ContractIdx = ContractIdx(2);
const LIMIT: Price = Price(650_000);
const FILL_A: Price = Price(610_000);
const FILL_C: Price = Price(380_000);

fn requester_pays(price: Price) -> Amount {
    SIZE.requester_contribution(price).expect("no overflow")
}
fn maker_pays(price: Price) -> Amount {
    SIZE.maker_contribution(price).expect("no overflow")
}

fn cast() -> Cast {
    Cast::new(vec![
        (REQUESTER, "requester"),
        (ALPHA, "Alpha"),
        (BETA, "Beta"),
        (GAMMA, "Gamma"),
        (DELTA, "Delta"),
    ])
}

// ═══════════════════════════ 2. leg 2 of 3 fails ═══════════════════════════

/// The request aborts whole. Leg 1 and leg 3's quotes are untouched and their makers are
/// never told they nearly traded.
///
/// # Panics
///
/// If the abort is not clean.
pub(crate) fn leg_two_of_three_fails() {
    let mut stage = Stage::open(
        "2. leg 2 of 3 has no eligible quote — the whole request aborts",
        "An abort path, and it ends where the abort does: no capital is committed, the other\n\
         two legs' quotes are still standing, and nobody is notified. \"Provisionally\n\
         matched\" was a local variable inside the plan phase and never left it.",
        contract_config(),
        cast(),
        START,
    );

    section("funding and the request");
    let reservation = Amount(requester_pays(LIMIT).0 * 3);
    let _deposited = Amount(reservation.0 + maker_pays(FILL_A).0 + maker_pays(FILL_C).0);
    stage.fund(REQUESTER, reservation);
    stage.fund(ALPHA, maker_pays(FILL_A));
    stage.fund(GAMMA, maker_pays(FILL_C));
    register_contracts(&mut stage, &[SEPTEMBER, OCTOBER, NOVEMBER]);

    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT };
    legs[2] = LegSpec { contract: NOVEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    stage
        .apply(
            "SubmitRequest  three legs, mixed sides",
            Command::SubmitRequest {
                requester: REQUESTER,
                deadline: seconds_after(START, 60),
                legs,
                n_legs: 3,
            },
        )
        .expect("admissible");
    let request = stage.harness().engine().ledger().requests().next().expect("exists").0;

    section("legs 1 and 3 are quoted; leg 2 is not");
    stage.at(seconds_after(START, 1));
    for (maker, leg, price) in [(ALPHA, 0_u8, FILL_A), (GAMMA, 2, FILL_C)] {
        stage
            .apply(
                &format!("SubmitQuote  leg {leg} @ {}", trace::price(price)),
                Command::SubmitQuote {
                    maker,
                    request,
                    leg: LegId(leg),
                    price,
                    size: SIZE,
                    expires_at: seconds_after(START, 30),
                },
            )
            .expect("admissible");
    }

    section("acceptance — and the abort");
    stage.at(seconds_after(START, 2));
    let before = format!("{:?}", stage.harness().engine().ledger());
    let events_before = stage.harness().emitted().len();
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    expected[1] = ExpectedFill { leg: LegId(1), price: LIMIT };
    expected[2] = ExpectedFill { leg: LegId(2), price: FILL_C };
    let outcome =
        stage.apply("AcceptRequest", Command::AcceptRequest { request, expected, n_legs: 3 });
    assert!(outcome.is_err(), "leg 2 has no quote at all");
    note("NoEligibleQuote{leg: 1, reason: NoQuotes} — the plan loop aborted before any mutation");

    assert_eq!(
        format!("{:?}", stage.harness().engine().ledger()),
        before,
        "byte-identical to the post-normalisation state"
    );
    assert_eq!(stage.harness().emitted().len(), events_before, "no maker was notified");
    for (_, quote) in stage.harness().engine().ledger().quotes() {
        assert_eq!(quote.state(), QuoteState::Active, "legs 1 and 3's quotes still stand");
    }
    assert_eq!(
        stage.harness().engine().ledger().request(request).expect("exists").state(),
        RequestState::Open,
        "the request is still Open — a later accept may succeed if a quote arrives"
    );
    for account in [REQUESTER, ALPHA, GAMMA] {
        assert_eq!(
            stage.harness().engine().ledger().account(account).expect("account").committed(),
            Amount::ZERO,
            "no capital was committed"
        );
    }
    stage.balances("after the abort — every claim is exactly where it was");
    stage.check();
    stage.close();
}

// ═══════════════════ 3. settlement reverts mid-flight ═══════════════════

/// A withdrawal lands between the pre-check and the settle. The pre-check passes, the
/// settlement reverts wholesale, and every claim comes back.
///
/// # Panics
///
/// If the revert is not clean.
pub(crate) fn settlement_reverts_mid_flight() {
    // The requester's withdrawal has to mature while the winning quotes are still live, and
    // §9.3's inequality makes that unreachable unless the engine's view is stale — so this
    // venue declares an indexer lag, and the timelock widens to cover it.
    let base = contract_config();
    let indexer_lag = Dur(400_000);
    let claim_window = Dur(base.max_request_ttl.0 + base.max_settling_time.0);
    let config = rfq_core::config::Config {
        max_indexer_lag: indexer_lag,
        withdrawal_delay: Dur(claim_window.0 + indexer_lag.0 + 1),
        ..base
    };

    let mut stage = Stage::open(
        "3. a withdrawal lands between the pre-check and the settle",
        "The pre-check is an optimisation with no correctness role: checking then submitting\n\
         is TOCTOU, and the window between them is exactly where a withdrawal lands. The\n\
         authoritative validation is inside the transaction, and it reverts the basket whole.",
        config,
        cast(),
        START,
    );

    section("funding");
    let reservation = requester_pays(LIMIT);
    let _deposited = Amount(reservation.0 + maker_pays(FILL_A).0);
    stage.fund(REQUESTER, reservation);
    stage.fund(ALPHA, maker_pays(FILL_A));
    register_contracts(&mut stage, &[SEPTEMBER]);

    section("the requester asks for their money back, long before trading");
    stage.harness_mut().stall_indexer();
    let matures_at = stage
        .harness_mut()
        .request_withdrawal(REQUESTER, reservation)
        .expect("the funds are available");
    note(&format!(
        "availability drops to nothing at once; the balance stays present until {} — that \
         gap is the timelock, and it is what makes soft reservation safe",
        trace::at(matures_at)
    ));
    note("the indexer is behind, so the engine has not heard — which is the only reason what follows is reachable");
    note("§9.3's inequality makes this unreachable at zero lag: no participant can withdraw out from under their own live claim");

    section("the request, opened late enough to still be live when the withdrawal lands");
    stage.at(Ts(matures_at.0 - 50_000));
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    stage
        .apply(
            "SubmitRequest  one leg — admitted against a stale mirror",
            Command::SubmitRequest {
                requester: REQUESTER,
                deadline: Ts(matures_at.0 + 10_000),
                legs,
                n_legs: 1,
            },
        )
        .expect("the engine still believes the money is there");
    let request = stage.harness().engine().ledger().requests().next().expect("exists").0;

    section("quoting and acceptance, still before maturity");
    stage.at(Ts(matures_at.0 - 20_000));
    stage
        .apply(
            "SubmitQuote  leg 0 @ 0.61",
            Command::SubmitQuote {
                maker: ALPHA,
                request,
                leg: LegId(0),
                price: FILL_A,
                size: SIZE,
                expires_at: Ts(matures_at.0 + 10_000),
            },
        )
        .expect("admissible");
    stage.at(Ts(matures_at.0 - 10_000));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    stage
        .apply("AcceptRequest", Command::AcceptRequest { request, expected, n_legs: 1 })
        .expect("the leg has an eligible quote");

    section("the pre-check, then the settle");
    let bundle = stage.harness().pending_bundles()[0];
    assert_eq!(
        stage.harness().custody().precheck(&bundle),
        Ok(()),
        "the money is still present, so the pre-check is happy"
    );
    note("pre-check: ok — every contribution is present at this instant");

    stage.at(matures_at);
    stage.harness_mut().custody_mut().on_settle_entry(Box::new(|ledger, now| {
        ledger.execute_matured_withdrawals(now);
    }));
    note("the withdrawal matures and lands inside the transaction, after entry and before validation");

    let outcomes = stage.settle();
    assert_eq!(
        outcomes[0],
        Err(SettleError::InsufficientFunds { account: REQUESTER }),
        "settle revalidates rather than trusting the pre-check"
    );
    assert_eq!(stage.harness().locked_escrows().count(), 0, "not one leg settled");
    assert_eq!(
        stage.harness().custody().ledger().balance(ALPHA),
        maker_pays(FILL_A),
        "and the maker was not debited"
    );

    section("the engine learns, and every claim comes back");
    stage.resume_indexer();
    assert_eq!(
        stage.harness().engine().ledger().request(request).expect("exists").state(),
        RequestState::SettlementFailed
    );
    for account in [REQUESTER, ALPHA] {
        assert_eq!(
            stage.harness().engine().ledger().account(account).expect("account").committed(),
            Amount::ZERO,
            "committed capital returned to free"
        );
    }
    let told: Vec<AccountIdx> = stage
        .harness()
        .emitted()
        .iter()
        .filter_map(|event| match event {
            Event::QuoteRejected {
                maker,
                reason: rfq_core::event::QuoteRejectReason::SettlementFailed,
                ..
            } => Some(*maker),
            _ => None,
        })
        .collect();
    assert_eq!(told, vec![ALPHA], "the maker was told their winning quote is unfilled");

    stage.balances("final — the requester has their withdrawal, the maker their stake");
    stage.check();
    stage.close();
}

// ═══════════════════ 4. the lost acknowledgement ═══════════════════

/// Submit, lose the acknowledgement, resubmit the same nonce **after** the original was
/// already included. Applied exactly once, and the retry's revert is not settlement failure.
///
/// # Panics
///
/// If the trade is applied more than once, or the retry's revert is misread.
pub(crate) fn lost_acknowledgement() {
    let mut stage = Stage::open(
        "4. the acknowledgement is lost, and the retry finds its own success",
        "The engine submits, the transaction IS included, and the acknowledgement is lost.\n\
         The engine correctly retries — that is what an idempotent nonce is for. The chain\n\
         refuses the retry because the nonce is consumed, and a naive implementation reports\n\
         Reverted and releases capital for a settlement that succeeded. Every component told\n\
         the truth; the trap is reading Reverted as a property of the submission rather than\n\
         of the nonce.",
        contract_config(),
        cast(),
        START,
    );

    section("funding, request, quote, acceptance");
    let reservation = requester_pays(LIMIT);
    let _deposited = Amount(reservation.0 + maker_pays(FILL_A).0);
    stage.fund(REQUESTER, reservation);
    stage.fund(ALPHA, maker_pays(FILL_A));
    register_contracts(&mut stage, &[SEPTEMBER]);
    let request = one_leg_market(&mut stage);

    section("submission — and the acknowledgement never arrives");
    stage.harness_mut().submit_pending();
    note("submitted. The submitter keeps the bundle, because that is the only way to ask again.");
    let nonce = stage
        .harness()
        .engine()
        .ledger()
        .request(request)
        .expect("exists")
        .state()
        .nonce()
        .expect("Settling");
    assert_eq!(stage.harness().custody().status(nonce), TxStatus::Pending);

    let included = stage.harness_mut().include_all();
    println!("      the chain includes it: {:?}", included[0].status);
    assert!(included[0].outcome.is_ok(), "the original was included and applied");
    let escrows_after_first = stage.harness().locked_escrows().count();
    assert_eq!(escrows_after_first, 1);
    note("included, one escrow formed — but the submitter never heard, so it retries");

    section("the retry, after inclusion");
    stage.harness_mut().resubmit(request).expect("the submitter kept what it sent");
    println!("      resubmitted — byte-identical bundle, same nonce");
    let retry = stage.harness_mut().include_all();
    println!(
        "      the chain includes the retry: outcome {:?}, and the NONCE says {:?}",
        retry[0].outcome, retry[0].status
    );
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
    note("the retry reverted on its own consumed nonce, and the nonce still says Settled");
    note("a retry bouncing off its own nonce is evidence the original succeeded");

    assert_eq!(
        stage.harness().locked_escrows().count(),
        escrows_after_first,
        "applied exactly once — no second escrow"
    );
    assert_eq!(
        stage.harness().engine().ledger().request(request).expect("exists").state(),
        RequestState::Escrowed,
        "reading the retry's revert as failure would have released capital that is escrowed"
    );
    stage.check();

    section("resolution and payout");
    stage.at(seconds_after(START, 20));
    stage.harness_mut().oracle_mut().propose(SEPTEMBER, Outcome::Yes).expect("first proposal");
    stage.at(seconds_after(START, 20_000));
    stage.harness_mut().oracle_mut().finalise(SEPTEMBER).expect("window closed");
    stage.harness_mut().report_oracle_status(SEPTEMBER).expect("first Final");
    let before = stage.harness().custody().ledger().balance(REQUESTER);
    let escrow = stage.harness().escrows()[0];
    stage.settle_escrow(escrow, SEPTEMBER, Outcome::Yes);
    assert_eq!(
        stage.harness().custody().ledger().balance(REQUESTER),
        Amount(before.0 + SIZE.notional().expect("no overflow").0),
        "the requester bought Yes and Yes happened"
    );

    stage.balances("final");
    stage.check();
    stage.close();
}

/// Fund, request, quote and accept a single `Yes` leg on September.
fn one_leg_market(stage: &mut Stage) -> ReqIdx {
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    stage
        .apply(
            "SubmitRequest  one leg, September Yes",
            Command::SubmitRequest {
                requester: REQUESTER,
                deadline: seconds_after(START, 60),
                legs,
                n_legs: 1,
            },
        )
        .expect("admissible");
    let request = stage.harness().engine().ledger().requests().next().expect("exists").0;
    stage.at(seconds_after(START, 1));
    stage
        .apply(
            "SubmitQuote  leg 0 @ 0.61",
            Command::SubmitQuote {
                maker: ALPHA,
                request,
                leg: LegId(0),
                price: FILL_A,
                size: SIZE,
                expires_at: seconds_after(START, 30),
            },
        )
        .expect("admissible");
    stage.at(seconds_after(START, 2));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL_A };
    stage
        .apply("AcceptRequest", Command::AcceptRequest { request, expected, n_legs: 1 })
        .expect("the leg has an eligible quote");
    request
}

/// Shared by the resolution scenarios.
pub(crate) fn one_leg_escrowed(stage: &mut Stage) -> (ReqIdx, rfq_core::escrow::EscrowId) {
    let request = one_leg_market(stage);
    let outcomes = stage.settle();
    assert!(outcomes[0].is_ok(), "every contribution is present");
    let escrow = stage.harness().escrows()[0];
    (request, escrow)
}

/// What the resolution scenarios fund.
#[must_use]
pub(crate) fn one_leg_funding() -> (Amount, Amount) {
    (requester_pays(LIMIT), maker_pays(FILL_A))
}
