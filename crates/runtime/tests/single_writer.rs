//! S1.5 gate: the single-writer proof (SPEC §13).
//!
//! **This is the one declared exception to CLAUDE 28.** It spawns threads because thread
//! interleaving is the property under test. The threads it spawns are **clients**; the
//! engine stays single-threaded, and the assertion is that the channel serialises them. The
//! engine and publisher threads belong to the runtime, not to the test — that is the
//! production topology of SPEC §13, not scaffolding. No other test in this repository may
//! spawn a thread.
//!
//! Two things the gate asserts about itself, because a green suite that never exercised the
//! property is worse than no suite:
//!
//! - **that rounds actually interleaved** (CLAUDE 39). A scheduler that ran the clients one
//!   after another would satisfy every assertion below and prove nothing, so the log is
//!   inspected for client switches and the count is compared against what a perfectly
//!   sequential run would produce. The count is printed.
//! - **that clock granularity did not hide the behaviour** (CLAUDE 40). A round completing
//!   inside one millisecond samples the same `now` throughout, so normalisation never fires
//!   and the time-dependent path goes untested. The main round runs on a clock that advances
//!   one tick per read, and the test asserts claims actually expired mid-round. The same
//!   round is then run once on the shipping `MonotonicClock`, to prove the wiring.

// `print_stdout` is denied across the workspace so that the only crate which writes to a
// terminal is the scenario runner (CLAUDE 8). This gate is the exception that proves the
// rule: the interleaving count must be *reported*, not merely asserted, or a reader has no
// way to tell a race from a coincidence.
#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::print_stdout
)]

use std::sync::{Arc, Barrier};

use rfq_core::account::AccountIdx;
use rfq_core::clock::TickClock;
use rfq_core::command::{Command, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::ContractIdx;
use rfq_core::types::{Amount, Dur, Price, Side, Size, Ts};
use rfq_runtime::clock::MonotonicClock;
use rfq_runtime::venue::{
    LogEntry, NullSink, RoundOutcome, RuntimeCapacities, Venue, replay,
};

const CLIENTS: u32 = 4;
const COMMANDS_PER_CLIENT: u32 = 24;
const FUNDING: u64 = 1_000_000;
/// Every client references this one contract; registering it is each round's first command.
const CONTRACT: ContractIdx = ContractIdx(0);
const EVENT_DATE: Ts = Ts(100_000_000);

fn config() -> Config {
    Config {
        max_accounts: CLIENTS,
        max_reservations: 128,
        max_requests: 128,
        max_quotes: 128,
        max_contracts: 4,
        ..Config::default()
    }
}

/// A small channel bound. Large enough to be a queue, small enough that a producer cannot
/// run to completion before the others get going — which is what makes the interleaving
/// assertion below something the runtime has to earn.
fn capacities() -> RuntimeCapacities {
    RuntimeCapacities { command_channel: 2, event_ring: 256, event_buffer: 64, command_log: 256 }
}

/// The commands one client submits: a mirror update, then requests with varying deadlines,
/// and one guaranteed rejection in every eighth slot.
///
/// The deadlines are a few ticks out, against a clock moving one tick per read, so a
/// request opened early in the round is expired — and its claim reclaimed by normalisation —
/// by the time a later command touches the same account.
///
/// The rejections are not decoration. A rejected command still normalises the account it
/// touched (SPEC §4.3), so "every submitted command appears exactly once in the log" has to
/// cover them — and a round in which nothing is ever refused cannot tell a log that keeps
/// every command from one that keeps only the successful ones.
fn client_commands(client: u32) -> Vec<Command> {
    let account = AccountIdx(client);
    let mut commands = Vec::with_capacity(COMMANDS_PER_CLIENT as usize);
    commands.push(Command::CreditAccount { account, free: Amount(FUNDING) });
    for step in 1..COMMANDS_PER_CLIENT {
        let mut legs = [LegSpec::default(); MAX_LEGS];
        // Deadlines land in a band that the tick clock crosses partway through the round:
        // requests submitted early are admitted and then expire while the round is still
        // running, and requests submitted late are refused outright. Both halves are wanted
        // — the first exercises normalisation, the second keeps rejections in the log.
        let deadline = Ts(1_000 + 30 + u64::from(step % 7) * 3);
        let (size, limit) = if step % 8 == 7 {
            // Ten times the account's whole balance: refused with InsufficientFree, always.
            (Size(FUNDING * 10), Price(1_000))
        } else {
            (Size(u64::from(step % 7) + 1), Price(1_000))
        };
        legs[0] = LegSpec { contract: CONTRACT, side: Side::Yes, size, limit };
        commands.push(Command::SubmitRequest { requester: account, deadline, legs, n_legs: 1 });
    }
    commands
}

/// Run one round with `CLIENTS` client threads submitting concurrently.
fn concurrent_round<C: rfq_core::clock::Clock + Send + 'static>(clock: C) -> RoundOutcome {
    let venue = Venue::start(config(), clock, NullSink, capacities()).unwrap();

    // The contract every client's legs reference. Submitted before the clients start, so
    // the round's interleaving is over the requests and not over this.
    venue
        .commands()
        .send(Command::RegisterContract { contract: CONTRACT, event_date: EVENT_DATE })
        .unwrap();

    // A barrier so the clients are released together. Without it the first thread spawned
    // can finish before the last is scheduled, and the race the test is about never happens.
    let barrier = Arc::new(Barrier::new(CLIENTS as usize));
    let mut clients = Vec::with_capacity(CLIENTS as usize);
    for client in 0..CLIENTS {
        let sender = venue.commands();
        let barrier = Arc::clone(&barrier);
        clients.push(
            std::thread::Builder::new()
                .name(format!("client-{client}"))
                .spawn(move || {
                    barrier.wait();
                    for command in client_commands(client) {
                        sender.send(command).expect("the engine outlives its clients");
                    }
                })
                .unwrap(),
        );
    }
    for client in clients {
        client.join().expect("a client thread must not panic");
    }

    venue.join()
}

/// Which client a log entry came from, read off the account it addresses.
fn client_of(entry: &LogEntry) -> Option<u32> {
    match entry.command {
        Command::CreditAccount { account, .. } => Some(account.0),
        Command::SubmitRequest { requester, .. } => Some(requester.0),
        _ => None,
    }
}

/// How many times the log switches from one client to another.
///
/// A perfectly sequential run — every client finishing before the next begins — produces
/// exactly `CLIENTS - 1` switches. Anything above that is real interleaving.
fn switches(log: &[LogEntry]) -> usize {
    log.windows(2).filter(|pair| client_of(&pair[0]) != client_of(&pair[1])).count()
}

#[test]
fn several_client_threads_submit_and_the_channel_serialises_them() {
    let start = Ts(1_000);
    let mut total_switches = 0_usize;
    let mut rounds_with_expiry = 0_u32;

    // 100 iterations. Flakiness here is a finding, not something to retry around.
    for iteration in 0..100 {
        let outcome = concurrent_round(TickClock::new(start, Dur(1)));
        let RoundOutcome {
            engine,
            log,
            coverage_checks,
            events_dropped,
            events_emitted,
            halted,
        } = outcome;
        // The round ended because the clients finished, not because the log queue filled.
        assert_eq!(halted, rfq_runtime::HaltReason::ChannelClosed, "iteration {iteration}");
        // The publisher only receives, so a refusal would mean it stalled or died.
        assert_eq!(events_dropped, 0, "iteration {iteration}: the publisher stopped draining");

        // ── (a) every submitted command appears exactly once, in channel order ──
        // One RegisterContract, then every client's commands.
        let expected = (CLIENTS * COMMANDS_PER_CLIENT) as usize + 1;
        assert_eq!(log.len(), expected, "iteration {iteration}: a command was lost or doubled");
        for (position, entry) in log.iter().enumerate() {
            assert_eq!(
                entry.sequence, position as u64,
                "iteration {iteration}: the log is not in channel order"
            );
        }
        // Exactly once, per client, and in the order that client sent them.
        for client in 0..CLIENTS {
            let seen: Vec<Command> = log
                .iter()
                .filter(|entry| client_of(entry) == Some(client))
                .map(|entry| entry.command)
                .collect();
            assert_eq!(
                seen,
                client_commands(client),
                "iteration {iteration}: client {client}'s commands were reordered or dropped"
            );
        }

        // The engine sampled a strictly increasing instant per command — one sample each,
        // never re-read, never shared.
        for pair in log.windows(2) {
            assert!(
                pair[1].at > pair[0].at,
                "iteration {iteration}: two commands sampled the same instant"
            );
        }

        // ── (b) replay reproduces the final state byte-for-byte ──
        let replayed = replay(config(), &log, capacities().event_buffer).unwrap();
        assert!(
            replayed == engine,
            "iteration {iteration}: replaying the log did not reproduce the engine's state"
        );

        // ── (c) claim coverage held throughout ──
        // The venue asserts it per command as a scoped debug assertion, so a violation
        // anywhere in the round would have panicked the engine thread and `join` would have
        // panicked in turn. Reaching this line is the assertion. What still has to be
        // checked explicitly is that the assertion actually ran, once per command — a debug
        // assertion that is never reached proves nothing (CLAUDE 39).
        assert_eq!(coverage_checks, expected as u64, "iteration {iteration}");
        assert_eq!(engine.ledger().check_claim_coverage(), Ok(()));

        // ── the gate's assertions about itself ──
        total_switches += switches(&log);

        // The round actually refused things, so gate (a) is covering rejected commands and
        // not merely successful ones (CLAUDE 39).
        let rejected = log.iter().filter(|entry| entry.outcome.is_err()).count();
        assert!(
            rejected >= CLIENTS as usize,
            "iteration {iteration}: only {rejected} commands were refused; the log's \
             every-command claim is untested"
        );

        // CLAUDE 40: normalisation must have fired. With TTLs of 1-5 ticks and ~96 commands
        // at one tick each, claims made early are long dead by the end; if the ledger holds
        // every claim ever made, the clock never moved and the time path went untested.
        let claims_made = log
            .iter()
            .filter(|entry| {
                entry.outcome.is_ok() && matches!(entry.command, Command::SubmitRequest { .. })
            })
            .count();
        let claims_alive = engine.ledger().reservation_count() as usize;
        assert!(claims_made > 0, "iteration {iteration}: no claims were made at all");
        if claims_alive < claims_made {
            rounds_with_expiry += 1;
        }

        // The engine really emits now: every admitted request fans a `RequestOpened` out to
        // makers, which is the step that makes the venue an RFQ (SPEC §5.2).
        assert!(
            events_emitted >= claims_made as u64,
            "iteration {iteration}: {events_emitted} events for {claims_made} admitted requests"
        );
        // Nothing is asserted about `events_dropped`: the ring is bounded and the publisher
        // is scheduled independently, so a drop here would be the design working, not a bug.
        let _ = events_dropped;
    }

    // The interleaving assertion (CLAUDE 39). A sequential scheduler gives exactly
    // CLIENTS - 1 switches per round; anything more is the channel serialising a genuine
    // race. Reported, not merely asserted.
    let sequential_floor = (CLIENTS as usize - 1) * 100;
    println!(
        "client switches across 100 rounds: {total_switches} (a sequential run would give \
         {sequential_floor}); rounds where normalisation reclaimed: {rounds_with_expiry}/100"
    );
    assert!(
        total_switches > sequential_floor,
        "the clients never interleaved: {total_switches} switches over 100 rounds is what a \
         sequential scheduler produces, so this gate proved nothing"
    );
    assert_eq!(
        rounds_with_expiry, 100,
        "normalisation did not fire in every round; the clock is not moving between commands \
         and the time-dependent path is untested (CLAUDE 40)"
    );
}

#[test]
fn the_same_round_runs_on_the_shipping_clock() {
    // CLAUDE 40's second half: the tick clock proves the logic, this proves the wiring.
    // A real round on a real wall clock may complete inside one millisecond, so nothing is
    // asserted here about normalisation — only that the loop runs, the log is complete and
    // ordered, and replay still reproduces the state.
    let outcome = concurrent_round(MonotonicClock::starting_at(Ts(1_000)));
    let expected = (CLIENTS * COMMANDS_PER_CLIENT) as usize + 1;

    assert_eq!(outcome.log.len(), expected);
    for (position, entry) in outcome.log.iter().enumerate() {
        assert_eq!(entry.sequence, position as u64);
    }
    // A monotonic clock may repeat an instant; it may never go backwards.
    for pair in outcome.log.windows(2) {
        assert!(pair[1].at >= pair[0].at, "the shipping clock ran backwards");
    }
    assert_eq!(outcome.coverage_checks, expected as u64);

    let replayed = replay(config(), &outcome.log, capacities().event_buffer).unwrap();
    assert!(replayed == outcome.engine, "replay diverged on the shipping clock");
}

#[test]
fn a_rejected_command_still_normalises_so_the_log_must_keep_it() {
    // A rejection mutates nothing of its own, but it *normalises* the account it touched
    // first (SPEC §4.3), and normalisation is a state change. So "every submitted command
    // appears in the log" has to mean every command, not every successful one — a log that
    // kept only the accepted ones would not replay.
    //
    // The sequence is built so the rejected command is the only thing that ever touches
    // account 0 again after its claim dies:
    //
    //   0  register the contract
    //   1  credit account 0 with 100
    //   2  account 0 opens a request reserving all 100, deadline at tick 3 — dead by the
    //      time command 3 lands
    //   3  account 0 asks for a request it cannot fund -> normalises the dead claim, rejects
    //   4  credit account 1 — a different account, so account 0 is never revisited
    let request = |size: u64, deadline: u64| {
        let mut legs = [LegSpec::default(); MAX_LEGS];
        legs[0] =
            LegSpec { contract: CONTRACT, side: Side::Yes, size: Size(size), limit: Price(1) };
        Command::SubmitRequest {
            requester: AccountIdx(0),
            deadline: Ts(deadline),
            legs,
            n_legs: 1,
        }
    };

    let venue = Venue::start(config(), TickClock::new(Ts(0), Dur(1)), NullSink, capacities())
        .unwrap();
    let sender = venue.commands();
    sender
        .send(Command::RegisterContract { contract: CONTRACT, event_date: EVENT_DATE })
        .unwrap();
    sender.send(Command::CreditAccount { account: AccountIdx(0), free: Amount(100) }).unwrap();
    sender.send(request(100, 3)).unwrap();
    sender.send(request(1_000, 20)).unwrap();
    sender.send(Command::CreditAccount { account: AccountIdx(1), free: Amount(1) }).unwrap();
    drop(sender);

    let outcome = venue.join();
    assert_eq!(outcome.log.len(), 5, "the rejected command must be in the log");
    assert!(outcome.log[3].outcome.is_err(), "1_000 is more than the account holds");

    // Its normalisation is the only reason account 0 holds nothing at the end.
    assert_eq!(outcome.engine.ledger().reservation_count(), 0);
    assert_eq!(
        outcome.engine.ledger().account(AccountIdx(0)).unwrap().reserved(),
        Amount::ZERO
    );

    // The full log replays exactly.
    let replayed = replay(config(), &outcome.log, capacities().event_buffer).unwrap();
    assert!(replayed == outcome.engine);

    // The log with the rejection pruned does not. This is the assertion that makes the
    // point: drop the rejected command and the dead claim is never reclaimed.
    let pruned: Vec<LogEntry> =
        outcome.log.iter().copied().filter(|entry| entry.outcome.is_ok()).collect();
    assert_eq!(pruned.len(), 4);
    let from_pruned = replay(config(), &pruned, capacities().event_buffer).unwrap();
    assert_eq!(
        from_pruned.ledger().reservation_count(),
        1,
        "without the rejected command nothing ever normalises account 0 again"
    );
    assert!(
        from_pruned != outcome.engine,
        "a log that kept only accepted commands replayed to the same state, which would mean \
         rejections do not normalise — and then SPEC §15.4's post-normalisation baseline \
         would be describing nothing"
    );
}

#[test]
fn the_engine_keeps_applying_after_the_publisher_dies() {
    // SPEC §13: the audit trail is best-effort, the state machine is authoritative. Stopping
    // the sole writer would leave commands unapplied and the command log incomplete, which
    // is worse than losing event continuity.
    struct DyingSink;
    impl rfq_runtime::venue::EventSink for DyingSink {
        fn publish(&mut self, _event: rfq_runtime::SequencedEvent) {
            panic!("the publisher died");
        }
    }

    let venue =
        Venue::start(config(), TickClock::new(Ts(0), Dur(1)), DyingSink, capacities()).unwrap();
    let sender = venue.commands();

    // The publisher dies on the first event the engine emits — and this stage's engine
    // really does emit: registering the contract and opening a request produce events.
    venue
        .commands()
        .send(Command::RegisterContract { contract: CONTRACT, event_date: EVENT_DATE })
        .unwrap();
    for client in 0..CLIENTS {
        for command in client_commands(client) {
            sender.send(command).expect("the engine must still be accepting commands");
        }
    }
    drop(sender);

    let outcome = venue.join();
    assert_eq!(
        outcome.log.len(),
        (CLIENTS * COMMANDS_PER_CLIENT) as usize + 1,
        "the engine stopped when the publisher did"
    );
    // Coverage is asserted per command inside the venue; a violation would have panicked the
    // engine thread rather than returned. This is the count that says it ran.
    assert_eq!(outcome.coverage_checks, outcome.log.len() as u64);
}
