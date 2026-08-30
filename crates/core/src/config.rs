//! Injectable configuration, and the two startup assertions (SPEC §5.2, §9.3).
//!
//! Market structure is configuration, not constants in the source: SPEC §14 turns on
//! `MAX_QUOTE_TTL` being a business input, and a build that hard-codes it cannot answer
//! the seconds-versus-days question with a value. Storage bounds are the opposite —
//! [`MAX_LEGS`] and [`MAX_QUOTES_PER_LEG`] size fixed arrays and cannot take a runtime
//! value, so the constant bounds storage and the config bounds policy, with the config
//! checked against the constant at startup (SPEC §3).

use crate::types::Dur;

/// Compile-time storage bound on legs per request.
///
/// Leg arrays are fixed-size (`[(LegId, Price); MAX_LEGS]` in `AcceptRequest`, SPEC
/// §7.1.1), so this cannot be a runtime value. It is also what makes the accept commit
/// phase's event count statically bounded, which is what `EventBufferFull` is checked
/// against in the CHECK phase (SPEC §7.2, CLAUDE 9).
pub const MAX_LEGS: usize = 8;

/// Compile-time storage bound on live quotes per leg.
///
/// Together with [`MAX_LEGS`] it bounds the accept commit phase at
/// `MAX_LEGS × MAX_QUOTES_PER_LEG` emitted events (SPEC §4.3, §6).
pub const MAX_QUOTES_PER_LEG: usize = 16;

/// Venue policy: every value the design leaves open, in one injectable structure.
///
/// Every bound here is a [`Dur`] — a length of time, never an instant. Both startup
/// assertions below are therefore `Dur`-to-`Dur` comparisons, which is what they were
/// always doing informally (SPEC §4.0, CLAUDE 12b).
///
/// Constructed once at startup and validated by [`Config::validate`]. The lag terms
/// default to zero because every term after the first in the §9.3 inequality *is* zero in
/// v1 — custody is in-process, confirmation depth is zero, inclusion is immediate. They
/// are settable so a violating configuration is constructible in a test: with every term
/// zero the inequality cannot be violated and a test of it would prove nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Legs a request may carry. Checked against [`MAX_LEGS`].
    pub max_legs: u8,
    /// Live quotes a leg may carry. Checked against [`MAX_QUOTES_PER_LEG`].
    pub max_quotes_per_leg: u8,
    /// Longest lifetime a maker may give a quote (SPEC §6). A market-structure input
    /// (SPEC §14), coupled to `withdrawal_delay` by the §9.3 inequality and by nothing else.
    pub max_quote_ttl: Dur,
    /// Longest `deadline − now` a request may ask for; beyond it, `DeadlineTooFar`.
    pub max_request_ttl: Dur,
    /// A contract may not be traded within this distance of its `event_date`;
    /// inside it, `ContractTooNear` (SPEC §5.2).
    pub min_horizon: Dur,
    /// Longest a request may sit in `Settling` before the timeout policy of SPEC §8.3
    /// escalates. Escalation is continued polling plus an alert, never an abort.
    pub max_settling_time: Dur,
    /// How long after `event_date` a silent oracle must stay silent before the stall exit
    /// admits `Void` (SPEC §10.2). Long relative to any plausible honest delay.
    pub stall_grace: Dur,
    /// Delay between `RequestWithdrawal` and execution (SPEC §9.3). The first term of the
    /// inequality below and the reason soft reservation is safe at all.
    pub withdrawal_delay: Dur,
    /// Accounts the balance mirror is preallocated for. Accounts are never freed, so
    /// this is a hard ceiling on how many the venue can ever address (SPEC §3).
    pub max_accounts: u32,
    /// Reservation-slab capacity. Exhaustion is `SlabExhausted`, never a grow (SPEC §4.3).
    ///
    /// Every live claim locks real capital, so in practice an account cannot hold more open
    /// quotes than its balance supports — the slab bounds the pathological case, not the
    /// normal one.
    pub max_reservations: u32,
    /// Request-slab capacity.
    pub max_requests: u32,
    /// Quote-slab capacity.
    pub max_quotes: u32,
    /// Escrows custody preallocates for. One per filled leg, and they are never freed.
    pub max_escrows: u32,
    /// Contract indices the table is preallocated for. Contracts are never freed, so this
    /// is a hard ceiling on how many distinct descriptions the venue can ever trade (§5.3).
    pub max_contracts: u32,
    /// Confirmation depth the balance mirror waits for. Zero in v1 (SPEC §2.3).
    ///
    /// A count, not a duration: it becomes one only when multiplied by `block_time`.
    pub confirmations: u32,
    /// Block time, multiplied by `confirmations` to give the mirror's confirmation lag.
    pub block_time: Dur,
    /// Worst-case indexer lag on top of confirmation depth. Zero in v1.
    pub max_indexer_lag: Dur,
    /// Worst-case submit-to-final settlement inclusion time. Zero in v1.
    pub max_settlement_inclusion_time: Dur,
}

/// A configuration that must not be allowed to start.
///
/// One variant per cause (CLAUDE 24). A single `InvalidConfig` would tell an operator that
/// something is wrong with eleven numbers and nothing about which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `max_legs` exceeds the compile-time storage bound [`MAX_LEGS`].
    MaxLegsExceedsStorage,
    /// `max_quotes_per_leg` exceeds the compile-time storage bound [`MAX_QUOTES_PER_LEG`].
    MaxQuotesPerLegExceedsStorage,
    /// A preallocated capacity is `u32::MAX`, which is reserved as the nil chain link.
    CapacityTooLarge,
    /// `max_request_ttl + max_settling_time` overflows.
    HorizonTermOverflow,
    /// `MIN_HORIZON > MAX_REQUEST_TTL + MAX_SETTLING_TIME` does not hold (SPEC §5.2).
    HorizonTooShort,
    /// One of the four §9.3 terms overflows while summing them.
    TimelockTermOverflow,
    /// The four-term withdrawal timelock inequality of SPEC §9.3 does not hold.
    WithdrawalDelayTooShort,
}

impl Config {
    /// The two startup assertions, in full.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`]. A configuration that fails this must not start: both
    /// inequalities are load-bearing, and both fail invisibly if merely violated.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if usize::from(self.max_legs) > MAX_LEGS {
            return Err(ConfigError::MaxLegsExceedsStorage);
        }
        if usize::from(self.max_quotes_per_leg) > MAX_QUOTES_PER_LEG {
            return Err(ConfigError::MaxQuotesPerLegExceedsStorage);
        }

        // `u32::MAX` is the nil link threading the intrusive chains (SPEC §4.3), so no slab
        // may be large enough for a real index to collide with it.
        let capacities = [
            self.max_accounts,
            self.max_reservations,
            self.max_requests,
            self.max_quotes,
            self.max_contracts,
            self.max_escrows,
        ];
        if capacities.contains(&u32::MAX) {
            return Err(ConfigError::CapacityTooLarge);
        }

        // SPEC §5.2. A request opened at the last legal instant, accepted at its deadline
        // and settling for the maximum time still forms escrow strictly before
        // `event_date`. Without it, escrow can be created on a contract whose stall grace
        // has already elapsed — a trade that is immediately `Void`-resolvable, which is a
        // free capital round-trip against makers.
        let latest_escrow_formation = self
            .max_request_ttl
            .checked_add(self.max_settling_time)
            .ok_or(ConfigError::HorizonTermOverflow)?;
        if self.min_horizon <= latest_escrow_formation {
            return Err(ConfigError::HorizonTooShort);
        }

        // SPEC §9.3, with every term named. Every term after the first is zero in v1, so
        // this inequality cannot fail in this build — it is written out anyway because the
        // shortfall is invisible exactly where it is cheapest to fix. On a real chain,
        // omitting the lag terms lets a maker withdraw out from under a quote that was
        // live when accepted, which is last look reintroduced through custody.
        let mirror_confirmation_lag = self
            .block_time
            .checked_mul(self.confirmations)
            .ok_or(ConfigError::TimelockTermOverflow)?;
        let must_exceed = self
            .max_quote_ttl
            .checked_add(mirror_confirmation_lag)
            .and_then(|sum| sum.checked_add(self.max_indexer_lag))
            .and_then(|sum| sum.checked_add(self.max_settlement_inclusion_time))
            .ok_or(ConfigError::TimelockTermOverflow)?;
        if self.withdrawal_delay <= must_exceed {
            return Err(ConfigError::WithdrawalDelayTooShort);
        }

        Ok(())
    }
}

impl Default for Config {
    /// The v1 venue: seconds-scale quotes, minutes-scale requests, and every lag term zero
    /// because custody is in-process.
    ///
    /// These are policy, not derived quantities — SPEC §14 is explicit that quote lifetime
    /// is a market-structure decision and that no data structure constrains it. They are
    /// chosen to satisfy both startup assertions with headroom, so a test that violates
    /// one has to do so deliberately.
    fn default() -> Self {
        Self {
            max_legs: 4,
            max_quotes_per_leg: 8,
            // 30s: a maker round trip plus room to reprice.
            max_quote_ttl: Dur(30_000),
            // 5min: long enough to collect quotes, short enough that the free option a
            // firm quote represents stays cheap (SPEC §11).
            max_request_ttl: Dur(300_000),
            // 1h, comfortably above max_request_ttl + max_settling_time = 6min.
            min_horizon: Dur(3_600_000),
            max_settling_time: Dur(60_000),
            // 24h of oracle silence before the stall exit admits Void.
            stall_grace: Dur(86_400_000),
            // 10min, comfortably above max_quote_ttl + 0 + 0 + 0 = 30s.
            withdrawal_delay: Dur(600_000),
            confirmations: 0,
            block_time: Dur::ZERO,
            max_indexer_lag: Dur::ZERO,
            max_settlement_inclusion_time: Dur::ZERO,
            // Preallocated storage. A request and a quote each back at most one claim, so a
            // reservation slab of `max_requests + max_quotes` could never be exhausted;
            // these are sized below that deliberately, because `SlabExhausted` must stay a
            // reachable rejection rather than a theoretical one.
            max_accounts: 256,
            max_reservations: 4_096,
            max_requests: 1_024,
            max_quotes: 4_096,
            max_contracts: 1_024,
            max_escrows: 4_096,
        }
    }
}
