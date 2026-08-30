//! The mocked optimistic oracle (SPEC §10.1).
//!
//! Propose, a challenge window, contestation, finality — and an escalation authority that
//! can rule anything, including `Void`.
//!
//! **None of this is in the engine.** The engine models no proposal, dispute, bonding or
//! voting; its contract state is two values and its interface to this is one status type and
//! one command. Importing this lifecycle into the state machine would couple it to a system
//! the venue does not control and cannot fix. What crosses is an [`OracleStatus`], carried
//! by an adapter into the same command queue as everything else.
//!
//! Bond sizing and slashing are the integrated oracle's concern and are not modelled (§16).
//! What is modelled is the shape the engine depends on: a status that only ever moves
//! forwards, and an authority of last resort that is named rather than argued away.

use std::collections::BTreeMap;

use rfq_core::account::AccountIdx;
use rfq_core::clock::Clock;
use rfq_core::contract::{ContractIdx, OracleStatus, Outcome};
use rfq_core::types::{Dur, Ts};

/// Why the oracle refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleError {
    /// A proposal already stands, or the contract is already final.
    AlreadyProposed,
    /// Nothing has been proposed, so there is nothing to contest or finalise.
    NothingProposed,
    /// The challenge window has not elapsed. Finalising early would make the window
    /// decorative.
    WindowOpen,
    /// The proposal was contested, so it cannot finalise on its own. Only the escalation
    /// authority can end it.
    Contested,
    /// Already decided. `Final` is terminal here as well as in the engine — a mock that let
    /// its own outcome be overwritten would hand the engine the very regression the engine
    /// exists to refuse.
    AlreadyFinal,
    /// Someone other than the escalation authority tried to rule.
    NotTheEscalationAuthority,
}

/// What the oracle knows about one contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proposal {
    /// Proposed, and contestable until this instant.
    Proposed { outcome: Outcome, window_closes: Ts },
    /// Contested. It can no longer finalise on its own.
    Contested,
    /// Decided.
    Final(Outcome),
}

/// An optimistic oracle, as far as the venue needs one.
#[derive(Debug)]
pub struct Oracle<C: Clock> {
    clock: C,
    challenge_window: Dur,
    /// The design's one trusted component: a single unbonded key that can assign any
    /// outcome, including `Void`. Named plainly, because §10.4 closes three routes to a
    /// forced unwind and this is the fourth, which stays open by construction.
    escalation_authority: AccountIdx,
    contracts: BTreeMap<ContractIdx, Proposal>,
}

impl<C: Clock> Oracle<C> {
    /// An oracle with its own clock, its challenge window, and its escalation authority.
    #[must_use]
    pub fn new(clock: C, challenge_window: Dur, escalation_authority: AccountIdx) -> Self {
        Self { clock, challenge_window, escalation_authority, contracts: BTreeMap::new() }
    }

    /// The oracle's clock, for a test to advance.
    pub const fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    /// What the engine would be told about this contract right now.
    ///
    /// The **only** thing that crosses into the engine. Proposal, window and contestation
    /// all collapse to `InProgress`: working, but not final.
    #[must_use]
    pub fn status(&self, contract: ContractIdx) -> OracleStatus {
        match self.contracts.get(&contract) {
            // An absent entry *is* silence; a separate variant for it would be a second way
            // to say the same thing.
            None => OracleStatus::Silent,
            Some(Proposal::Proposed { .. } | Proposal::Contested) => OracleStatus::InProgress,
            Some(Proposal::Final(outcome)) => OracleStatus::Final(*outcome),
        }
    }

    /// Propose an outcome, opening the challenge window.
    ///
    /// # Errors
    ///
    /// [`OracleError::AlreadyProposed`], [`OracleError::AlreadyFinal`].
    pub fn propose(
        &mut self,
        contract: ContractIdx,
        outcome: Outcome,
    ) -> Result<Ts, OracleError> {
        match self.contracts.get(&contract) {
            Some(Proposal::Final(_)) => return Err(OracleError::AlreadyFinal),
            Some(Proposal::Proposed { .. } | Proposal::Contested) => {
                return Err(OracleError::AlreadyProposed);
            }
            None => {}
        }
        let window_closes = self
            .clock
            .now()
            .checked_add(self.challenge_window)
            .ok_or(OracleError::WindowOpen)?;
        self.contracts.insert(contract, Proposal::Proposed { outcome, window_closes });
        Ok(window_closes)
    }

    /// Contest a standing proposal.
    ///
    /// After this the proposal can never finalise on its own — only the escalation authority
    /// can end it. That is the *point*: a losing party who contests buys a delay, not a free
    /// unwind, because the engine's stall exit conditions on `Silent` and contestation moves
    /// the status to `InProgress` and keeps it there (§10.4).
    ///
    /// # Errors
    ///
    /// [`OracleError::NothingProposed`], [`OracleError::AlreadyFinal`].
    pub fn contest(&mut self, contract: ContractIdx) -> Result<(), OracleError> {
        match self.contracts.get(&contract) {
            Some(Proposal::Proposed { .. }) => {
                self.contracts.insert(contract, Proposal::Contested);
                Ok(())
            }
            Some(Proposal::Contested) => Ok(()),
            Some(Proposal::Final(_)) => Err(OracleError::AlreadyFinal),
            None => Err(OracleError::NothingProposed),
        }
    }

    /// Finalise an uncontested proposal whose window has closed.
    ///
    /// # Errors
    ///
    /// [`OracleError::NothingProposed`], [`OracleError::WindowOpen`],
    /// [`OracleError::Contested`], [`OracleError::AlreadyFinal`].
    pub fn finalise(&mut self, contract: ContractIdx) -> Result<Outcome, OracleError> {
        let now = self.clock.now();
        match self.contracts.get(&contract) {
            Some(Proposal::Proposed { outcome, window_closes }) => {
                if now < *window_closes {
                    return Err(OracleError::WindowOpen);
                }
                let outcome = *outcome;
                self.contracts.insert(contract, Proposal::Final(outcome));
                Ok(outcome)
            }
            Some(Proposal::Contested) => Err(OracleError::Contested),
            Some(Proposal::Final(_)) => Err(OracleError::AlreadyFinal),
            None => Err(OracleError::NothingProposed),
        }
    }

    /// The escalation authority rules, ending a contest.
    ///
    /// A single unbonded key that can assign any outcome, including `Void`. This is the
    /// design's one trusted component and the one route to a forced unwind that stays open
    /// (§10.4) — a trust assumption rather than a mechanism, stated rather than argued away.
    ///
    /// # Errors
    ///
    /// [`OracleError::NotTheEscalationAuthority`], [`OracleError::AlreadyFinal`].
    pub fn escalate(
        &mut self,
        contract: ContractIdx,
        outcome: Outcome,
        who: AccountIdx,
    ) -> Result<(), OracleError> {
        if who != self.escalation_authority {
            return Err(OracleError::NotTheEscalationAuthority);
        }
        if matches!(self.contracts.get(&contract), Some(Proposal::Final(_))) {
            return Err(OracleError::AlreadyFinal);
        }
        self.contracts.insert(contract, Proposal::Final(outcome));
        Ok(())
    }
}
