//! Two resolution paths: a contest ended by the escalation authority, and a silent oracle
//! stalling into `Void`.

#![allow(clippy::print_stdout)]

use rfq_core::command::Command;
use rfq_core::contract::{ContractState, OracleStatus, Outcome};
use rfq_core::types::{Amount, Price, Ts};

use crate::contract_config;
use crate::failures::{one_leg_escrowed, one_leg_funding};
use crate::happy::{ALPHA, REQUESTER, SEPTEMBER, SIZE, START};
use crate::stage::{Stage, note, seconds_after, section};
use crate::trace::{self, Cast};
use crate::{EVENT_DATE, register_contracts};

const ESCALATION: rfq_core::account::AccountIdx = rfq_core::account::AccountIdx(5);

fn cast() -> Cast {
    Cast::new(vec![
        (REQUESTER, "requester"),
        (ALPHA, "Alpha"),
        (ESCALATION, "escalation"),
    ])
}

// ═══════════════════ 5. contested, then escalated ═══════════════════

/// The oracle reports `InProgress`, settlement is refused, the escalation authority rules,
/// and settlement proceeds.
///
/// # Panics
///
/// If a contested contract can be settled, or if anyone but the authority can end it.
pub(crate) fn contested_then_escalated() {
    let mut stage = Stage::open(
        "5. a contested resolution, ended by the escalation authority",
        "Contesting buys a delay, not an outcome. The stall exit conditions on Silence, so a\n\
         contested contract never times out into Void — otherwise a party who is losing\n\
         contests a correct outcome, waits out the grace period, and cancels a trade they\n\
         have already lost. Once the oracle is working the only exits are Final or the\n\
         escalation authority: a single unbonded key, and the design's one trusted component.",
        contract_config(),
        cast(),
        START,
    );

    section("funding, market, escrow");
    let (requester_stake, maker_stake) = one_leg_funding();
    stage.fund(REQUESTER, requester_stake);
    stage.fund(ALPHA, maker_stake);
    register_contracts(&mut stage, &[SEPTEMBER]);
    let (_request, escrow) = one_leg_escrowed(&mut stage);
    stage.balances("escrowed — both sides' capital is with custody");

    section("the oracle proposes, and is contested");
    stage.at(seconds_after(START, 20));
    stage.harness_mut().oracle_mut().propose(SEPTEMBER, Outcome::Yes).expect("first proposal");
    stage.harness_mut().oracle_mut().contest(SEPTEMBER).expect("a standing proposal");
    let status = stage.harness_mut().report_oracle_status(SEPTEMBER).expect("Silent → InProgress");
    assert_eq!(status, OracleStatus::InProgress);
    note("proposal and contest both collapse to InProgress: working, but not final");
    stage.check();

    section("settlement is refused, and stays refused however long anyone waits");
    let refused = stage
        .apply("SettleEscrow", Command::SettleEscrow { escrow, contract: SEPTEMBER });
    assert!(refused.is_err(), "OutcomeNotYet — the contract has no outcome");

    // Far past the stall grace, and still nothing: InProgress never times out into Void.
    stage.at(Ts(EVENT_DATE.0 + 86_400_000 + 1_000));
    let still_refused = stage
        .apply(
            "SettleEscrow  — now far past the stall grace",
            Command::SettleEscrow { escrow, contract: SEPTEMBER },
        );
    assert!(still_refused.is_err(), "the stall exit requires Silence");
    note("the stall exit is unreachable once the oracle has spoken — contestation is not a free unwind");
    assert_eq!(stage.harness().locked_escrows().count(), 1, "the escrow stays locked");

    section("nobody but the escalation authority can end it");
    assert!(
        stage.harness_mut().oracle_mut().escalate(SEPTEMBER, Outcome::No, ALPHA).is_err(),
        "the authority is a single designated id"
    );
    note("Alpha, who stands to gain, cannot rule on their own trade");

    section("the authority rules, and settlement proceeds");
    stage
        .harness_mut()
        .oracle_mut()
        .escalate(SEPTEMBER, Outcome::No, ESCALATION)
        .expect("the authority may assign any outcome");
    let status = stage.harness_mut().report_oracle_status(SEPTEMBER).expect("InProgress → Final");
    assert_eq!(status, OracleStatus::Final(Outcome::No));
    assert_eq!(
        stage.harness().engine().ledger().contract(SEPTEMBER).expect("registered").state(),
        ContractState::Resolved(Outcome::No)
    );
    stage.check();

    let maker_before = stage.harness().custody().ledger().balance(ALPHA);
    stage.settle_escrow(escrow, SEPTEMBER, Outcome::No);
    assert_eq!(
        stage.harness().custody().ledger().balance(ALPHA),
        Amount(maker_before.0 + SIZE.notional().expect("no overflow").0),
        "the requester bought Yes and No happened, so the maker takes the notional"
    );
    note("the requester bought Yes; the outcome is No; the payout maps through the leg's side");

    stage.balances("final");
    stage.check();
    stage.close();
}

// ═══════════════════ 6. stalled into Void ═══════════════════

/// The oracle never speaks. Past the grace period the outcome is `Void`, and each side gets
/// **its own contribution** back — asserted by amount, because a 50/50 split also "resolves
/// to Void" and moves money between the parties.
///
/// # Panics
///
/// If the refund is a split rather than a return.
pub(crate) fn stalled_into_void() {
    let mut stage = Stage::open(
        "6. the oracle stays silent, and the trade unwinds into Void",
        "Void is an outcome value, not a state, so ambiguity needs no special path. The stall\n\
         exit is triggerable only by time, never by a participant, and it returns each side\n\
         its OWN contribution: refunding contributions restores the exact pre-trade\n\
         allocation, while splitting the notional moves money between the parties and is a\n\
         redistribution disguised as neutrality.",
        contract_config(),
        cast(),
        START,
    );

    section("funding, market, escrow");
    let (requester_stake, maker_stake) = one_leg_funding();
    stage.fund(REQUESTER, requester_stake);
    stage.fund(ALPHA, maker_stake);
    register_contracts(&mut stage, &[SEPTEMBER]);
    let (_request, escrow) = one_leg_escrowed(&mut stage);

    let requester_contribution =
        SIZE.requester_contribution(Price(610_000)).expect("no overflow");
    let maker_contribution = SIZE.maker_contribution(Price(610_000)).expect("no overflow");
    assert_ne!(
        requester_contribution, maker_contribution,
        "the two contributions must differ, or this scenario cannot tell a refund from a split"
    );
    note(&format!(
        "escrow holds {} — {} from the requester, {} from the maker",
        trace::minor_units(SIZE.notional().expect("no overflow")),
        trace::minor_units(requester_contribution),
        trace::minor_units(maker_contribution)
    ));

    section("before the grace period elapses, there is no outcome");
    stage.at(Ts(EVENT_DATE.0 + 86_400_000));
    assert_eq!(
        stage.harness().engine().ledger().contract(SEPTEMBER).expect("registered").oracle_status(),
        OracleStatus::Silent,
        "the stall exit conditions on Silence, and the oracle has said nothing"
    );
    let refused =
        stage.apply("SettleEscrow  — exactly at the grace boundary", Command::SettleEscrow { escrow, contract: SEPTEMBER });
    assert!(refused.is_err(), "the boundary is half-open at this end too");

    section("past it, the outcome is Void");
    stage.at(Ts(EVENT_DATE.0 + 86_400_000 + 1));
    let requester_before = stage.harness().custody().ledger().balance(REQUESTER);
    let maker_before = stage.harness().custody().ledger().balance(ALPHA);
    stage
        .apply("SettleEscrow", Command::SettleEscrow { escrow, contract: SEPTEMBER })
        .expect("the stall exit is now open");
    let payouts = stage.harness_mut().apply_payouts();
    assert_eq!(payouts, vec![Ok(true)]);
    stage.check_settlement();

    assert_eq!(
        stage.harness().custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + requester_contribution.0),
        "the requester gets back exactly what they put in"
    );
    assert_eq!(
        stage.harness().custody().ledger().balance(ALPHA),
        Amount(maker_before.0 + maker_contribution.0),
        "and the maker exactly what they put in"
    );
    let half = Amount(SIZE.notional().expect("no overflow").0 / 2);
    assert_ne!(
        stage.harness().custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + half.0),
        "a 50/50 split would have paid this instead — asserted by amount, not by label"
    );
    note("each side's own contribution, not half each: the pre-trade allocation, exactly restored");

    stage.balances("final — everybody is back where they started");
    for (account, stake) in [(REQUESTER, requester_stake), (ALPHA, maker_stake)] {
        assert_eq!(
            stage.harness().custody().ledger().balance(account),
            stake,
            "a void trade leaves every balance exactly as it was funded"
        );
    }
    stage.check();
    stage.close();
}
