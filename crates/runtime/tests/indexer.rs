//! S6 gate: PLAN (a)–(d), the chain boundary.
//!
//! The indexer is a separate component translating log entries into engine commands. Cursor,
//! confirmation depth and dedup are three properties, each covering a failure the others do
//! not, and each tested by removing it rather than by watching them all work together.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_chain::log::ChainPayload;
use rfq_core::account::AccountIdx;
use rfq_core::clock::TestClock;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::{ContractIdx, ContractState, OracleStatus, Outcome};
use rfq_core::escrow::EscrowId;
use rfq_core::types::{Amount, Dur, LegId, Price, Side, Size, Ts};
use rfq_runtime::harness::Harness;

const REQUESTER: AccountIdx = AccountIdx(0);
const ALPHA: AccountIdx = AccountIdx(1);
const ESCALATION: AccountIdx = AccountIdx(3);
const SEPTEMBER: ContractIdx = ContractIdx(0);
const EVENT_DATE: Ts = Ts(50_000_000);
const DEADLINE: Ts = Ts(100_000);
const SIZE: Size = Size(100);
const LIMIT: Price = Price(650_000);
const FILL: Price = Price(610_000);

/// Two confirmations, so an entry is believed only once two more blocks sit on top of it.
const CONFIRMATIONS: u32 = 2;

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
        escalation_authority: ESCALATION,
        confirmations: CONFIRMATIONS,
        block_time: Dur(1_000),
        // The timelock has to cover the confirmation lag it now admits (§9.3).
        withdrawal_delay: Dur(1_000_000),
        ..Config::default()
    }
}

type TestHarness = Harness<TestClock, TestClock>;

fn harness() -> TestHarness {
    Harness::new(
        config(),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
        TestClock::at(Ts(1_000)),
    )
    .unwrap()
}

/// Mine enough blocks for everything already logged to become deep enough.
fn confirm(harness: &mut TestHarness) {
    for _ in 0..CONFIRMATIONS {
        harness.advance_block();
    }
}

fn maker_contribution() -> Amount {
    SIZE.maker_contribution(FILL).unwrap()
}

/// A funded, matched, escrowed one-leg market, with everything confirmed.
fn escrowed(harness: &mut TestHarness) -> EscrowId {
    harness.deposit(REQUESTER, SIZE.requester_contribution(LIMIT).unwrap()).unwrap();
    harness.deposit(ALPHA, maker_contribution()).unwrap();
    confirm(harness);
    harness.apply(Command::RegisterContract { contract: SEPTEMBER, event_date: EVENT_DATE }).unwrap();

    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT };
    harness
        .apply(Command::SubmitRequest { requester: REQUESTER, deadline: DEADLINE, legs, n_legs: 1 })
        .unwrap();
    let request = harness.engine().ledger().requests().next().unwrap().0;
    harness.set_both_clocks(Ts(1_100));
    harness
        .apply(Command::SubmitQuote {
            maker: ALPHA,
            request,
            leg: LegId(0),
            price: FILL,
            size: SIZE,
            expires_at: Ts(20_000),
        })
        .unwrap();
    harness.set_both_clocks(Ts(1_200));
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    expected[0] = ExpectedFill { leg: LegId(0), price: FILL };
    harness.apply(Command::AcceptRequest { request, expected, n_legs: 1 }).unwrap();
    harness.submit_pending();
    harness.include_all();
    // The chain announces the resolution; the indexer delivers it once it is deep enough.
    // Nothing polls: a terminal answer arrives through the log.
    confirm(harness);
    harness.escrows()[0]
}

// ═══════════════ (a) replay: the cursor is not what makes it safe ═══════════════

#[test]
fn rewinding_the_cursor_changes_nothing_but_forgetting_the_dedup_set_delivers_twice() {
    // Two properties, and only one of them is load-bearing here. A restart that lost its
    // position replays the whole log; dedup on `(tx_hash, log_index)` is what makes that a
    // no-op. Clearing only the cursor would show a green test that proves nothing about
    // either, so the dedup state is cleared too and the difference is the assertion.
    let mut harness = harness();
    harness.deposit(REQUESTER, Amount(1_000)).unwrap();
    harness.deposit(ALPHA, Amount(500)).unwrap();
    confirm(&mut harness);

    let balances = [
        harness.custody().ledger().balance(REQUESTER),
        harness.custody().ledger().balance(ALPHA),
    ];
    let mirrored = harness.engine().ledger().account(REQUESTER).unwrap().free();
    let delivered = harness.indexer().delivered_count();
    assert!(delivered >= 2, "the log must actually have entries to replay");
    assert_eq!(harness.indexer().cursor(), harness.custody().log().entries().len());

    // Replay from cursor zero with the dedup set intact: every entry is read again and every
    // one is recognised.
    harness.indexer_mut().rewind();
    assert_eq!(harness.indexer().cursor(), 0);
    let redelivered = harness.pump_indexer();
    assert_eq!(redelivered, 0, "a replay must deliver nothing");
    assert_eq!(harness.indexer().delivered_count(), delivered, "and record nothing new");

    // Balances unchanged, conservation holds, no phantom credit.
    assert_eq!(harness.custody().ledger().balance(REQUESTER), balances[0]);
    assert_eq!(harness.custody().ledger().balance(ALPHA), balances[1]);
    assert_eq!(harness.engine().ledger().account(REQUESTER).unwrap().free(), mirrored);
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));

    // Now lose the dedup set as well. The same entries arrive a second time — which is what
    // proves dedup, and not the cursor, was doing the work.
    harness.indexer_mut().forget_everything();
    let redelivered = harness.pump_indexer();
    assert_eq!(redelivered, delivered, "without dedup the whole log arrives again");

    // And even then nothing is corrupted, because a mirror update is idempotent: it sets an
    // absolute value rather than applying a delta. That is the second line of defence, and
    // it is why replay is *harmless* rather than merely *prevented*.
    assert_eq!(harness.engine().ledger().account(REQUESTER).unwrap().free(), mirrored);
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

// ═══════════════ (b) a reorg below the confirmation depth ═══════════════

#[test]
fn a_reorg_below_the_confirmation_depth_is_never_seen_by_the_engine() {
    // Confirmation depth **avoids** reorgs rather than recovering from them: an entry that
    // vanishes before it is deep enough was never delivered, so there is nothing to undo.
    // Deeper-reorg recovery — roll back and reapply from the command log — is designed and
    // not built (§12).
    let mut harness = harness();
    harness.apply(Command::RegisterContract { contract: SEPTEMBER, event_date: EVENT_DATE }).unwrap();

    // Block 1 carries a resolution the chain is about to change its mind about.
    harness.custody_mut().log_mut().advance_block();
    let orphaned_block = harness.custody().log().head();
    harness.custody_mut().log_mut().append(ChainPayload::OracleStatusReported {
        contract: SEPTEMBER,
        status: OracleStatus::Final(Outcome::Yes),
    });

    // One block on top is not enough at depth two.
    harness.advance_block();
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Unresolved,
        "the entry exists and is not yet believed"
    );
    assert_eq!(harness.indexer().delivered_count(), 0);

    // The chain reorganises and replaces it with the opposite outcome.
    harness.custody_mut().log_mut().reorg(orphaned_block);
    assert!(
        harness.custody().log().entries().is_empty(),
        "the orphaned entry is gone from the log"
    );
    harness.custody_mut().log_mut().advance_block();
    harness.custody_mut().log_mut().append(ChainPayload::OracleStatusReported {
        contract: SEPTEMBER,
        status: OracleStatus::Final(Outcome::No),
    });
    confirm(&mut harness);

    // The engine saw exactly one resolution, and it is the surviving one. Had it seen the
    // first, the second would have been refused as an overwrite of Final — so the engine
    // would have been permanently wrong, and monotonicity would have kept it that way.
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Resolved(Outcome::No)
    );
    assert_eq!(harness.indexer().delivered_count(), 1, "one delivery, not two");
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

// ═══════════════ (c) an event arriving before what it depends on ═══════════════

#[test]
fn a_settlement_request_arriving_before_its_resolution_is_refused_and_works_afterwards() {
    // Safety does not depend on delivery order. The indexer translates; it does not
    // interpret, and the engine re-derives admissibility from the state it holds.
    let mut harness = harness();
    let escrow = escrowed(&mut harness);
    assert_eq!(harness.locked_escrows().count(), 1);

    // The settlement request arrives first, naming an outcome the engine has never heard of.
    harness.request_escrow_settlement(escrow, SEPTEMBER, Outcome::Yes);
    confirm(&mut harness);
    assert!(
        harness.pending_payouts().is_empty(),
        "no intent was emitted, because the engine could not derive an outcome"
    );
    assert_eq!(harness.apply_payouts(), Vec::new());
    assert_eq!(harness.locked_escrows().count(), 1, "and the escrow is untouched");

    // The resolution it depended on arrives afterwards.
    harness.oracle_mut().escalate(SEPTEMBER, Outcome::Yes, ESCALATION).unwrap();
    harness.report_oracle_status(SEPTEMBER).unwrap();
    confirm(&mut harness);
    assert_eq!(
        harness.engine().ledger().contract(SEPTEMBER).unwrap().state(),
        ContractState::Resolved(Outcome::Yes)
    );

    // A second request now succeeds. The first was refused, not queued: nothing was
    // remembered and replayed on the engine's behalf, which is what "safety does not depend
    // on delivery order" has to mean if it is to mean anything.
    let requester_before = harness.custody().ledger().balance(REQUESTER);
    harness.request_escrow_settlement(escrow, SEPTEMBER, Outcome::Yes);
    confirm(&mut harness);
    assert_eq!(harness.apply_payouts(), vec![Ok(true)]);
    assert_eq!(
        harness.custody().ledger().balance(REQUESTER),
        Amount(requester_before.0 + SIZE.notional().unwrap().0)
    );
    assert_eq!(harness.locked_escrows().count(), 0);
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

// ═══════════════ (d) lag: stale but never wrong ═══════════════

#[test]
fn a_lagging_indexer_leaves_the_engine_stale_but_never_wrong() {
    // Lag is injectable and set non-zero here — the confirmation depth is two blocks and the
    // configuration says so. With it hard-wired to zero this test would pass without ever
    // exercising the property, which proves only that it is untested.
    let mut harness = harness();
    harness.deposit(ALPHA, Amount(1_000)).unwrap();
    confirm(&mut harness);
    assert_eq!(harness.engine().ledger().account(ALPHA).unwrap().free(), Amount(1_000));

    // A deposit lands on chain and sits below the horizon.
    harness.custody_mut().log_mut().advance_block();
    harness.custody_mut().deposit(ALPHA, Amount(500)).unwrap();
    harness.pump_indexer();

    // The precondition, asserted rather than assumed: the horizon really is behind the head.
    let head = harness.custody().log().head();
    let horizon = harness.indexer().horizon(head).expect("the chain is deep enough");
    assert!(horizon < head, "the lagging horizon must actually be behind the prompt one");
    assert_eq!(head - horizon, u64::from(CONFIRMATIONS));

    // Custody holds 1_500; the engine still believes 1_000. **Stale, and low** — never a
    // phantom credit, because the engine only ever learns of money that is already deep.
    assert_eq!(harness.custody().ledger().balance(ALPHA), Amount(1_500));
    assert_eq!(
        harness.engine().ledger().account(ALPHA).unwrap().free(),
        Amount(1_000),
        "the engine has not heard yet"
    );
    assert!(
        harness.engine().ledger().account(ALPHA).unwrap().free()
            < harness.custody().ledger().balance(ALPHA),
        "staleness understates, so a claim admitted against it is always backed"
    );

    // Conservation and coverage hold throughout — a stale mirror is a liveness cost, never a
    // money-state error (§2.3).
    assert_eq!(harness.check_conservation_only(), Ok(()));

    // And it catches up on its own once the entry is deep enough.
    confirm(&mut harness);
    assert_eq!(harness.engine().ledger().account(ALPHA).unwrap().free(), Amount(1_500));
    assert_eq!(harness.check_cross_system_invariants(), Ok(()));
}

#[test]
fn nothing_is_delivered_from_a_chain_shallower_than_the_confirmation_depth() {
    // The boundary case: at head 0 with depth 2 there is no horizon at all, and the indexer
    // must deliver nothing rather than treat "no horizon" as "everything".
    let mut harness = harness();
    harness.custody_mut().deposit(ALPHA, Amount(1_000)).unwrap();
    assert_eq!(harness.indexer().horizon(harness.custody().log().head()), None);
    assert_eq!(harness.pump_indexer(), 0);
    assert_eq!(harness.engine().ledger().account(ALPHA).unwrap().free(), Amount::ZERO);

    harness.advance_block();
    assert_eq!(harness.pump_indexer(), 0, "one block deep is still not two");
    harness.advance_block();
    assert!(harness.pump_indexer() > 0 || harness.indexer().delivered_count() > 0);
    assert_eq!(harness.engine().ledger().account(ALPHA).unwrap().free(), Amount(1_000));
}

#[test]
fn one_transaction_with_several_logs_is_deduplicated_per_log_index() {
    // The dedup key is `(tx_hash, log_index)`, not the hash alone. A transaction emitting
    // several events is ordinary; keying on the hash would drop all but the first, which
    // would look exactly like a working dedup until someone batched two credits.
    let mut harness = harness();
    harness.custody_mut().log_mut().append_transaction(&[
        ChainPayload::BalanceChanged { account: REQUESTER, available: Amount(700) },
        ChainPayload::BalanceChanged { account: ALPHA, available: Amount(300) },
    ]);
    confirm(&mut harness);

    assert_eq!(harness.indexer().delivered_count(), 2, "both logs, one transaction");
    assert_eq!(harness.engine().ledger().account(REQUESTER).unwrap().free(), Amount(700));
    assert_eq!(harness.engine().ledger().account(ALPHA).unwrap().free(), Amount(300));

    // And a replay of that transaction still delivers nothing.
    harness.indexer_mut().rewind();
    assert_eq!(harness.pump_indexer(), 0);
}
