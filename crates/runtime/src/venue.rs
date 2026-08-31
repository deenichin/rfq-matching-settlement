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
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use rfq_core::clock::Clock;
use rfq_core::command::Command;
use rfq_core::config::Config;
use rfq_core::engine::{Engine, EngineError};
use rfq_core::event::{Event, EventBuffer};
use rfq_core::types::Ts;

use crate::event_ring::SequencedEvent;
use crate::shared_ring::SharedRing;

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
    /// How many commands were applied, and therefore how many times the debug assertion on
    /// claim coverage ran. Asserting the count is what keeps "coverage held throughout" from
    /// being satisfied by never checking (CLAUDE 39).
    pub coverage_checks: u64,
    /// Events evicted from the ring unread.
    pub events_dropped: u64,
    /// Events the engine emitted. Counted so a test can assert the engine *did* emit —
    /// an event path that carries nothing proves nothing about the publisher.
    pub events_emitted: u64,
}

/// A running venue: one engine thread, one publisher thread, one bounded command channel.
#[derive(Debug)]
pub struct Venue {
    commands: SyncSender<Command>,
    engine_thread: JoinHandle<EngineRound>,
    publisher_thread: JoinHandle<()>,
    logger_thread: JoinHandle<Vec<LogEntry>>,
    ring: Arc<SharedRing<Event>>,
}

/// What the engine thread returns. The command log is not part of it: the engine hands
/// entries to the logger and never owns the growing collection (see [`run_logger`]).
#[derive(Debug)]
struct EngineRound {
    engine: Engine,
    coverage_checks: u64,
    events_dropped: u64,
    events_emitted: u64,
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
        let ring = Arc::new(SharedRing::with_capacity(capacities.event_ring));

        // Unbounded, and deliberately: a bounded log channel would either stall the engine
        // when full — the one thing SPEC §13 forbids — or drop entries, which breaks the
        // replay the log exists for. Unbounded moves the growth to a thread that is not
        // latency-critical, and costs the engine one small constant-size send per command
        // instead of an occasional quarter-megabyte reallocation.
        //
        // **This is a small improvement, and it is worth being exact about how small.** It
        // changes the shape of the allocation rather than removing it. An unbounded channel
        // has to allocate to hold an arbitrary number of entries, so a send is a slot write
        // plus a block allocation every few dozen commands. What it buys is that the worst
        // single command is now one small block instead of a reallocate-and-copy of the
        // whole log — bounded rather than proportional to how long the venue has been up —
        // and that the unbounded growth belongs to a thread with nothing to be late for.
        // The mean cost went slightly *up*: an atomic and an amortised malloc where there
        // used to be a bare memcpy into a preallocated slot. That trade is right for a
        // writer whose tail matters, and it is not the same claim as allocation-free.
        //
        // See `run_logger` for what allocation-free would actually take.
        let (entries, journal) = mpsc::channel::<LogEntry>();
        let logger_thread = thread::Builder::new()
            .name("logger".to_owned())
            .spawn(move || run_logger(&journal, capacities.command_log))
            .unwrap_or_else(|error| panic!("the logger thread must start: {error}"));

        let engine_ring = Arc::clone(&ring);
        let engine_thread = thread::Builder::new()
            .name("engine".to_owned())
            .spawn(move || {
                // The clock is owned by this closure and therefore by the engine thread.
                // Nothing else can read it, so there is one clock and one sample per command.
                run_engine(engine, &clock, &inbox, &engine_ring, &entries, capacities)
            })
            .unwrap_or_else(|error| panic!("the engine thread must start: {error}"));

        let publisher_ring = Arc::clone(&ring);
        let publisher_thread = thread::Builder::new()
            .name("publisher".to_owned())
            .spawn(move || run_publisher(sink, &publisher_ring))
            .unwrap_or_else(|error| panic!("the publisher thread must start: {error}"));

        Ok(Self { commands, engine_thread, publisher_thread, logger_thread, ring })
    }

    /// A handle for a client thread to submit on. Cloneable: many producers, one consumer.
    #[must_use]
    pub fn commands(&self) -> SyncSender<Command> {
        self.commands.clone()
    }

    /// The event ring, for a test standing in for the emissions S2 will produce.
    #[must_use]
    pub fn ring(&self) -> Arc<SharedRing<Event>> {
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
        let Self { commands, engine_thread, publisher_thread, logger_thread, ring } = self;
        drop(commands);
        let round = engine_thread
            .join()
            .unwrap_or_else(|_| panic!("the engine thread must not panic; it owns the state"));
        // The engine dropped its sender when it returned, so the logger's channel is closed
        // and it has already retired with the complete log.
        let log = logger_thread.join().unwrap_or_default();
        // The publisher is asked to stop only after the engine is done, so the engine is
        // never waiting on it. A publisher that died already is joined here as an Err and
        // deliberately ignored: the audit trail is best-effort (SPEC §13).
        ring.finish();
        let _ = publisher_thread.join();
        RoundOutcome {
            engine: round.engine,
            log,
            coverage_checks: round.coverage_checks,
            events_dropped: round.events_dropped,
            events_emitted: round.events_emitted,
        }
    }
}

/// The single writer. Everything that mutates engine state happens on this thread.
fn run_engine<C: Clock>(
    mut engine: Engine,
    clock: &C,
    inbox: &mpsc::Receiver<Command>,
    ring: &SharedRing<Event>,
    entries: &mpsc::Sender<LogEntry>,
    capacities: RuntimeCapacities,
) -> EngineRound {
    let mut events = EventBuffer::with_capacity(capacities.event_buffer);
    let mut sequence: u64 = 0;
    let mut coverage_checks: u64 = 0;
    let mut events_emitted: u64 = 0;

    loop {
        match inbox.try_recv() {
            Ok(command) => {
                // Sampled once, here, at the call site (CLAUDE 2). Nothing downstream can
                // re-read it, because nothing downstream holds the clock.
                let now = clock.now();
                events.clear();
                let outcome = engine.apply(command, now, &mut events);

                // Handed off, not accumulated. A `Vec` that grows on this thread turns one
                // push in every capacity-doubling into a reallocate-and-copy of the whole
                // log — amortised O(1), and a tail-latency spike exactly where the design
                // cares about the tail. The send is constant-size and the growth belongs to
                // a thread with nothing to be late for. A logger that has gone away is
                // ignored: the audit trail is best-effort and the state machine is
                // authoritative (SPEC §13).
                let _ = entries.send(LogEntry { sequence, at: now, command, outcome });
                sequence = sequence.saturating_add(1);

                // Events move to the ring *outside* apply. The engine never touches the
                // lock and never waits on the publisher — and because the publisher checks
                // an unshared atomic before locking, this acquisition is uncontended
                // whenever the publisher is idle, which is most of the time.
                if !events.is_empty() {
                    events_emitted =
                        events_emitted.saturating_add(ring.append(events.drain()));
                }

                // Claim coverage is an invariant, and an invariant check is not production
                // work. SPEC §15's table places it in "the harness, after every command, in
                // tests and scenarios"; this loop is neither. It ran here unconditionally,
                // scanning all `max_accounts` preallocated rows — 256 by default — for a
                // command that addresses at most two, and then only counted what it found.
                //
                // It is now a debug assertion over the accounts the command actually names,
                // so it still fires on every command under test and compiles out of release
                // entirely. It halts rather than tallies: a broken invariant is not a
                // statistic.
                //
                // Skipped after a mirror update, and that exclusion is the point rather than
                // a convenience. `CreditAccount` carries custody's *availability*, which
                // drops the moment a withdrawal is requested — so it can legitimately put
                // the mirror below claims the engine already holds. Asserting through that
                // window would fail on a correct system, which CLAUDE 41 names as worse than
                // not asserting at all. The form that does hold there spans both systems and
                // is the harness's.
                coverage_checks = coverage_checks.saturating_add(1);
                debug_assert!(
                    matches!(command, Command::CreditAccount { .. })
                        || engine
                            .touched_accounts(command)
                            .iter()
                            .flatten()
                            .all(|account| {
                                engine.ledger().check_claim_coverage_for(*account).is_ok()
                            }),
                    "claim coverage broke on {command:?}"
                );
            }
            // Busy-spin. The venue has one writer and no reason to park it.
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
            Err(TryRecvError::Disconnected) => break,
        }
    }

    let events_dropped = ring.dropped();
    EngineRound { engine, coverage_checks, events_dropped, events_emitted }
}

/// Owns the command log. Receives entries and never touches engine state.
///
/// The log has to be **complete** — `replay` reproduces engine state from it, and SPEC §12
/// names roll-back-and-reapply as the recovery for a reorg deeper than the confirmation
/// depth. A lossy log serves neither, which is why the channel is unbounded and this thread
/// simply absorbs whatever arrives. Where it grows without limit is still a problem, but it
/// is now a problem belonging to a thread that can page, rotate or persist without anybody
/// waiting on it.
///
/// # What this would be with a dependency, or with `unsafe`
///
/// A genuinely allocation-free writer is reachable and this is not it. The shape is a
/// **preallocated single-producer ring of `LogEntry` slots**: the engine writes into a slot
/// it already owns and publishes an index, and this thread copies out and does its own
/// growth off the hot path. That is allocation-free on the writer *and* still complete,
/// because the growth moves rather than disappears — which is the property the channel
/// version does not have.
///
/// Two things keep it out of this build. `crossbeam_queue::ArrayQueue` is the ready-made
/// version and is a dependency (CLAUDE 36); hand-rolling the equivalent needs `UnsafeCell`
/// for the slots, because the producer holds `&mut` to one while the consumer holds `&` to
/// another, and safe Rust cannot express that (CLAUDE 25). In production the answer is the
/// former — an audited, loom-tested queue rather than a bespoke one — or an in-house ring
/// where the slot layout is chosen for the entry rather than for a generic `T`.
///
/// It also needs an answer for a full ring, and that answer is awkward on purpose: blocking
/// the engine is what SPEC §13 forbids, and dropping breaks the replay the log exists for.
/// A logger that does nothing but a memcpy will not fall behind a single writer, so the
/// overflow branch would be a "cannot happen" that still has to be written and defended —
/// which is its own reason to reach for a queue somebody else already argues about.
///
/// `crossbeam-channel` would also be the production choice for the unbounded form used
/// here, being faster than `std`'s for the same semantics; it is excluded by the same rule.
fn run_logger(journal: &mpsc::Receiver<LogEntry>, capacity: usize) -> Vec<LogEntry> {
    let mut log: Vec<LogEntry> = Vec::with_capacity(capacity);
    // Ends when the engine drops its sender, which happens when `run_engine` returns.
    while let Ok(entry) = journal.recv() {
        log.push(entry);
    }
    log
}

/// Drains the ring and performs all I/O. Owns no engine state and writes none.
fn run_publisher<S: EventSink>(mut sink: S, ring: &SharedRing<Event>) {
    loop {
        // The idle probe first, and it touches no lock. Locking in order to discover there
        // is nothing to do is what made this loop contend with the engine on every command.
        if ring.is_idle() {
            if ring.is_finished() {
                // `is_idle` is a relaxed read and may lag a final append, so the last look
                // is taken under the lock. `finish` is called only after the engine thread
                // has been joined, so nothing can arrive after this drain.
                while let Some(event) = ring.pop() {
                    sink.publish(event);
                }
                return;
            }
            std::hint::spin_loop();
            continue;
        }
        match ring.pop() {
            Some(event) => sink.publish(event),
            // Lost the race to nobody: the ring emptied between the probe and the lock.
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
