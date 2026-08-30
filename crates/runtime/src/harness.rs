//! The harness (SPEC §13.1, CLAUDE 8c).
//!
//! Two systems that cannot see each other still need something that can see both. The
//! harness owns one engine and one custody instance, advances **both clocks
//! independently**, and drives the wires between them: engine events to the settlement
//! adapter, chain log to the indexer. It is the only place global conservation and claim
//! coverage can be asserted (SPEC §2.2), because those assertions span both halves and no
//! function inside either half can compute them.
//!
//! Two constraints, and they are the reason this type exists rather than a pair of `pub`
//! fields somewhere convenient:
//!
//! - **It is not a back door.** It may read both systems; no engine code path may. If a
//!   production path ever needs something only the harness can see, that is a design error,
//!   not a convenience.
//! - **It has no production counterpart.** In v2 its wiring is replaced by real transport
//!   and its cross-system assertions become the reconciler (SPEC §12) — a monitoring
//!   component that can *report* divergence, not an oracle of truth that prevents it.
//!
//! Stage S0 delivers the skeleton: both systems owned, both clocks advancing separately,
//! and the cross-system assertion hooks present but empty. They are empty because there is
//! nothing yet to assert — custody has no balances until S3 — and they are *present*
//! because the alternative is discovering in S3 that conservation has nowhere to live.

use rfq_chain::custody::Custody;
use rfq_core::clock::Clock;
use rfq_core::config::{Config, ConfigError};
use rfq_core::engine::Engine;
use rfq_core::types::Ts;

/// One engine, one custody, two clocks.
///
/// Generic over both clocks so that the divergence of SPEC §9.1 is expressible by type:
/// a test builds `Harness<TestClock, TestClock>` and moves them apart, while a scenario
/// binary can build one on monotonic clocks and get the same wiring.
#[derive(Debug)]
pub struct Harness<EC: Clock, CC: Clock> {
    engine: Engine,
    /// The **venue's** clock. Held here rather than inside the engine because
    /// `apply(cmd, now)` samples time once at the call site (CLAUDE 2); the engine has no
    /// clock to re-read.
    engine_clock: EC,
    /// Custody holds the **chain's** clock itself, and neither system can read the other's.
    custody: Custody<CC>,
}

impl<EC: Clock, CC: Clock> Harness<EC, CC> {
    /// Validate the configuration and construct both systems.
    ///
    /// The two startup assertions of SPEC §5.2 and §9.3 are checked here, and this is the
    /// natural place for the second of them: the four-term timelock inequality relates the
    /// engine's `MAX_QUOTE_TTL` to custody's `WITHDRAWAL_DELAY`, so it is a statement about
    /// the pair. Neither system alone can check it, for the same reason neither alone can
    /// check conservation.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`]. A venue whose timelock does not cover the maximum quote
    /// lifetime plus mirror lag must not start.
    pub fn new(config: Config, engine_clock: EC, custody_clock: CC) -> Result<Self, ConfigError> {
        let engine = Engine::new(config)?;
        let custody = Custody::new(custody_clock, config.withdrawal_delay);
        Ok(Self { engine, engine_clock, custody })
    }

    /// Venue time, sampled once — this is the `now` that would be passed to `apply`.
    pub fn engine_now(&self) -> Ts {
        self.engine_clock.now()
    }

    /// Chain time. Deliberately a separate reading; the two may disagree.
    pub fn custody_now(&self) -> Ts {
        self.custody.now()
    }

    /// The venue's clock, for a test to advance.
    pub const fn engine_clock_mut(&mut self) -> &mut EC {
        &mut self.engine_clock
    }

    /// The chain's clock, for a test to advance — independently, and by a different amount.
    pub const fn custody_clock_mut(&mut self) -> &mut CC {
        self.custody.clock_mut()
    }

    /// The engine, read-only.
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Custody, read-only.
    pub const fn custody(&self) -> &Custody<CC> {
        &self.custody
    }

    /// Every cross-system invariant, run after every command in tests and scenarios
    /// (SPEC §15, invariants 5–8).
    pub fn assert_cross_system_invariants(&self) {
        self.assert_conservation();
        self.assert_claim_coverage();
        self.assert_mirror_agreement();
    }

    /// SPEC §15.5 — `Σ custody.free + Σ notional over Locked escrows == deposited − withdrawn`.
    ///
    /// Purely custody-side: `reserved` and `committed` are claims *against* `free`, not
    /// partitions of it, and adding them here would double-count. Only `Locked` escrows are
    /// counted — a `Settled` escrow has already paid out, and summing every escrow makes
    /// the first payout read as newly created money (CLAUDE 41).
    ///
    /// Empty until S3, when custody acquires balances and escrows.
    #[allow(clippy::unused_self)] // S3 fills this in; the hook exists so it has a home.
    fn assert_conservation(&self) {}

    /// SPEC §15.6 — `∀ a: custody.free(a) ≥ reserved(a) + committed(a)`.
    ///
    /// A violation means the engine has promised capital custody does not hold, which is
    /// exactly the failure the §9.3 withdrawal timelock exists to prevent.
    ///
    /// Empty until S3.
    #[allow(clippy::unused_self)] // S3.
    fn assert_claim_coverage(&self) {}

    /// SPEC §15.7 — `mirror.free(a) == custody.free(a)` for all accounts; exact in v1,
    /// bounded by the §9.3 lag terms in v2.
    ///
    /// Empty until S3.
    #[allow(clippy::unused_self)] // S3.
    fn assert_mirror_agreement(&self) {}
}
