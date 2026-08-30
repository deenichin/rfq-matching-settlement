//! The engine: the single-writer state machine.
//!
//! Holds requests, quotes, reservations, claims and the read-only balance mirror. It holds
//! no reference to custody and cannot read a balance from it; it sees `EscrowId` and its own
//! mirror, nothing else (SPEC §13.1).
//!
//! Note what the engine does **not** own: a clock. `apply(cmd, now)` samples time exactly
//! once at the call site (CLAUDE 2), so the runtime samples it and passes the value in.
//! Re-reading the clock mid-command is therefore not forbidden by convention — it is
//! unreachable, because the engine has nothing to re-read. The same property is what makes
//! the command log replayable: the log records the instant that was sampled, and replay
//! feeds it back.
//!
//! `apply` performs **no I/O** — not logging, not metrics, not `println!` (CLAUDE 8). It
//! writes events into a caller-provided buffer and returns.

use crate::account::AccountIdx;
use crate::command::Command;
use crate::config::{Config, ConfigError};
use crate::event::{EventBuffer, EventBufferFull};
use crate::ledger::{Ledger, LedgerError};
use crate::reservation::Reservation;
use crate::types::Ts;

/// A rejected command. Each variant names the specific cause (CLAUDE 24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The ledger refused. The inner variant names which rule was violated.
    Ledger(LedgerError),
    /// The command would emit more events than the caller's buffer has headroom for.
    ///
    /// Checked in the CHECK phase, so the commit phase never has to (CLAUDE 9). Reachable
    /// only by construction, never by message volume: the accept commit phase's event count
    /// is statically bounded by `MAX_LEGS × MAX_QUOTES_PER_LEG` (§4.3).
    EventBufferFull,
    /// The command's timestamp arithmetic overflowed — a `ttl` past the end of time.
    ExpiryOverflow,
}

impl From<LedgerError> for EngineError {
    fn from(error: LedgerError) -> Self {
        Self::Ledger(error)
    }
}

impl From<EventBufferFull> for EngineError {
    fn from(_: EventBufferFull) -> Self {
        Self::EventBufferFull
    }
}

/// The single-writer state machine.
///
/// `PartialEq` is structural and reaches every slab generation and free-list link, so
/// "replay reproduces the final state byte-for-byte" (SPEC §13) is checkable by comparison
/// rather than by a digest that could agree by coincidence.
#[derive(Debug, PartialEq, Eq)]
pub struct Engine {
    config: Config,
    ledger: Ledger,
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
        let ledger = Ledger::new(&config);
        Ok(Self { config, ledger })
    }

    /// The venue policy this engine was started with.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// The claim ledger, read-only.
    #[must_use]
    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Apply one command at the instant `now`.
    ///
    /// `now` is sampled **once**, by the caller, and every predicate evaluated during this
    /// command uses that one value (§4.1, CLAUDE 2).
    ///
    /// Phases, in order:
    ///
    /// 1. **NORMALISE** — `release_expired` for every account the command touches. Mutates
    ///    state, depends only on `(accounts, now)`, and never on the command's content or on
    ///    whether the command will be accepted. Emits **no events**: its count is bounded
    ///    only by how many claims expired, so emitting would put an unbounded write into the
    ///    buffer before the CHECK phase could verify headroom (§4.3, CLAUDE 9).
    /// 2. **PLAN / CHECK / COMMIT** — the ledger operation, which runs its own three phases
    ///    internally and leaves nothing half-applied on a rejection.
    ///
    /// This is why the no-mutation-on-rejection guarantee is stated relative to the
    /// **post-normalisation** state (§15.4): normalisation is time catching up, not the
    /// command acting.
    ///
    /// # Errors
    ///
    /// [`EngineError`]. A rejection leaves the post-normalisation state untouched.
    pub fn apply(
        &mut self,
        command: Command,
        now: Ts,
        events: &mut EventBuffer,
    ) -> Result<(), EngineError> {
        debug_assert!(events.is_empty(), "apply is handed a drained buffer");

        // ── NORMALISE ──
        if let Some(account) = self.touched_account(command) {
            // An unknown account has nothing to reclaim; the command's own validation is
            // what reports it, and normalisation must not depend on the command's fate.
            let _ = self.ledger.release_expired(account, now);
        }
        debug_assert!(events.is_empty(), "normalisation emits no events (SPEC §4.3)");

        // ── PLAN / CHECK / COMMIT ──
        match command {
            Command::CreditAccount { account, free } => {
                self.ledger.apply_mirror_update(account, free)?;
            }
            Command::OpenQuote { maker, amount, ttl } => {
                let expires_at = now.checked_add(ttl).ok_or(EngineError::ExpiryOverflow)?;
                self.ledger.open_quote_reserving(maker, amount, expires_at)?;
            }
            Command::OpenRequest { requester, amount, ttl } => {
                let expires_at = now.checked_add(ttl).ok_or(EngineError::ExpiryOverflow)?;
                self.ledger.open_request_reserving(requester, amount, expires_at)?;
            }
            Command::CloseQuote { quote } => {
                let claim = self.ledger.quote(quote).ok_or(LedgerError::StaleOwner)?.claim();
                if let Some(claim) = claim {
                    self.ledger.release(claim)?;
                }
                self.ledger.close_quote(quote)?;
            }
            Command::CommitQuote { quote, request } => {
                let claim = self
                    .ledger
                    .quote(quote)
                    .ok_or(LedgerError::StaleOwner)?
                    .claim()
                    .ok_or(LedgerError::StaleReservation)?;
                self.ledger.commit(claim, request)?;
            }
        }

        Ok(())
    }

    /// Which account this command touches, and therefore which account normalisation
    /// reclaims from.
    ///
    /// Derived from the command's *addressing*, not from whether it will succeed. A handle
    /// that does not resolve names no account, and the command itself then reports why.
    fn touched_account(&self, command: Command) -> Option<AccountIdx> {
        match command {
            Command::CreditAccount { account, .. } => Some(account),
            Command::OpenQuote { maker, .. } => Some(maker),
            Command::OpenRequest { requester, .. } => Some(requester),
            Command::CloseQuote { quote } | Command::CommitQuote { quote, .. } => {
                let claim = self.ledger.quote(quote)?.claim()?;
                self.ledger.reservation(claim).map(Reservation::account)
            }
        }
    }
}
