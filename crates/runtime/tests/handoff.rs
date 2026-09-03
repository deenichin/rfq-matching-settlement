//! The two hand-offs out of the engine thread.
//!
//! These replace the ring's tests. The ring is gone: events and the command log now leave on
//! bounded channels to workers that own the consequences, so what has to be asserted changed
//! from *eviction order inside a shared structure* to *what crosses the boundary and what
//! happens when it cannot*.
//!
//! **Two paths are not asserted, and both are named rather than papered over (CLAUDE 29).**
//! `HaltReason::LogUnrecordable` and a refused event both require a worker that has stopped
//! draining — and neither worker can fall behind, because each does nothing but receive.
//! `a_one_slot_event_channel_still_applies_every_command` demonstrates that directly: one
//! slot, sixty-four events, nothing refused. Forcing either path needs a worker that blocks
//! until a test releases it, which needs a second thread (CLAUDE 28) or a wait (CLAUDE 43).
//!
//! That the failure paths are hard to reach *is* the design's claim. It is also why they are
//! counted and reported rather than assumed away: `events_dropped` and `halted` are in
//! `RoundOutcome` so an operator can see a fault that a test cannot construct.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use std::sync::{Arc, Mutex};

use rfq_core::account::AccountIdx;
use rfq_core::clock::TickClock;
use rfq_core::command::{Command, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::ContractIdx;
use rfq_core::types::{Amount, Dur, Price, Side, Size, Ts};
use rfq_runtime::venue::{EventSink, RuntimeCapacities, Venue};
use rfq_runtime::{HaltReason, SequencedEvent};

const SEPTEMBER: ContractIdx = ContractIdx(0);
const REQUESTER: AccountIdx = AccountIdx(0);

fn config() -> Config {
    Config { max_quotes_per_leg: 4, ..Config::default() }
}

/// Records everything that reached the wire, in arrival order.
#[derive(Debug, Default)]
struct Recorder {
    sequences: Vec<u64>,
}

struct Sink(Arc<Mutex<Recorder>>);

impl EventSink for Sink {
    fn publish(&mut self, event: SequencedEvent) {
        if let Ok(mut recorder) = self.0.lock() {
            recorder.sequences.push(event.sequence);
        }
    }
}

/// A request that emits one `RequestOpened`.
fn request(deadline: u64) -> Command {
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec {
        contract: SEPTEMBER,
        side: Side::Yes,
        size: Size(100_000),
        limit: Price(650_000),
    };
    Command::SubmitRequest { requester: REQUESTER, deadline: Ts(deadline), legs, n_legs: 1 }
}

/// Drive `count` requests through a live venue and return the round plus what the sink saw.
fn round(count: u64, capacities: RuntimeCapacities) -> (rfq_runtime::RoundOutcome, Vec<u64>) {
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let venue = Venue::start(
        config(),
        TickClock::new(Ts(1_000), Dur(1)),
        Sink(Arc::clone(&recorder)),
        capacities,
    )
    .expect("the configuration starts");

    let sender = venue.commands();
    sender
        .send(Command::RegisterContract { contract: SEPTEMBER, event_date: Ts(10_000_000) })
        .unwrap();
    sender
        .send(Command::CreditAccount { account: REQUESTER, free: Amount(1_000_000_000_000_000) })
        .unwrap();
    for index in 0..count {
        sender.send(request(50_000 + index)).unwrap();
    }
    drop(sender);

    let outcome = venue.join();
    let seen = recorder.lock().unwrap().sequences.clone();
    (outcome, seen)
}

#[test]
fn every_event_crosses_the_boundary_and_arrives_in_order() {
    let (outcome, seen) = round(8, RuntimeCapacities::default());

    assert_eq!(outcome.halted, HaltReason::ChannelClosed, "the round ended normally");
    assert_eq!(outcome.events_dropped, 0, "the publisher never stopped draining");
    assert!(outcome.events_emitted >= 8, "the engine must actually have emitted");

    // The publisher is the only reader, so what it saw is what crossed.
    assert_eq!(
        seen.len() as u64,
        outcome.events_emitted,
        "every accepted event reached the sink"
    );
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "arrival order is send order");
}

#[test]
fn sequence_numbers_are_contiguous_when_nothing_is_refused() {
    // The gap-detection story: with no drops the consumer sees 0,1,2,… and any hole would
    // mean something was refused at the boundary. This is what makes the drop *visible*
    // rather than merely counted.
    let (outcome, seen) = round(8, RuntimeCapacities::default());
    assert_eq!(outcome.events_dropped, 0);

    let expected: Vec<u64> = (0..seen.len() as u64).collect();
    assert_eq!(seen, expected, "no gaps, because nothing was refused");
}

#[test]
fn a_one_slot_event_channel_still_applies_every_command() {
    // The narrowest possible event channel — one slot — and the engine still applies every
    // command, because it cannot block on the hand-off.
    //
    // **And nothing is refused even here**, which is the interesting result rather than a
    // weakness in the test. The publisher does nothing but `recv` and hand to the sink, so a
    // single writer cannot outrun it: it is not fast *enough*, it is faster than the producer
    // by construction. That is precisely the argument for owning the reader — a full queue
    // stops meaning "a subscriber is slow", which is unbounded, and starts meaning "the
    // worker has stopped", which is a fault.
    //
    // So the refusal path is **unreached** here, like `LogUnrecordable`. Forcing it needs a
    // sink that blocks until released, which needs a second thread (CLAUDE 28) or a wait
    // (CLAUDE 43). Asserted below is what is true: every command applied, and the accounting
    // adds up.
    let capacities = RuntimeCapacities { event_ring: 1, ..RuntimeCapacities::default() };
    let (outcome, _) = round(64, capacities);

    assert_eq!(outcome.halted, HaltReason::ChannelClosed, "a full event queue never halts");
    assert_eq!(outcome.log.len(), 66, "every command was applied: 64 requests + 2 setup");
    assert!(
        outcome.log.iter().all(|entry| entry.outcome.is_ok()),
        "and every one succeeded — a refused event is not a refused command"
    );
    // 64, not 66: `RegisterContract` and `CreditAccount` emit nothing, so only the requests
    // produce an event. Commands and events are not one-for-one.
    assert_eq!(
        outcome.events_emitted + outcome.events_dropped,
        64,
        "emitted plus refused accounts for every event the engine produced"
    );
    assert_eq!(
        outcome.events_dropped, 0,
        "the publisher kept up even at one slot — which is the claim, not an accident"
    );
}

#[test]
fn the_command_log_is_complete_and_ordered_across_the_handoff() {
    // The log's guarantee is stricter than the event stream's: `replay` reproduces engine
    // state from it, so a lost entry is a lost guarantee rather than a lost message. The
    // logger thread must therefore deliver every entry, in order.
    let (outcome, _) = round(16, RuntimeCapacities::default());

    assert_eq!(outcome.log.len(), 18, "16 requests plus the two setup commands");
    let sequences: Vec<u64> = outcome.log.iter().map(|entry| entry.sequence).collect();
    assert_eq!(sequences, (0..18).collect::<Vec<u64>>(), "contiguous and in order");
    assert!(
        outcome.log.windows(2).all(|pair| pair[1].at >= pair[0].at),
        "and the sampled clock never runs backwards"
    );
}

#[test]
fn both_workers_retire_when_the_engine_drops_its_senders() {
    // No shutdown flag and no handshake: closing the sender *is* the signal. If either
    // worker failed to notice, `join` would hang and this test would never return.
    let (outcome, seen) = round(4, RuntimeCapacities::default());
    assert_eq!(outcome.halted, HaltReason::ChannelClosed);
    assert!(!outcome.log.is_empty(), "the logger returned its collection");
    assert!(!seen.is_empty(), "the publisher drained before retiring");
}
