//! Clock authority (SPEC §4.1).
//!
//! A monotonic clock owned by the venue and injected as a trait. `Instant::now()` never
//! appears outside a `Clock` implementation (CLAUDE 1), and there is exactly one such
//! implementation that reads a wall clock — `MonotonicClock`, in `rfq-runtime`.
//!
//! There are **two** clocks in the system, not one: the venue's and custody's. Custody is
//! a separate system being mocked and holds its own instance, sampled once per settlement
//! transaction (SPEC §9.1). The divergence between venue time and chain time is a real
//! property the design must expose, so it is representable from S0 rather than retrofitted.
//! Neither clock can read the other; only the harness (SPEC §13.1) advances both.

use crate::types::{Dur, Ts};

/// A source of the current time, in milliseconds (SPEC §4.0).
///
/// `apply(cmd, now)` samples this **once**, at the call site, and every predicate
/// evaluated during the command uses that one value (CLAUDE 2). The engine therefore does
/// not hold a clock at all: the runtime samples it and passes the value in, which makes
/// re-reading mid-command impossible rather than merely forbidden.
pub trait Clock {
    /// The current time.
    fn now(&self) -> Ts;
}

/// A clock the test moves by hand.
///
/// Every time-dependent test advances this instead of sleeping (CLAUDE 27, 43). It reads
/// no wall clock, which is why it lives in `core` alongside the trait rather than beside
/// `MonotonicClock` in the runtime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestClock {
    now: Ts,
}

impl TestClock {
    /// A clock reading `now`.
    #[must_use]
    pub const fn at(now: Ts) -> Self {
        Self { now }
    }

    /// Move the clock forward by `duration`, clamped at the end of time.
    ///
    /// Takes a [`Dur`], not a [`Ts`]: advancing a clock *by an instant* is the confusion
    /// SPEC §4.0's two types exist to make unsayable.
    pub const fn advance(&mut self, duration: Dur) {
        self.now = self.now.saturating_add(duration);
    }

    /// Move the clock to `now`.
    ///
    /// # Panics
    ///
    /// In debug builds, if `now` is earlier than the current reading. SPEC §4.1 calls for
    /// a monotonic clock; a test that walks time backwards is testing something the
    /// production clock cannot do, and its result would not transfer.
    pub const fn set(&mut self, now: Ts) {
        debug_assert!(now.0 >= self.now.0, "TestClock must not run backwards (SPEC §4.1)");
        self.now = now;
    }
}

impl Clock for TestClock {
    fn now(&self) -> Ts {
        self.now
    }
}
