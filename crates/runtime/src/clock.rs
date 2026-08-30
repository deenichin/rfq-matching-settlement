//! The one wall clock in the repository (CLAUDE 1, SPEC §4.1).
//!
//! `Instant` is a disallowed type workspace-wide; this module carries the single `allow`
//! that lifts the ban, so `grep` for it finds every place a wall clock is read. Everything
//! else in the system — engine, custody, harness, every test — takes time as a value.

// CLAUDE 1: wall-clock reads appear only inside a Clock implementation, and this is it.
#![allow(clippy::disallowed_types)]

use std::time::Instant;

use rfq_core::clock::Clock;
use rfq_core::types::Ts;

/// A monotonic clock in milliseconds since construction (SPEC §4.0).
///
/// `Instant` is used rather than `SystemTime` because SPEC §4.1 asks for monotonicity, and
/// a wall clock that can step backwards over an NTP correction would make an expired quote
/// live again. The epoch is arbitrary by design: no participant-supplied timestamp is ever
/// compared against it, because participant timestamps are advisory and never trusted.
#[derive(Debug)]
pub struct MonotonicClock {
    origin: Instant,
    epoch: Ts,
}

impl MonotonicClock {
    /// A clock reading zero now.
    #[must_use]
    pub fn new() -> Self {
        Self::starting_at(Ts::ZERO)
    }

    /// A clock reading `epoch` now.
    ///
    /// Two instances constructed at different moments read differently, which is what the
    /// engine and custody want: they are separate systems and their clocks are allowed to
    /// disagree (SPEC §9.1).
    #[must_use]
    pub fn starting_at(epoch: Ts) -> Self {
        Self { origin: Instant::now(), epoch }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Ts {
        // `as_millis` is u128; a process running long enough to overflow u64 milliseconds
        // is 584 million years old. Saturating rather than wrapping, because a clock that
        // wraps runs backwards and SPEC §4.1 requires monotonic.
        let elapsed_ms = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.epoch.saturating_add(Ts(elapsed_ms))
    }
}
