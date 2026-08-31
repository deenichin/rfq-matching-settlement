//! The event ring as the two threads actually share it.
//!
//! [`EventRing`](crate::event_ring::EventRing) is the data structure; this is the handshake
//! around it. The engine appends a batch after each command and the publisher drains it, and
//! both need two facts — whether anything is waiting, and whether the engine has finished —
//! without either stalling the other.
//!
//! **Why the length and the shutdown flag live outside the mutex.** `Mutex::lock` is a
//! read-modify-write: it takes the cache line holding the lock word into an exclusive state,
//! which invalidates every other core's copy. A publisher that locks in order to discover
//! the ring is empty therefore invalidates that line as fast as the core will let it, while
//! doing no work at all — and the engine, the one thread whose latency the design exists to
//! protect, has to win an atomic contest against that on every command it emits from. An
//! atomic *load* needs the line only in a shared state and generates no coherence traffic,
//! so a publisher that checks before locking costs the engine nothing while it is idle.
//!
//! Both atomics are padded to their own cache line. Sharing one with the lock word would
//! reintroduce exactly the invalidation this exists to remove: the engine's `lock` would
//! evict the publisher's copy of the value it is spinning on.
//!
//! **Two alternatives, both larger than this one.**
//!
//! A bounded channel — `sync_channel` plus `try_send` — removes the mutex and the spin
//! together. The engine cannot block by construction, the publisher parks on `recv`, and
//! dropping the sender terminates it, so the shutdown flag disappears rather than moving.
//! It costs the drop-oldest policy: a full channel refuses the *newest* event instead of
//! evicting the oldest, which leaves a persistently slow consumer's staleness unbounded
//! where the ring bounds it at capacity. That is a policy change and belongs in the spec,
//! not in a performance fix.
//!
//! A lock-free single-producer, single-consumer ring removes the lock entirely. Drop-oldest
//! is what stops the textbook version working: the producer would have to advance the
//! *consumer's* cursor, and the reason SPSC needs no synchronisation is precisely that
//! neither side writes the other's. The version that does work stamps every slot with its
//! sequence and lets the consumer detect an overwrite by re-reading the stamp — which needs
//! either `UnsafeCell` (forbidden, CLAUDE 25) or a dependency (forbidden, CLAUDE 36).
//!
//! This change needs neither and leaves the policy alone.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::event_ring::{EventRing, Sequenced};

/// A value on a cache line of its own.
///
/// 64 bytes is the line size on every target this runs on. Over-aligning costs padding and
/// nothing else; under-aligning costs the whole point of this module.
#[derive(Debug, Default)]
#[repr(align(64))]
struct CacheLine<T>(T);

/// The event ring, plus the two facts the publisher needs before it decides to lock.
#[derive(Debug)]
pub struct SharedRing<T: Clone> {
    ring: Mutex<EventRing<T>>,
    /// Mirrors `ring.len()`. Written only while the lock is held, so a plain store is
    /// enough and no read-modify-write is ever needed.
    len: CacheLine<AtomicUsize>,
    /// Set once, after the engine thread has already finished.
    shutdown: CacheLine<AtomicBool>,
}

impl<T: Clone> SharedRing<T> {
    /// A shared ring holding `capacity` events.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ring: Mutex::new(EventRing::with_capacity(capacity)),
            len: CacheLine(AtomicUsize::new(0)),
            shutdown: CacheLine(AtomicBool::new(false)),
        }
    }

    /// Append a batch, returning how many events were taken.
    ///
    /// One lock acquisition per batch rather than per event: the engine pays for the
    /// handshake once per command, however many events that command emitted.
    pub fn append<I: IntoIterator<Item = T>>(&self, events: I) -> u64 {
        let Ok(mut ring) = self.ring.lock() else { return 0 };
        let mut appended = 0_u64;
        for event in events {
            ring.push(event);
            appended = appended.saturating_add(1);
        }
        self.len.0.store(ring.len(), Ordering::Relaxed);
        appended
    }

    /// Take the oldest event, if there is one.
    pub fn pop(&self) -> Option<Sequenced<T>> {
        let mut ring = self.ring.lock().ok()?;
        let taken = ring.pop();
        self.len.0.store(ring.len(), Ordering::Relaxed);
        taken
    }

    /// Whether the ring is empty, **without touching the mutex**.
    ///
    /// `Relaxed` is sufficient and deliberate. This value carries no data: every read of the
    /// ring still happens under the lock, which supplies the happens-before edges. It is a
    /// hint about whether locking is worth it, and both ways of being wrong are harmless —
    /// a stale zero costs one more spin, and a stale non-zero costs one `pop` that returns
    /// `None`. A stronger ordering would emit a barrier on ARM to buy nothing.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.len.0.load(Ordering::Relaxed) == 0
    }

    /// Whether the engine has finished. `Acquire`, because this one *is* ordered against
    /// the engine thread having ended.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.shutdown.0.load(Ordering::Acquire)
    }

    /// Tell the publisher there will be no more events.
    ///
    /// Called only after the engine thread has been joined, so nothing can arrive after it.
    pub fn finish(&self) {
        self.shutdown.0.store(true, Ordering::Release);
    }

    /// Events evicted unread.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.ring.lock().map_or(0, |ring| ring.dropped())
    }

    /// How many events are waiting, read under the lock.
    ///
    /// The authoritative answer, as opposed to [`is_idle`](Self::is_idle)'s hint.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.lock().map_or(0, |ring| ring.len())
    }

    /// Whether the ring is empty, read under the lock.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
