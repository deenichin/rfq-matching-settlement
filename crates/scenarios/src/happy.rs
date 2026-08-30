//! The happy path: SPEC Appendix A, exactly.
//!
//! One maker per leg would mean selection never selects and the trace demonstrates nothing.
//! This is the calendar spread — long September, short October — with four makers, and the
//! auction exercises competition, the arrival-order tiebreak, over-limit admission, a best
//! quote expiring mid-auction, a replacement that changes the winner, losing release,
//! over-reservation release, and both directions of the payout mapping in one settlement.
//!
//! Every figure below is Appendix A's, in minor units: the appendix quotes USDC, and
//! `UNIT = 1_000_000` minor units per contract, so its 65,000 is 65,000,000,000 here.

#![allow(clippy::print_stdout)]

use rfq_core::account::AccountIdx;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::MAX_LEGS;
use rfq_core::contract::{ContractIdx, Outcome};
use rfq_core::event::Event;
use rfq_core::types::{Amount, LegId, Price, Side, Size, Ts};

use crate::stage::{Stage, note, seconds_after, section};
use crate::trace::{self, Cast};
use crate::{contract_config, register_contracts};

pub(crate) const REQUESTER: AccountIdx = AccountIdx(0);
pub(crate) const ALPHA: AccountIdx = AccountIdx(1);
pub(crate) const BETA: AccountIdx = AccountIdx(2);
pub(crate) const GAMMA: AccountIdx = AccountIdx(3);
pub(crate) const DELTA: AccountIdx = AccountIdx(4);

pub(crate) const SEPTEMBER: ContractIdx = ContractIdx(0);
pub(crate) const OCTOBER: ContractIdx = ContractIdx(1);

/// 100,000 contracts on each leg.
pub(crate) const SIZE: Size = Size(100_000);
/// Leg A limit 0.65, leg B limit 0.50.
const LIMIT_A: Price = Price(650_000);
const LIMIT_B: Price = Price(500_000);

pub(crate) const START: Ts = Ts(1_000);

fn requester_pays(price: Price) -> Amount {
    SIZE.requester_contribution(price).expect("no overflow at these sizes")
}
fn maker_pays(price: Price) -> Amount {
    SIZE.maker_contribution(price).expect("no overflow at these sizes")
}
fn notional() -> Amount {
    SIZE.notional().expect("no overflow at these sizes")
}

/// Run it.
///
/// # Panics
///
/// If any figure departs from Appendix A, or any invariant breaks.
pub(crate) fn run() {
    let cast = Cast::new(vec![
        (REQUESTER, "requester"),
        (ALPHA, "Alpha"),
        (BETA, "Beta"),
        (GAMMA, "Gamma"),
        (DELTA, "Delta"),
    ]);
    let mut stage = Stage::open(
        "1. happy path — the September/October calendar spread (SPEC Appendix A)",
        "Four makers, two legs, mixed sides. Competition on leg A, an arrival-order tiebreak,\n\
         an over-limit quote admitted and never published, the best leg-B quote expiring\n\
         mid-auction, a replacement that changes the winner, and both payout directions in\n\
         one settlement.",
        contract_config(),
        cast,
        START,
    );

    // ── funding: exactly what each account will need at its peak, and no more ──
    // Slack absorbs a claim released twice or held once too often, and every coverage
    // assertion below would then pass regardless of correctness (CLAUDE 38).
    section("funding — each account to exactly its peak requirement");
    let requester_reservation = Amount(requester_pays(LIMIT_A).0 + requester_pays(LIMIT_B).0);
    // Alpha holds A@0.62 throughout and B@0.48 then B@0.44; the replacement releases before
    // it reserves, so the peak is 0.62's claim plus 0.44's.
    let alpha_peak = Amount(maker_pays(Price(620_000)).0 + maker_pays(Price(440_000)).0);
    let beta_peak = Amount(maker_pays(Price(610_000)).0 + maker_pays(Price(430_000)).0);
    let gamma_peak = Amount(maker_pays(Price(610_000)).0 + maker_pays(Price(450_000)).0);
    let delta_peak = maker_pays(Price(700_000));
    stage.fund(REQUESTER, requester_reservation);
    stage.fund(ALPHA, alpha_peak);
    stage.fund(BETA, beta_peak);
    stage.fund(GAMMA, gamma_peak);
    stage.fund(DELTA, delta_peak);
    register_contracts(&mut stage, &[SEPTEMBER, OCTOBER]);

    // ── the request ──
    section("the request — Σ size × limit reserved before any price exists");
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT_B };
    let deadline = seconds_after(START, 60);
    stage
        .apply(
            "SubmitRequest  A: September Yes @≤0.65 · B: October No @≤0.50",
            Command::SubmitRequest { requester: REQUESTER, deadline, legs, n_legs: 2 },
        )
        .expect("the request is admissible");
    let request = stage
        .harness()
        .engine()
        .ledger()
        .requests()
        .next()
        .map(|(handle, _)| handle)
        .expect("the request exists");
    assert_eq!(
        stage.harness().engine().ledger().account(REQUESTER).expect("account").reserved(),
        requester_reservation,
        "Appendix A: 65,000 + 50,000 = 115,000 reserved"
    );
    note(&format!(
        "reserved {} — Appendix A's 115,000 USDC, in minor units",
        trace::minor_units(requester_reservation)
    ));

    // ── the auction ──
    section("the auction");
    let quote = |maker, leg: u8, price: u32, expires: Ts| Command::SubmitQuote {
        maker,
        request,
        leg: LegId(leg),
        price: Price(price),
        size: SIZE,
        expires_at: expires,
    };
    let long_lived = seconds_after(START, 30);

    stage.at(seconds_after(START, 1));
    stage.apply("+1s  Alpha quotes A @ 0.62", quote(ALPHA, 0, 620_000, long_lived)).expect("ok");

    stage.at(seconds_after(START, 2));
    stage.apply("+2s  Beta quotes A @ 0.61  — new best", quote(BETA, 0, 610_000, long_lived)).expect("ok");
    stage
        .apply(
            "+2s  Gamma quotes A @ 0.61  — same price, later arrival",
            quote(GAMMA, 0, 610_000, long_lived),
        )
        .expect("ok");
    note("no BestSelectionChanged for Gamma: the tiebreak is arrival order, and Beta was first");

    stage.at(seconds_after(START, 3));
    stage
        .apply(
            "+3s  Delta quotes A @ 0.70  — ABOVE the 0.65 limit",
            quote(DELTA, 0, 700_000, long_lived),
        )
        .expect("admitted: there is no price-based rejection anywhere");
    assert_eq!(
        stage.harness().engine().ledger().account(DELTA).expect("account").reserved(),
        delta_peak,
        "an over-limit quote reserves capital like any other"
    );
    note(&format!(
        "admitted and reserving {} — never published, never eligible. Rejecting on price \
         would hand a maker a free oracle for the hidden limit",
        trace::minor_units(delta_peak)
    ));
    assert!(
        stage.harness().emitted().iter().all(|event| !matches!(
            event,
            Event::BestSelectionChanged { price, leg, .. }
                if leg.0 == 0 && *price > LIMIT_A
        )),
        "an ineligible quote is never selected and never published"
    );
    stage.apply("+3s  Gamma quotes B @ 0.45", quote(GAMMA, 1, 450_000, long_lived)).expect("ok");

    stage.at(seconds_after(START, 4));
    let beta_b_dies = seconds_after(START, 6);
    stage
        .apply("+4s  Beta quotes B @ 0.43, TTL 2s  — best, but dies at +6s", quote(BETA, 1, 430_000, beta_b_dies))
        .expect("ok");

    stage.at(seconds_after(START, 5));
    stage.apply("+5s  Alpha quotes B @ 0.48", quote(ALPHA, 1, 480_000, long_lived)).expect("ok");

    // Expiry publishes nothing: normalisation emits no events, so the requester's view of
    // leg B still names Beta's dead quote until the next command touches the request.
    let published_before = published_count(&stage);
    stage.at(seconds_after(START, 6));
    note("+6s  Beta's leg-B quote expires — and nothing is published, by design");
    assert_eq!(published_before, published_count(&stage), "expiry emits nothing (§4.3)");
    assert_eq!(
        stage
            .harness()
            .engine()
            .ledger()
            .request(request)
            .and_then(|record| record.leg(LegId(1)))
            .and_then(rfq_core::request::Leg::published),
        Some(Price(430_000)),
        "between the expiry and the next command on this leg, the requester's view still \
         names Beta's dead quote"
    );
    note("the published view still reads 0.43, a quote that no longer exists: normalisation is silent, so the feed is stale until the next command touches the leg — which is safe only because the accept re-derives the best from live quotes and binds at-or-better");

    stage.at(seconds_after(START, 7));
    let slots_before = stage.harness().engine().ledger().quote_count();
    stage
        .apply("+7s  Alpha REPLACES B @ 0.44  — an improvement, so permitted", quote(ALPHA, 1, 440_000, long_lived))
        .expect("a replacement that improves is admissible");
    assert_eq!(
        stage.harness().engine().ledger().quote_count(),
        slots_before,
        "one slab slot per maker per leg, however many times they requote"
    );
    note("the old claim was released in the same command; occupancy is unchanged");


    note(&format!(
        "note Beta below: still {} reserved, though its leg-B quote died at +6s. Nothing has \
         touched Beta's account since, and there is no sweeper — the stored total is the sum of \
         the account's chain, not of its live claims, and it is reconciled by the next command \
         that needs Beta's capital",
        trace::minor_units(
            stage.harness().engine().ledger().account(BETA).expect("account").reserved()
        )
    ));
    stage.balances("after the auction");

    // ── acceptance ──
    section("acceptance at +8s");
    stage.at(seconds_after(START, 8));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: Price(610_000) };
    expected[1] = ExpectedFill { leg: LegId(1), price: Price(440_000) };
    stage
        .apply(
            "AcceptRequest  expecting A @ 0.61 · B @ 0.44",
            Command::AcceptRequest { request, expected, n_legs: 2 },
        )
        .expect("both legs have an eligible quote");

    // Appendix A's acceptance block, asserted line by line.
    let filled = Amount(requester_pays(Price(610_000)).0 + requester_pays(Price(440_000)).0);
    let released = Amount(requester_reservation.0 - filled.0);
    let requester_entry =
        *stage.harness().engine().ledger().account(REQUESTER).expect("account");
    assert_eq!(requester_entry.committed(), filled, "Appendix A: 61,000 + 44,000 = 105,000");
    assert_eq!(requester_entry.reserved(), Amount::ZERO);
    note(&format!(
        "requester {} reserved → {} committed → {} released (limit − fill)",
        trace::minor_units(requester_reservation),
        trace::minor_units(filled),
        trace::minor_units(released)
    ));
    assert_eq!(
        stage.harness().engine().ledger().account(BETA).expect("account").committed(),
        maker_pays(Price(610_000)),
        "Appendix A: Beta A 39,000 reserved → committed"
    );
    assert_eq!(
        stage.harness().engine().ledger().account(ALPHA).expect("account").committed(),
        maker_pays(Price(440_000)),
        "Appendix A: Alpha B 56,000 reserved → committed"
    );
    for loser in [GAMMA, DELTA] {
        assert_eq!(
            stage.harness().engine().ledger().account(loser).expect("account").reserved(),
            Amount::ZERO,
            "every losing reservation is released"
        );
    }
    let intent = stage
        .harness()
        .emitted()
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::SubmitIntent { legs, n_legs, .. } => Some((*legs, *n_legs)),
            _ => None,
        })
        .expect("commit emits exactly one intent");
    assert_eq!(intent.1, 2);
    assert_eq!(intent.0[0].maker, BETA, "leg A: Beta and Gamma both quoted 0.61, Beta arrived first");
    assert_eq!(intent.0[0].fill_price, Price(610_000));
    assert_eq!(intent.0[1].maker, ALPHA, "leg B: Alpha's replacement at 0.44 beat Gamma's 0.45");
    assert_eq!(intent.0[1].fill_price, Price(440_000));
    assert!(
        stage.harness().emitted().iter().any(|event| matches!(
            event,
            Event::QuoteExpired { maker, .. } if *maker == BETA
        )),
        "Beta's leg-B quote died of expiry, so it is reported as expired and not as outbid"
    );

    let outbid = outbid_makers(&stage);
    assert_eq!(outbid.len(), 4, "Alpha A, Gamma A, Delta A, Gamma B");
    note("every non-winner was told by name: makers never infer the fate of their capital from silence");

    stage.balances("after acceptance — escrow does not exist yet");
    assert_eq!(stage.harness().locked_escrows().count(), 0, "escrow forms only when settlement confirms");

    // ── settlement ──
    section("settlement");
    let outcomes = stage.settle();
    assert!(outcomes[0].is_ok(), "every contribution is present");
    assert_eq!(stage.harness().locked_escrows().count(), 2);
    for (index, price) in [(0_usize, Price(610_000)), (1, Price(440_000))] {
        let escrow = stage.harness().escrows()[index];
        let record = *stage
            .harness()
            .custody()
            .ledger()
            .escrow(escrow)
            .expect("the escrow exists");
        assert_eq!(record.notional(), notional(), "each leg holds size × UNIT = 100,000");
        assert_eq!(record.requester_contribution(), requester_pays(price));
        assert_eq!(record.maker_contribution(), maker_pays(price));
        note(&format!(
            "escrow {} holds {} — split {} requester / {} maker",
            escrow.0,
            trace::minor_units(record.notional()),
            trace::minor_units(record.requester_contribution()),
            trace::minor_units(record.maker_contribution())
        ));
    }
    stage.balances("after settlement — both sides' capital is in escrow");

    // ── resolution and payout ──
    section("resolution — the ECB cuts at both meetings, so both contracts resolve Yes");
    stage.at(seconds_after(START, 20));
    for contract in [SEPTEMBER, OCTOBER] {
        stage
            .harness_mut()
            .oracle_mut()
            .propose(contract, Outcome::Yes)
            .expect("nothing has been proposed yet");
    }
    note("the oracle proposes Yes on both, opening a challenge window on each");
    stage.at(seconds_after(START, 20_000));
    for contract in [SEPTEMBER, OCTOBER] {
        stage.harness_mut().oracle_mut().finalise(contract).expect("the window has closed");
        stage
            .harness_mut()
            .report_oracle_status(contract)
            .expect("a first Final is always admissible");
    }
    note("uncontested and past the window, both finalise — and the adapter carries the status in");
    stage.check();

    let requester_before = stage.harness().custody().ledger().balance(REQUESTER);
    let alpha_before = stage.harness().custody().ledger().balance(ALPHA);
    let beta_before = stage.harness().custody().ledger().balance(BETA);

    section("payout — one outcome, two directions");
    let september_escrow = stage.harness().escrows()[0];
    let october_escrow = stage.harness().escrows()[1];
    stage.settle_escrow(september_escrow, SEPTEMBER, Outcome::Yes);
    stage.settle_escrow(october_escrow, OCTOBER, Outcome::Yes);

    // Leg A: requester bought Yes, outcome Yes → the requester takes the notional.
    // Leg B: requester bought No, outcome Yes → Alpha takes it. Same outcome value,
    // opposite recipients, decided entirely by the side stored on the escrow.
    assert_eq!(
        stage.harness().custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + notional().0),
        "leg A pays the requester"
    );
    assert_eq!(
        stage.harness().custody().ledger().balance(ALPHA),
        Amount(alpha_before.0 + notional().0),
        "leg B pays the maker"
    );
    assert_eq!(
        stage.harness().custody().ledger().balance(BETA),
        beta_before,
        "Beta wrote the Yes leg and lost it"
    );
    note("leg A: requester Yes, outcome Yes → requester. leg B: requester No, outcome Yes → Alpha.");

    // Appendix A's closing arithmetic: paid 105,000, received 100,000, down 5,000.
    let paid = filled;
    let received = notional();
    assert_eq!(
        stage.harness().custody().ledger().balance(REQUESTER),
        Amount(requester_reservation.0 - paid.0 + received.0)
    );
    note(&format!(
        "the requester paid {} and received {} — down {}, exactly the cost of a spread \
         where one leg won and the other lost",
        trace::minor_units(paid),
        trace::minor_units(received),
        trace::minor_units(Amount(paid.0 - received.0))
    ));

    stage.balances("final");
    stage.check();
    stage.close();
}

fn published_count(stage: &Stage) -> usize {
    stage
        .harness()
        .emitted()
        .iter()
        .filter(|event| matches!(event, Event::BestSelectionChanged { .. }))
        .count()
}

fn outbid_makers(stage: &Stage) -> Vec<AccountIdx> {
    stage
        .harness()
        .emitted()
        .iter()
        .filter_map(|event| match event {
            Event::QuoteRejected {
                maker,
                reason: rfq_core::event::QuoteRejectReason::Outbid,
                ..
            } => Some(*maker),
            _ => None,
        })
        .collect()
}
