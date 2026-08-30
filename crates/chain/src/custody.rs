//! The custody mock (skeleton).
//!
//! An in-process model of an escrow contract (SPEC §9). It holds balances and escrows; the
//! engine holds only escrow ids and cannot reach into them.
//!
//! **Custody validates against its own clock, not the engine's** (SPEC §9.1). In v1 the
//! two are separate `Clock` instances that happen to agree, and the mock allows an offset
//! so a test can demonstrate the divergence case: a quote the engine believes live is
//! expired at the custody layer, the transaction reverts, and the basket aborts safely.
//! Without a second clock that assumption is invisible in v1 and false in v2, and no test
//! could ever surface it — which is why the clock is here from S0 rather than added when
//! settlement is written.

use rfq_core::clock::Clock;
use rfq_core::types::Ts;

/// The escrow contract, as a local mock.
///
/// Generic over its clock so that production wiring takes a monotonic clock and tests take
/// a settable one. [`Custody::clock_mut`] is how a test injects divergence, and it can
/// only be used to *move* a clock that is movable: a custody built on a monotonic clock
/// exposes no way to advance it, because the type has no such method.
#[derive(Debug)]
pub struct Custody<C: Clock> {
    clock: C,
    withdrawal_delay: Ts,
}

impl<C: Clock> Custody<C> {
    /// Construct custody with its own clock and its withdrawal timelock.
    ///
    /// The delay is passed as a bare duration rather than as the venue's whole `Config`:
    /// custody enforces the timelock but has no business knowing how many legs a request
    /// may carry. The four-term inequality that relates this delay to the engine's quote
    /// lifetime spans both systems and is therefore asserted once, at startup, by whatever
    /// constructs them both (SPEC §9.3).
    pub const fn new(clock: C, withdrawal_delay: Ts) -> Self {
        Self { clock, withdrawal_delay }
    }

    /// Chain time. Sampled once per settlement transaction (CLAUDE 2), never by the engine.
    pub fn now(&self) -> Ts {
        self.clock.now()
    }

    /// How long `RequestWithdrawal` waits before execution (SPEC §9.3).
    pub const fn withdrawal_delay(&self) -> Ts {
        self.withdrawal_delay
    }

    /// Custody's clock, for the harness to advance independently of the engine's.
    pub const fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }
}
