//! What the engine thread pays, measured rather than argued.
//!
//! The design argues its latency properties **structurally** — preallocated slabs, no
//! allocation in `apply`, intrusive chains, one writer, no locks around engine state — and
//! until now never measured any of them. That is exactly how three costs *outside* `apply`
//! survived every test in the suite: a mutex the publisher contended for, an account scan
//! proportional to the venue rather than the command, and a log that reallocated.
//!
//! Each group below compares the implementations against each other, so the numbers are a
//! *difference* rather than an absolute. Absolutes here mean little — this is a laptop, not
//! the target — but the ratio between two implementations on the same machine is the thing
//! the changes were made for.
//!
//! **The threaded groups are the least trustworthy.** Contention depends on core topology,
//! scheduling and frequency scaling, and criterion cannot control any of them. Treat a 2×
//! difference there as real and a 10% one as noise. The coverage group has no threads and is
//! the one to believe.

//! ## Results, on one laptop, and what they actually say
//!
//! ### The event hand-off — three runs, because one was misleading
//!
//! | design | r1 | r2 | r3 | |
//! |---|---|---|---|---|
//! | 1 — mutex, spinning consumer | 56.4 ns | 46.0 ns | 56.0 ns | noisy |
//! | 2 — mutex, cache-padded atomic probe | 70.6 ns | 41.5 ns | 70.5 ns | **bimodal** |
//! | 3 — bounded channel *(shipping)* | 41.0 ns | 42.0 ns | 42.3 ns | **stable** |
//!
//! Design 2 looks unstable here — bimodal, and slower than design 1 on two runs of three.
//! **That reading is a mean applied to a skewed distribution, and `benches/latency.rs`
//! corrects it**: design 2 has a *median* of 10.4 ns, five times better than either other
//! design and stable to a tenth of a nanosecond, with a long tail that drags the mean around.
//!
//! What is true is that the probe removes the consumer's contention and adds a producer-side
//! store to the padded length on every push, so the producer invalidates **two** cache lines
//! where it used to invalidate one — which shows up as the worst p99 of the three. It
//! improves the common case and worsens the tail.
//!
//! See `docs/benchmarks.md`. This file reports means; percentiles are the useful summary for
//! a single writer, and they live next door.
//!
//! Design 3 is both fastest and — more usefully — the only one whose cost is predictable.
//!
//! ### Claim coverage — no threads, so believe this one
//!
//! | | |
//! |---|---|
//! | global, whole 256-row table | 159.5 ns |
//! | scoped, the two accounts a command touches | **0.29 ns** |
//!
//! ~540×, per command, and it was running unconditionally to maintain a statistic nobody
//! read. This is the least ambiguous result in the file.
//!
//! ### The command log — the honest one
//!
//! | | per batch of 8192 | per entry |
//! |---|---|---|
//! | `Vec`, preallocated, no realloc | 31.7 µs | 3.9 ns |
//! | bounded channel to a worker | 142.5 µs | **17.4 ns** |
//!
//! **The channel is ~4× slower in mean throughput**, and that is the correct result rather
//! than a defect: a local push and a cross-thread hand-off are not the same operation. The
//! hand-off costs about 13 ns more per command.
//!
//! What it buys is the tail:
//!
//! | | one operation |
//! |---|---|
//! | one `Vec` push that triggers a realloc of 65,536 entries | **194 µs** |
//! | one channel send | **31 ns** |
//!
//! That 194 µs is a *forced* boundary push on a fresh 65,536-entry `Vec`. Measured against a
//! log growing organically, `benches/latency.rs` sees a worst batch of about 6 µs — large
//! reallocations on this platform appear to be served by virtual-memory remapping rather
//! than a physical copy, so the spike is much cheaper than the byte count suggests, and
//! platform-dependent.
//!
//! So the latency case for moving the log is **weaker than this group implies**. The
//! justification that survives measurement is that a `Vec` on the writer grows without bound
//! in the writer's own address space, and that growth belongs on a thread that can page,
//! rotate or persist it. See `docs/benchmarks.md`.
//!
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]
// Benchmark payloads are sized to match the real types and never read; `criterion_group!`
// generates an undocumented function. Neither is worth contorting the file over.
#![allow(dead_code, missing_docs)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use criterion::{Criterion, criterion_group, criterion_main};

use rfq_core::account::AccountIdx;
use rfq_core::config::Config;
use rfq_core::ledger::Ledger;

// ─────────────────────────── the event hand-off, three ways ───────────────────────────

/// A minimal stand-in for the payload. `Event` is 280 bytes; this is 288, so the memcpy is
/// comparable and the bench does not need to forge slab handles.
#[derive(Clone, Copy)]
struct Payload([u64; 36]);

impl Payload {
    const fn new() -> Self {
        Self([0; 36])
    }
}

/// **Before commit 1.** A mutex-guarded ring with a consumer that spins *while holding the
/// lock's cache line* — it locks on every iteration, including when there is nothing to do.
fn mutex_ring_spinning(c: &mut Criterion) {
    c.bench_function("handoff/1-mutex-spinning-consumer", |b| {
        let ring: Arc<Mutex<Vec<Payload>>> = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let stop = Arc::new(AtomicUsize::new(0));

        let consumer_ring = Arc::clone(&ring);
        let consumer_stop = Arc::clone(&stop);
        let consumer = thread::spawn(move || {
            while consumer_stop.load(Ordering::Relaxed) == 0 {
                // The defect: acquire the lock to discover there is nothing to do.
                if let Ok(mut ring) = consumer_ring.lock() {
                    ring.pop();
                }
                std::hint::spin_loop();
            }
        });

        b.iter(|| {
            if let Ok(mut ring) = ring.lock() {
                ring.push(Payload::new());
            }
        });

        stop.store(1, Ordering::Relaxed);
        let _ = consumer.join();
    });
}

/// **Commit 1.** Same mutex, but the consumer reads a cache-padded atomic length first and
/// only locks when there is something to take.
fn mutex_ring_atomic_probe(c: &mut Criterion) {
    #[repr(align(64))]
    struct Padded(AtomicUsize);

    c.bench_function("handoff/2-mutex-atomic-probe", |b| {
        let ring: Arc<Mutex<Vec<Payload>>> = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let len = Arc::new(Padded(AtomicUsize::new(0)));
        let stop = Arc::new(AtomicUsize::new(0));

        let consumer_ring = Arc::clone(&ring);
        let consumer_len = Arc::clone(&len);
        let consumer_stop = Arc::clone(&stop);
        let consumer = thread::spawn(move || {
            while consumer_stop.load(Ordering::Relaxed) == 0 {
                // The fix: an unshared load, no coherence traffic against the lock word.
                if consumer_len.0.load(Ordering::Relaxed) == 0 {
                    std::hint::spin_loop();
                    continue;
                }
                if let Ok(mut ring) = consumer_ring.lock() {
                    ring.pop();
                    consumer_len.0.store(ring.len(), Ordering::Relaxed);
                }
            }
        });

        b.iter(|| {
            if let Ok(mut ring) = ring.lock() {
                ring.push(Payload::new());
                len.0.store(ring.len(), Ordering::Relaxed);
            }
        });

        stop.store(1, Ordering::Relaxed);
        let _ = consumer.join();
    });
}

/// **Commit 4, shipping.** A bounded preallocated channel to a worker that parks on `recv`.
fn bounded_channel(c: &mut Criterion) {
    c.bench_function("handoff/3-bounded-channel", |b| {
        let (tx, rx) = mpsc::sync_channel::<Payload>(256);
        let consumer = thread::spawn(move || while rx.recv().is_ok() {});

        b.iter(|| match tx.try_send(Payload::new()) {
            Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
        });

        drop(tx);
        let _ = consumer.join();
    });
}

// ─────────────────────────── claim coverage, scoped vs global ───────────────────────────

/// The account table is `resize`d to `max_accounts`, so the global form walks every row
/// whether the venue has three participants or two hundred — for a command that addresses at
/// most two. No threads here, so this group is the trustworthy one.
fn claim_coverage(c: &mut Criterion) {
    let ledger = Ledger::new(&Config::default()); // 256 preallocated accounts

    let mut group = c.benchmark_group("coverage");
    group.bench_function("global-whole-table", |b| {
        b.iter(|| ledger.check_claim_coverage());
    });
    group.bench_function("scoped-two-accounts", |b| {
        b.iter(|| {
            let _ = ledger.check_claim_coverage_for(AccountIdx(0));
            let _ = ledger.check_claim_coverage_for(AccountIdx(1));
        });
    });
    group.finish();
}

// ─────────────────────────── the command log, two ways ───────────────────────────

/// 240 bytes an entry, which is what a `LogEntry` measures.
#[derive(Clone, Copy)]
struct Entry([u64; 30]);

/// The floor: 8192 pushes into a `Vec` that never reallocates. Local memory, no
/// synchronisation — nothing can beat this, and it is here so the others have a baseline.
fn log_vec_preallocated(c: &mut Criterion) {
    c.bench_function("log/1-vec-preallocated-8192", |b| {
        b.iter(|| {
            let mut log: Vec<Entry> = Vec::with_capacity(8192);
            for _ in 0..8192 {
                log.push(Entry([0; 30]));
            }
            log.len()
        });
    });
}

/// **Shipping.** 8192 hand-offs across a bounded channel to a live consumer.
///
/// This is **not** the same operation as the one above and is expected to lose: a local push
/// versus a cross-thread hand-off. The difference is what it costs to move growth and I/O
/// off the writer, and the interesting number is per entry, not for the batch.
fn log_bounded_channel(c: &mut Criterion) {
    c.bench_function("log/2-bounded-channel-8192", |b| {
        let (tx, rx) = mpsc::sync_channel::<Entry>(1024);
        let consumer = thread::spawn(move || {
            let mut sink: Vec<Entry> = Vec::with_capacity(8192);
            while let Ok(entry) = rx.recv() {
                sink.push(entry);
            }
            sink.len()
        });

        b.iter(|| {
            for _ in 0..8192 {
                let _ = tx.try_send(Entry([0; 30]));
            }
        });

        drop(tx);
        let _ = consumer.join();
    });
}

/// **The tail, which is the whole reason the log moved.** One push into a `Vec` that is
/// exactly full, so this single call reallocates and copies 65,536 entries — what the
/// writer thread used to do at every capacity doubling, in the middle of a command.
///
/// Compare against `single-channel-send` below: the same one operation, bounded.
fn log_push_at_realloc_boundary(c: &mut Criterion) {
    c.bench_function("log/3-single-push-at-realloc-boundary", |b| {
        b.iter_batched(
            || {
                let mut log: Vec<Entry> = Vec::with_capacity(65_536);
                log.resize(65_536, Entry([0; 30])); // len == capacity: the next push reallocates
                log
            },
            |mut log| {
                log.push(Entry([0; 30]));
                log.len()
            },
            criterion::BatchSize::LargeInput,
        );
    });
}

/// The same single operation on the shipping path: one hand-off, always the same cost.
fn log_single_send(c: &mut Criterion) {
    c.bench_function("log/4-single-channel-send", |b| {
        let (tx, rx) = mpsc::sync_channel::<Entry>(1024);
        let consumer = thread::spawn(move || while rx.recv().is_ok() {});
        b.iter(|| {
            let _ = tx.try_send(Entry([0; 30]));
        });
        drop(tx);
        let _ = consumer.join();
    });
}

criterion_group!(
    benches,
    mutex_ring_spinning,
    mutex_ring_atomic_probe,
    bounded_channel,
    claim_coverage,
    log_vec_preallocated,
    log_bounded_channel,
    log_push_at_realloc_boundary,
    log_single_send
);
criterion_main!(benches);
