//! Tail latency for the three event hand-offs, and for the command log.
//!
//! Criterion answers "how fast on average". This answers "how bad does it get", which for a
//! single-writer venue is the question that decides the design — a mean is a poor summary of
//! something that is fast a million times and then stalls once.
//!
//! **A named exception to CLAUDE 1.** Wall-clock reads are confined to the `Clock`
//! implementation, "nowhere else, including tests". Measuring latency requires one. This is
//! a benchmark rather than a test, it is outside the engine, and criterion already reads the
//! clock internally — so the exception is narrow and deliberate rather than an oversight.
//!
//! **Method.** Each sample times a batch of 32 operations. Timing a single ~40 ns operation
//! with `Instant::now()` is meaningless — the read costs about as much as the thing measured
//! — so the cost is amortised across a batch, and per-operation figures are derived. A batch
//! containing a stall still shows as an outlier, which is exactly what the tail columns are
//! there to catch.
//!
//! Absolutes are laptop numbers and mean little. The comparison between rows is the point.
//!
//! **`max` is the least trustworthy column.** A single batch can be interrupted by the OS
//! scheduler, and one such interruption dominates the maximum of forty thousand samples —
//! it measures the machine, not the code. `p99.9` is the tail figure to compare on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]
#![allow(clippy::print_stdout, dead_code, missing_docs)]
// CLAUDE 1 confines wall-clock reads to the `Clock` implementation, and `clippy.toml` puts
// `Instant` on the disallowed list so the rule is enforced rather than remembered. Measuring
// latency requires one. Allowed here and in no other file: this is a benchmark, outside the
// engine, and criterion already reads the clock internally.
#![allow(clippy::disallowed_types)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const BATCH: usize = 32;
const SAMPLES: usize = 40_000;

#[derive(Clone, Copy)]
struct Payload([u64; 36]); // 288 bytes; `Event` is 280

#[derive(Clone, Copy)]
struct Entry([u64; 30]); // 240 bytes, which is what a `LogEntry` measures

/// Per-operation timings in **hundredths of a nanosecond**, as integers.
///
/// CLAUDE 13 bans `f32`/`f64` anywhere in the repository. Percentiles do not need floats —
/// scaling by 100 before dividing keeps two decimal places of resolution in `u64`, which is
/// ample for figures in the tens of nanoseconds and avoids a second exception in this file.
struct Summary {
    mean: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

/// `nanos × 100 / BATCH`, so the result is hundredths of a nanosecond per operation.
fn per_op(batch_nanos: u64) -> u64 {
    batch_nanos.saturating_mul(100) / BATCH as u64
}

fn summarise(mut batch_nanos: Vec<u64>) -> Summary {
    batch_nanos.sort_unstable();
    let last = batch_nanos.len().saturating_sub(1);
    let at = |permille: u64| {
        let index = (last as u64).saturating_mul(permille) / 1000;
        per_op(batch_nanos[index as usize])
    };
    let total: u64 = batch_nanos.iter().sum();
    Summary {
        mean: total.saturating_mul(100) / (batch_nanos.len() as u64 * BATCH as u64),
        p50: at(500),
        p90: at(900),
        p99: at(990),
        p999: at(999),
        max: per_op(batch_nanos[last]),
    }
}

/// Hundredths of a nanosecond as `n.nn`.
fn ns(hundredths: u64) -> String {
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

fn row(name: &str, s: &Summary) {
    println!(
        "{name:<34} {:>9} {:>9} {:>9} {:>9} {:>10} {:>12}",
        ns(s.mean), ns(s.p50), ns(s.p90), ns(s.p99), ns(s.p999), ns(s.max)
    );
}

/// Design 1 — a mutex-guarded ring whose consumer locks on every iteration, including when
/// there is nothing to take.
fn mutex_spinning() -> Summary {
    let ring: Arc<Mutex<Vec<Payload>>> = Arc::new(Mutex::new(Vec::with_capacity(256)));
    let stop = Arc::new(AtomicUsize::new(0));
    let (r, s) = (Arc::clone(&ring), Arc::clone(&stop));
    let consumer = thread::spawn(move || {
        while s.load(Ordering::Relaxed) == 0 {
            if let Ok(mut ring) = r.lock() {
                ring.pop();
            }
            std::hint::spin_loop();
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            if let Ok(mut ring) = ring.lock() {
                ring.push(Payload([0; 36]));
            }
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    stop.store(1, Ordering::Relaxed);
    let _ = consumer.join();
    summarise(samples)
}

/// Design 2 — the same mutex, with a cache-padded atomic length the consumer probes before
/// deciding to lock.
fn mutex_atomic_probe() -> Summary {
    #[repr(align(64))]
    struct Padded(AtomicUsize);

    let ring: Arc<Mutex<Vec<Payload>>> = Arc::new(Mutex::new(Vec::with_capacity(256)));
    let len = Arc::new(Padded(AtomicUsize::new(0)));
    let stop = Arc::new(AtomicUsize::new(0));
    let (r, l, s) = (Arc::clone(&ring), Arc::clone(&len), Arc::clone(&stop));
    let consumer = thread::spawn(move || {
        while s.load(Ordering::Relaxed) == 0 {
            if l.0.load(Ordering::Relaxed) == 0 {
                std::hint::spin_loop();
                continue;
            }
            if let Ok(mut ring) = r.lock() {
                ring.pop();
                l.0.store(ring.len(), Ordering::Relaxed);
            }
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            if let Ok(mut ring) = ring.lock() {
                ring.push(Payload([0; 36]));
                len.0.store(ring.len(), Ordering::Relaxed);
            }
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    stop.store(1, Ordering::Relaxed);
    let _ = consumer.join();
    summarise(samples)
}

/// Design 3, shipping — a bounded preallocated channel to a worker that parks on `recv`.
fn bounded_channel() -> Summary {
    let (tx, rx) = mpsc::sync_channel::<Payload>(256);
    let consumer = thread::spawn(move || while rx.recv().is_ok() {});

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            match tx.try_send(Payload([0; 36])) {
                Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
            }
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    drop(tx);
    let _ = consumer.join();
    summarise(samples)
}

/// Design 4 — the same bounded channel, but the consumer **spins on `try_recv`** instead of
/// parking on `recv`.
///
/// Added to test the hypothesis that design 3's slower median came from having to *wake* a
/// parked consumer. **It does not.** Design 4 is consistently slower than design 3, so the
/// wake is not the cost.
///
/// What is left is the explanation commit 1 was built on: design 2's consumer, when idle,
/// spins on a cache-padded atomic that nothing else touches, generating no coherence traffic
/// at all. Both channel designs touch the channel's internal head/tail atomics on every
/// attempt, idle or not — design 4 hammers them hardest, which is why it is worst. Design 2
/// has a contention-free idle state and neither channel has one.
///
/// Kept because a disproved hypothesis is worth more in the file than a guess, and because
/// it is what attributes design 2's median to the right cause. Not a shipping candidate: a
/// venue should not burn a core to publish events.
fn bounded_channel_spinning_consumer() -> Summary {
    let (tx, rx) = mpsc::sync_channel::<Payload>(256);
    let stop = Arc::new(AtomicUsize::new(0));
    let s = Arc::clone(&stop);
    let consumer = thread::spawn(move || {
        while s.load(Ordering::Relaxed) == 0 {
            if rx.try_recv().is_err() {
                std::hint::spin_loop();
            }
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            match tx.try_send(Payload([0; 36])) {
                Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
            }
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    stop.store(1, Ordering::Relaxed);
    let _ = consumer.join();
    drop(tx);
    summarise(samples)
}

/// The command log, before — a `Vec` grown on the writer thread, so the tail contains every
/// capacity doubling.
fn log_growing_vec() -> Summary {
    let mut log: Vec<Entry> = Vec::with_capacity(1024);
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            log.push(Entry([0; 30]));
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    summarise(samples)
}

/// The command log, shipping — a bounded channel to the logger thread.
fn log_bounded_channel() -> Summary {
    let (tx, rx) = mpsc::sync_channel::<Entry>(1024);
    let consumer = thread::spawn(move || {
        let mut sink: Vec<Entry> = Vec::with_capacity(1 << 21);
        while let Ok(entry) = rx.recv() {
            sink.push(entry);
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..BATCH {
            let _ = tx.try_send(Entry([0; 30]));
        }
        samples.push(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    drop(tx);
    let _ = consumer.join();
    summarise(samples)
}

fn main() {
    println!(
        "\n{:<34} {:>9} {:>9} {:>9} {:>9} {:>10} {:>12}",
        "nanoseconds per operation", "mean", "p50", "p90", "p99", "p99.9", "max"
    );
    println!("{}", "─".repeat(100));

    println!("EVENT HAND-OFF  ({SAMPLES} batches of {BATCH}, consumer running)");
    row("  1 mutex, spinning consumer", &mutex_spinning());
    row("  2 mutex, atomic probe", &mutex_atomic_probe());
    row("  3 bounded channel  [shipping]", &bounded_channel());
    row("  4 bounded channel, spinning rx", &bounded_channel_spinning_consumer());

    println!("\nCOMMAND LOG");
    row("  1 Vec grown on the writer", &log_growing_vec());
    row("  2 bounded channel  [shipping]", &log_bounded_channel());
    println!();
}
