//! The single-writer runtime (SPEC §13).
//!
//! One thread owns all mutable engine state and busy-spins on a bounded MPSC command
//! channel. Gateway threads and the indexer produce commands; a publisher thread consumes
//! the event stream and performs all I/O. There is no lock around engine state, because
//! there is only one writer — the races of §11 are impossible by construction rather than
//! guarded (CLAUDE 6, 7).
//!
//! Three threads exist, and only one of them writes:
//!
//! | Thread | Owns | Writes engine state |
//! |---|---|---|
//! | clients (any number) | nothing | no — they send commands |
//! | engine (exactly one) | the [`Engine`], the clock, the command log | **yes** |
//! | publisher (one) | the sink | no — it drains the event ring |
//!
//! The engine thread's loop is the whole concurrency story:
//!
//! ```text
//! try_recv -> sample the clock ONCE -> apply(cmd, now) -> log(now, cmd, outcome)
//!                                   -> move emitted events into the ring
//! ```
//!
//! `now` is sampled at the call site and recorded in the log beside the command, which is
//! what makes replay exact: a fresh engine fed the same commands at the same instants
//! reaches the same state, including everything normalisation reclaimed along the way.
//!
//! **If the publisher dies, the engine keeps applying.** The audit trail is best-effort; the
//! state machine is authoritative. Stopping the sole writer would leave commands unapplied
//! and the command log incomplete, which is strictly worse than losing event continuity.

use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use rfq_core::clock::Clock;
use rfq_core::command::Command;
use rfq_core::config::Config;
use rfq_core::engine::{Engine, EngineError};
use rfq_core::event::EventBuffer;
use rfq_core::types::Ts;

use crate::event_ring::{EventRing, SequencedEvent};

/// One entry of the append-only command log.
///
/// The recorded `at` is the instant the engine sampled, not the instant the command was
/// sent. Replay feeds it back, which is why a log that recorded a fixed or re-sampled
/// instant would replay to a different state — normalisation reclaims by time.
///
/// Rejected commands are logged too, and must be: a rejection still normalises the account
/// it touched (§4.3), so a log that kept only the accepted ones would not replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// Position in the total order the channel imposed.
    pub sequence: u64,
    /// The instant the engine sampled for this command.
    pub at: Ts,
    /// What was applied.
    pub command: Command,
    /// What the engine made of it.
    pub outcome: Result<(), EngineError>,
}

/// Where published events go. The **only** place I/O happens (CLAUDE 8, SPEC §13).
///
/// A trait rather than a concrete writer so the runtime crate itself prints nothing —
/// `print_stdout` is denied everywhere except the scenario runner, which is how "no I/O in
/// the engine" is enforced by the compiler rather than by review.
pub trait EventSink: Send {
    /// Publish one event. Called only on the publisher thread.
    fn publish(&mut self, event: SequencedEvent);
}

/// A sink that discards. The engine emits no events in this stage's command set, so this is
/// the honest default rather than a placeholder for one that does more.
#[derive(Debug, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn publish(&mut self, _event: SequencedEvent) {}
}

/// What a finished round leaves behind.
#[derive(Debug)]
pub struct RoundOutcome {
    /// The engine, after the last command.
    pub engine: Engine,
    /// Every command, in the order the channel serialised them.
    pub log: Vec<LogEntry>,
    /// How many times claim coverage was checked. Asserting the count is what keeps
    /// "coverage held throughout" from being satisfied by never checking (CLAUDE 39).
    pub coverage_checks: u64,
    /// How many of those checks failed. Must be zero.
    pub coverage_violations: u64,
    /// Events evicted from the ring unread.
    pub events_dropped: u64,
}

/// A running venue: one engine thread, one publisher thread, one bounded command channel.
#[derive(Debug)]
pub struct Venue {
    commands: SyncSender<Command>,
    engine_thread: JoinHandle<RoundOutcome>,
    publisher_thread: JoinHandle<()>,
    ring: Arc<Mutex<EventRing>>,
}

/// How large the runtime's preallocated structures are.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeCapacities {
    /// Bound on the command channel. Small values force contention rather than letting a
    /// producer run to completion before another starts.
    pub command_channel: usize,
    /// Events the ring holds before it starts evicting.
    pub event_ring: usize,
    /// Events one command may emit. Checked in the CHECK phase (`EventBufferFull`).
    pub event_buffer: usize,
    /// Commands the log is preallocated for. The log grows if exceeded — it is the
    /// runtime's audit trail, not engine state, and no allocation happens inside `apply`.
    pub command_log: usize,
}

impl Default for RuntimeCapacities {
    fn default() -> Self {
        Self { command_channel: 4, event_ring: 256, event_buffer: 64, command_log: 1024 }
    }
}

impl Venue {
    /// Start the engine and publisher threads.
    ///
    /// The clock is moved onto the engine thread and read by nothing else, so there is one
    /// clock, one reader, and one sample per command.
    ///
    /// # Errors
    ///
    /// Any [`rfq_core::config::ConfigError`] — the venue refuses to start on a configuration
    /// that violates either startup assertion.
    ///
    /// # Panics
    ///
    /// If the operating system refuses to spawn either thread. A venue that cannot start its
    /// sole writer has nothing to degrade to.
    pub fn start<C, S>(
        config: Config,
        clock: C,
        sink: S,
        capacities: RuntimeCapacities,
    ) -> Result<Self, rfq_core::config::ConfigError>
    where
        C: Clock + Send + 'static,
        S: EventSink + 'static,
    {
        let engine = Engine::new(config)?;
        let (commands, inbox) = mpsc::sync_channel::<Command>(capacities.command_channel);
        let ring = Arc::new(Mutex::new(EventRing::with_capacity(capacities.event_ring)));

        let engine_ring = Arc::clone(&ring);
        let engine_thread = thread::Builder::new()
            .name("engine".to_owned())
            .spawn(move || {
                // The clock is owned by this closure and therefore by the engine thread.
                // Nothing else can read it, so there is one clock and one sample per command.
                run_engine(engine, &clock, &inbox, &engine_ring, capacities)
            })
            .unwrap_or_else(|error| panic!("the engine thread must start: {error}"));

        let publisher_ring = Arc::clone(&ring);
        let publisher_thread = thread::Builder::new()
            .name("publisher".to_owned())
            .spawn(move || run_publisher(sink, &publisher_ring))
            .unwrap_or_else(|error| panic!("the publisher thread must start: {error}"));

        Ok(Self { commands, engine_thread, publisher_thread, ring })
    }

    /// A handle for a client thread to submit on. Cloneable: many producers, one consumer.
    #[must_use]
    pub fn commands(&self) -> SyncSender<Command> {
        self.commands.clone()
    }

    /// The event ring, for a test standing in for the emissions S2 will produce.
    #[must_use]
    pub fn ring(&self) -> Arc<Mutex<EventRing>> {
        Arc::clone(&self.ring)
    }

    /// Close the channel, wait for the engine to drain it, and take the round's results.
    ///
    /// The publisher is joined too, but its fate does not gate the engine's: it is joined
    /// *after* the engine has finished, so a publisher that died mid-round costs nothing.
    ///
    /// # Panics
    ///
    /// If the engine thread panicked. Its state is the authoritative one, so there is
    /// nothing sensible to return.
    #[must_use]
    pub fn join(self) -> RoundOutcome {
        let Self { commands, engine_thread, publisher_thread, ring } = self;
        drop(commands);
        let outcome = engine_thread
            .join()
            .unwrap_or_else(|_| panic!("the engine thread must not panic; it owns the state"));
        // The publisher is asked to stop only after the engine is done, so the engine is
        // never waiting on it. A publisher that died already is joined here as an Err and
        // deliberately ignored: the audit trail is best-effort (SPEC §13).
        if let Ok(mut ring) = ring.lock() {
            ring.push_shutdown();
        }
        let _ = publisher_thread.join();
        outcome
    }
}

/// The single writer. Everything that mutates engine state happens on this thread.
fn run_engine<C: Clock>(
    mut engine: Engine,
    clock: &C,
    inbox: &mpsc::Receiver<Command>,
    ring: &Arc<Mutex<EventRing>>,
    capacities: RuntimeCapacities,
) -> RoundOutcome {
    let mut log: Vec<LogEntry> = Vec::with_capacity(capacities.command_log);
    let mut events = EventBuffer::with_capacity(capacities.event_buffer);
    let mut sequence: u64 = 0;
    let mut coverage_checks: u64 = 0;
    let mut coverage_violations: u64 = 0;

    loop {
        match inbox.try_recv() {
            Ok(command) => {
                // Sampled once, here, at the call site (CLAUDE 2). Nothing downstream can
                // re-read it, because nothing downstream holds the clock.
                let now = clock.now();
                events.clear();
                let outcome = engine.apply(command, now, &mut events);

                log.push(LogEntry { sequence, at: now, command, outcome });
                sequence = sequence.saturating_add(1);

                // Events move to the ring *outside* apply. The engine never touches the
                // lock and never waits on the publisher.
                if !events.is_empty()
                    && let Ok(mut ring) = ring.lock()
                {
                    for event in events.drain() {
                        ring.push(event);
                    }
                }

                coverage_checks = coverage_checks.saturating_add(1);
                if engine.ledger().check_claim_coverage().is_err() {
                    coverage_violations = coverage_violations.saturating_add(1);
                }
            }
            // Busy-spin. The venue has one writer and no reason to park it.
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
            Err(TryRecvError::Disconnected) => break,
        }
    }

    let events_dropped = ring.lock().map_or(0, |ring| ring.dropped());
    RoundOutcome { engine, log, coverage_checks, coverage_violations, events_dropped }
}

/// Drains the ring and performs all I/O. Owns no engine state and writes none.
fn run_publisher<S: EventSink>(mut sink: S, ring: &Arc<Mutex<EventRing>>) {
    loop {
        let next = match ring.lock() {
            Ok(mut ring) => {
                if ring.shutdown_requested() && ring.is_empty() {
                    return;
                }
                ring.pop()
            }
            // The ring's lock is poisoned, which means the engine panicked while holding it.
            // The publisher has nothing left to publish.
            Err(_) => return,
        };
        match next {
            Some(event) => sink.publish(event),
            None => std::hint::spin_loop(),
        }
    }
}

/// Replay a command log through a fresh engine, single-threaded.
///
/// This is the real content of the single-writer claim (SPEC §13): "the result is one of the
/// legal serial orders" is not assertable — for N concurrent submits the legal set is N! —
/// and in practice degrades to re-checking conservation, which any implementation satisfies.
/// Reproducing the final state exactly is a statement with teeth.
///
/// Each entry is applied at **its recorded instant**, not at a fresh one. Time is an input
/// to the state machine, so replaying against a different clock replays a different history.
///
/// # Errors
///
/// Any [`rfq_core::config::ConfigError`].
pub fn replay(
    config: Config,
    log: &[LogEntry],
    event_buffer: usize,
) -> Result<Engine, rfq_core::config::ConfigError> {
    let mut engine = Engine::new(config)?;
    let mut events = EventBuffer::with_capacity(event_buffer);
    for entry in log {
        events.clear();
        let outcome = engine.apply(entry.command, entry.at, &mut events);
        debug_assert_eq!(
            outcome, entry.outcome,
            "replay disagreed with the log about command {}",
            entry.sequence
        );
    }
    Ok(engine)
}
