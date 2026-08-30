//! S0 gate: the two startup assertions (SPEC §5.2, §9.3).
//!
//! Both are inequalities that hold with room to spare in the default v1 configuration, so
//! each test constructs the violation deliberately and asserts the venue refuses to start.
//! The §9.3 test does so **with a non-zero lag term**: every term after the first is zero
//! in v1, so with all of them zero the inequality reduces to
//! `withdrawal_delay > max_quote_ttl` and a test of the four-term form would pass without
//! ever exercising three of its four terms (CLAUDE 39).

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_core::config::{Config, ConfigError, MAX_LEGS, MAX_QUOTES_PER_LEG};
use rfq_core::engine::Engine;
use rfq_core::types::Dur;

#[test]
fn the_default_configuration_starts() {
    assert_eq!(Config::default().validate(), Ok(()));
    assert!(Engine::new(Config::default()).is_ok());
}

#[test]
fn a_lag_term_alone_can_violate_the_four_term_timelock_inequality() {
    // withdrawal_delay = 40s, max_quote_ttl = 30s. Against the two-term form
    // `withdrawal_delay > MAX_QUOTE_TTL` this configuration is fine, and that is the point:
    // the shortfall lives entirely in the terms a v1-only reading would have dropped.
    let base =
        Config { withdrawal_delay: Dur(40_000), max_quote_ttl: Dur(30_000), ..Config::default() };

    // Precondition (CLAUDE 39): with every lag term zero it starts. If this failed, the
    // test below would be proving something other than what it claims.
    assert_eq!(base.confirmations, 0);
    assert_eq!(base.block_time, Dur::ZERO);
    assert_eq!(base.max_indexer_lag, Dur::ZERO);
    assert_eq!(base.max_settlement_inclusion_time, Dur::ZERO);
    assert_eq!(base.validate(), Ok(()), "the two-term form is satisfied");

    // CONFIRMATIONS × block_time: 12 × 1s = 12s. 30 + 12 > 40.
    let confirmation_lag = Config { confirmations: 12, block_time: Dur(1_000), ..base };
    assert_ne!(confirmation_lag.block_time, Dur::ZERO, "the lag term under test must be non-zero");
    assert_eq!(confirmation_lag.validate(), Err(ConfigError::WithdrawalDelayTooShort));
    assert!(matches!(Engine::new(confirmation_lag), Err(ConfigError::WithdrawalDelayTooShort)));

    // Indexer lag alone. 30 + 15 > 40.
    let indexer_lag = Config { max_indexer_lag: Dur(15_000), ..base };
    assert_ne!(indexer_lag.max_indexer_lag, Dur::ZERO);
    assert_eq!(indexer_lag.validate(), Err(ConfigError::WithdrawalDelayTooShort));

    // Inclusion time alone. 30 + 11 > 40.
    let inclusion = Config { max_settlement_inclusion_time: Dur(11_000), ..base };
    assert_ne!(inclusion.max_settlement_inclusion_time, Dur::ZERO);
    assert_eq!(inclusion.validate(), Err(ConfigError::WithdrawalDelayTooShort));

    // Each term is individually load-bearing, and together they still only just fail:
    // 30 + 3 + 4 + 3 = 40, and the inequality is strict, so equality is a refusal.
    let exactly_equal = Config {
        confirmations: 3,
        block_time: Dur(1_000),
        max_indexer_lag: Dur(4_000),
        max_settlement_inclusion_time: Dur(3_000),
        ..base
    };
    assert_eq!(exactly_equal.validate(), Err(ConfigError::WithdrawalDelayTooShort));

    // One millisecond of headroom and it starts, which pins the boundary rather than
    // asserting only that some large violation fails.
    let just_enough = Config { withdrawal_delay: Dur(40_001), ..exactly_equal };
    assert_eq!(just_enough.validate(), Ok(()));
}

#[test]
fn a_horizon_that_does_not_cover_request_ttl_plus_settling_time_fails_to_start() {
    // SPEC §5.2: MIN_HORIZON > MAX_REQUEST_TTL + MAX_SETTLING_TIME. Violated, a request
    // can be opened, quoted, accepted and settled on a contract whose stall grace has
    // already elapsed — escrow on a trade that is immediately Void-resolvable.
    let violating = Config {
        max_request_ttl: Dur(300_000),
        max_settling_time: Dur(60_000),
        min_horizon: Dur(360_000), // exactly equal: the inequality is strict.
        ..Config::default()
    };
    assert_eq!(violating.validate(), Err(ConfigError::HorizonTooShort));
    assert!(matches!(Engine::new(violating), Err(ConfigError::HorizonTooShort)));

    let shorter_still = Config { min_horizon: Dur(1_000), ..violating };
    assert_eq!(shorter_still.validate(), Err(ConfigError::HorizonTooShort));

    let just_enough = Config { min_horizon: Dur(360_001), ..violating };
    assert_eq!(just_enough.validate(), Ok(()));
}

#[test]
fn config_may_not_exceed_the_compile_time_storage_bounds() {
    // SPEC §3: the constant bounds storage, the config bounds policy. Leg arrays are
    // fixed-size and cannot take a runtime value, so a config asking for more legs than
    // storage provides is a startup failure with its own variant.
    let too_many_legs = Config { max_legs: u8::try_from(MAX_LEGS).unwrap() + 1, ..Config::default() };
    assert_eq!(too_many_legs.validate(), Err(ConfigError::MaxLegsExceedsStorage));

    let too_many_quotes = Config {
        max_quotes_per_leg: u8::try_from(MAX_QUOTES_PER_LEG).unwrap() + 1,
        ..Config::default()
    };
    assert_eq!(too_many_quotes.validate(), Err(ConfigError::MaxQuotesPerLegExceedsStorage));

    // At the bound exactly, it starts.
    let at_the_bound = Config {
        max_legs: u8::try_from(MAX_LEGS).unwrap(),
        max_quotes_per_leg: u8::try_from(MAX_QUOTES_PER_LEG).unwrap(),
        ..Config::default()
    };
    assert_eq!(at_the_bound.validate(), Ok(()));
}

#[test]
fn overflowing_terms_are_rejected_rather_than_wrapped() {
    // CLAUDE 14: overflow is a rejection, never a wrap. A wrapped sum here would compute a
    // tiny bound and cheerfully accept a timelock that covers nothing.
    let horizon_overflow =
        Config { max_request_ttl: Dur(u64::MAX), max_settling_time: Dur(1), ..Config::default() };
    assert_eq!(horizon_overflow.validate(), Err(ConfigError::HorizonTermOverflow));

    let timelock_overflow = Config {
        max_quote_ttl: Dur(u64::MAX),
        max_indexer_lag: Dur(1),
        min_horizon: Dur(u64::MAX),
        ..Config::default()
    };
    assert_eq!(timelock_overflow.validate(), Err(ConfigError::TimelockTermOverflow));
}
