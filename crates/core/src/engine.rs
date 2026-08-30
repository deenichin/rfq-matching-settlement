//! The engine (skeleton).
//!
//! Holds requests, quotes, reservations, claims and the read-only balance mirror. It
//! holds no reference to custody and cannot read a balance from it; it sees `EscrowId` and
//! its own mirror, nothing else (SPEC §13.1).
//!
//! Note what the engine does **not** own: a clock. `apply(cmd, now)` samples time exactly
//! once at the call site (CLAUDE 2), so the runtime samples and passes the value in.
//! Re-reading the clock mid-command is therefore not forbidden by convention — it is
//! unreachable, because the engine has nothing to re-read.
//!
//! State lands in S1 (ledger and reservations) and S2 (requests, quotes, selection).

use crate::config::{Config, ConfigError};

/// The single-writer state machine.
#[derive(Debug)]
pub struct Engine {
    config: Config,
}

impl Engine {
    /// Validate the configuration and construct the engine.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`]. This is the "fails to start" of SPEC §5.2 and §9.3: a venue
    /// whose withdrawal timelock does not cover the maximum quote lifetime plus mirror lag
    /// is one where a maker can withdraw out from under a live quote, and it must not run.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self { config })
    }

    /// The venue policy this engine was started with.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }
}
