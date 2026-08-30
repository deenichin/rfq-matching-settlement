//! S0 gate: the harness owns both systems and the two clocks are genuinely independent.
//!
//! Clock divergence between the venue and the chain is a real property the design has to
//! expose (SPEC §9.1). In v1 the two instances happen to agree, so a build with one shared
//! clock would look identical and every later test of the divergence case would be
//! impossible to write. These tests pin the separation while it is still cheap.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_core::clock::{Clock, TestClock};
use rfq_core::config::{Config, ConfigError};
use rfq_core::types::Ts;
use rfq_runtime::clock::MonotonicClock;
use rfq_runtime::harness::Harness;

fn harness() -> Harness<TestClock, TestClock> {
    Harness::new(Config::default(), TestClock::at(Ts::ZERO), TestClock::at(Ts::ZERO)).unwrap()
}

#[test]
fn the_two_clocks_advance_independently() {
    let mut harness = harness();
    assert_eq!(harness.engine_now(), Ts::ZERO);
    assert_eq!(harness.custody_now(), Ts::ZERO);

    harness.engine_clock_mut().advance(Ts(1_000));
    assert_eq!(harness.engine_now(), Ts(1_000));
    assert_eq!(harness.custody_now(), Ts::ZERO, "advancing venue time must not move chain time");

    harness.custody_clock_mut().advance(Ts(2_500));
    assert_eq!(harness.custody_now(), Ts(2_500));
    assert_eq!(harness.engine_now(), Ts(1_000), "advancing chain time must not move venue time");

    // The precondition every later divergence test rests on (CLAUDE 39): the two readings
    // can actually differ. If this ever passes trivially, the clocks have been merged.
    assert_ne!(harness.engine_now(), harness.custody_now());
}

#[test]
fn custody_can_run_ahead_of_the_venue() {
    // SPEC §9.1's divergence case in its dangerous direction: chain time ahead means a
    // quote the engine believes live is already expired at settlement. S3 gate (c2) turns
    // this into a settlement that reverts; S0 only has to make it representable.
    let mut harness = harness();
    harness.engine_clock_mut().advance(Ts(5_000));
    harness.custody_clock_mut().advance(Ts(8_000));

    assert!(harness.custody_now() > harness.engine_now());
    assert_eq!(harness.custody_now().saturating_sub(harness.engine_now()), Ts(3_000));
}

#[test]
fn the_harness_refuses_to_start_on_a_configuration_that_fails_its_assertions() {
    // The four-term inequality is a statement about the *pair* of systems: MAX_QUOTE_TTL
    // is the engine's, WITHDRAWAL_DELAY is custody's. The harness is where they meet, so
    // it is where the refusal has to happen.
    let violating = Config {
        withdrawal_delay: Ts(40_000),
        max_quote_ttl: Ts(30_000),
        max_indexer_lag: Ts(15_000),
        ..Config::default()
    };
    assert_ne!(violating.max_indexer_lag, Ts::ZERO, "the lag term under test must be non-zero");

    let refused = Harness::new(violating, TestClock::at(Ts::ZERO), TestClock::at(Ts::ZERO));
    assert!(matches!(refused, Err(ConfigError::WithdrawalDelayTooShort)));

    let short_horizon = Config { min_horizon: Ts(1_000), ..Config::default() };
    let refused = Harness::new(short_horizon, TestClock::at(Ts::ZERO), TestClock::at(Ts::ZERO));
    assert!(matches!(refused, Err(ConfigError::HorizonTooShort)));
}

#[test]
fn custody_holds_its_own_clock_instance() {
    // Not the same clock passed twice: custody was handed its own and reads it itself.
    let mut harness =
        Harness::new(Config::default(), TestClock::at(Ts(100)), TestClock::at(Ts(7))).unwrap();
    assert_eq!(harness.engine_now(), Ts(100));
    assert_eq!(harness.custody_now(), Ts(7));
    assert_eq!(harness.custody().now(), Ts(7));

    harness.custody_clock_mut().set(Ts(9));
    assert_eq!(harness.custody().now(), Ts(9));
    assert_eq!(harness.engine_now(), Ts(100));
}

#[test]
fn the_monotonic_clock_does_not_run_backwards() {
    // The one wall-clock implementation. No sleeping (CLAUDE 27, 43) — this asserts the
    // ordering property only, which is all the type promises.
    let clock = MonotonicClock::starting_at(Ts(1_000));
    let first = clock.now();
    let second = clock.now();
    assert!(second >= first);
    assert!(first >= Ts(1_000), "the epoch is the floor");
}
