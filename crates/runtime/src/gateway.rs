//! The gateway (SPEC §3, §5.3).
//!
//! The boundary where external identifiers become dense `u32` indices, and the **only**
//! place a contract description exists. The core is addressed by index, never hashes, never
//! compares bytes, and never sees a description (CLAUDE 11). Allocation is permitted here
//! and forbidden past it.
//!
//! **Contract identity is byte equality** over the full description, the event date and the
//! resolution source. Two requests trade the same contract iff all three match exactly.
//! There is no fuzzy matching and no canonicalisation — near-identical wording produces
//! different contracts, which is correct: the wording *is* the product.
//!
//! No hash is used, anywhere. A non-cryptographic hash would be strictly worse than byte
//! equality here, because a collision would let a trade formed on one contract resolve under
//! another's outcome, and contract identity is an adversarial surface (§11). A cryptographic
//! hash would buy nothing over byte equality while adding a dependency, and nothing in v1
//! puts a contract id on a wire or a chain, so there is nothing to compress. `BTreeMap`
//! rather than a hash map: ordered, deterministic iteration, and no hashing (CLAUDE 3).
//!
//! The gateway lives in the runtime because it is an adapter that *produces commands*, like
//! the indexer (§13). It holds no engine state and cannot write any.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rfq_core::account::AccountIdx;
use rfq_core::command::{Command, LegSpec};
use rfq_core::contract::ContractIdx;
use rfq_core::types::{Price, Side, Size, Ts};

/// A contract as a participant describes it. **Never crosses into the core.**
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractRef {
    /// The full wording. This *is* the identity.
    pub description: Vec<u8>,
    /// When the event settles.
    pub event_date: Ts,
    /// Where the outcome comes from — part of the identity, because the same wording
    /// resolved by two different sources is two different products.
    pub resolution_source: Vec<u8>,
}

/// The append-only description store.
///
/// Shared with the publisher, which resolves a `ContractIdx` back to the verbatim wording
/// when it fans `RequestOpened` out to makers — the description is broadcast, so a maker
/// quoting a contract is asserting they have read and priced that exact byte sequence
/// (§5.3). It is behind a lock for the same reason the event ring is: it is outside the
/// engine, append-only, written by the gateway and read by the publisher, and CLAUDE 7's
/// prohibition is on locks around *engine state*.
#[derive(Debug, Default)]
pub struct ContractRegistry {
    by_index: Vec<ContractRef>,
}

impl ContractRegistry {
    /// The description behind an index, for republishing verbatim.
    #[must_use]
    pub fn describe(&self, contract: ContractIdx) -> Option<&ContractRef> {
        self.by_index.get(contract.0 as usize)
    }

    /// How many contracts have been assigned an index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_index.len()
    }

    /// Whether nothing has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_index.is_empty()
    }
}

/// Why the gateway could not admit something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayError {
    /// No index left in the configured contract table.
    ContractTableFull,
    /// No index left in the configured account table.
    AccountTableFull,
    /// More legs than `MAX_LEGS` allows.
    TooManyLegs,
    /// The registry's lock is poisoned — a thread panicked while holding it.
    RegistryUnavailable,
}

/// External ids in, dense indices and commands out.
#[derive(Debug)]
pub struct Gateway {
    /// Byte equality over description + event date + resolution source. `BTreeMap`'s `Ord`
    /// on `Vec<u8>` *is* lexicographic byte comparison, so identity needs no code of its own.
    contracts: BTreeMap<ContractRef, ContractIdx>,
    registry: Arc<Mutex<ContractRegistry>>,
    max_contracts: u32,
    accounts: BTreeMap<Vec<u8>, AccountIdx>,
    max_accounts: u32,
}

impl Gateway {
    /// A gateway that may assign up to the engine's configured capacities.
    #[must_use]
    pub fn new(max_accounts: u32, max_contracts: u32) -> Self {
        Self {
            contracts: BTreeMap::new(),
            registry: Arc::new(Mutex::new(ContractRegistry::default())),
            max_contracts,
            accounts: BTreeMap::new(),
            max_accounts,
        }
    }

    /// A read handle on the description store, for the publisher.
    #[must_use]
    pub fn registry(&self) -> Arc<Mutex<ContractRegistry>> {
        Arc::clone(&self.registry)
    }

    /// The dense index for an account key, assigning one on first sight.
    ///
    /// # Errors
    ///
    /// [`GatewayError::AccountTableFull`].
    pub fn account(&mut self, key: &[u8]) -> Result<AccountIdx, GatewayError> {
        if let Some(index) = self.accounts.get(key) {
            return Ok(*index);
        }
        let next = u32::try_from(self.accounts.len()).unwrap_or(u32::MAX);
        if next >= self.max_accounts {
            return Err(GatewayError::AccountTableFull);
        }
        let index = AccountIdx(next);
        self.accounts.insert(key.to_vec(), index);
        Ok(index)
    }

    /// The dense index for a contract, assigning one on first sight, plus the command that
    /// tells the core about it.
    ///
    /// Returns `None` for the command when the contract was already known: the gateway
    /// resolves an identical description to the same index, so re-registering would be a
    /// no-op the core does not need to hear about.
    ///
    /// # Errors
    ///
    /// [`GatewayError::ContractTableFull`], [`GatewayError::RegistryUnavailable`].
    pub fn contract(
        &mut self,
        reference: &ContractRef,
    ) -> Result<(ContractIdx, Option<Command>), GatewayError> {
        if let Some(index) = self.contracts.get(reference) {
            return Ok((*index, None));
        }
        let next = u32::try_from(self.contracts.len()).unwrap_or(u32::MAX);
        if next >= self.max_contracts {
            return Err(GatewayError::ContractTableFull);
        }
        let index = ContractIdx(next);
        self.contracts.insert(reference.clone(), index);
        {
            let mut registry =
                self.registry.lock().map_err(|_| GatewayError::RegistryUnavailable)?;
            registry.by_index.push(reference.clone());
        }
        Ok((
            index,
            Some(Command::RegisterContract { contract: index, event_date: reference.event_date }),
        ))
    }

    /// Build a `SubmitRequest` from external terms.
    ///
    /// Every description is resolved to an index here, so no byte of wording reaches the
    /// core. The returned commands must be submitted in order: the `RegisterContract`s
    /// first, then the request that references them.
    ///
    /// # Errors
    ///
    /// [`GatewayError`].
    pub fn submit_request(
        &mut self,
        requester: &[u8],
        deadline: Ts,
        legs: &[(ContractRef, Side, Size, Price)],
    ) -> Result<(Vec<Command>, Command), GatewayError> {
        if legs.len() > rfq_core::config::MAX_LEGS {
            return Err(GatewayError::TooManyLegs);
        }
        let requester = self.account(requester)?;
        let mut registrations = Vec::new();
        let mut specs = [LegSpec::default(); rfq_core::config::MAX_LEGS];
        for (index, (reference, side, size, limit)) in legs.iter().enumerate() {
            let (contract, registration) = self.contract(reference)?;
            if let Some(registration) = registration {
                registrations.push(registration);
            }
            if let Some(slot) = specs.get_mut(index) {
                *slot = LegSpec { contract, side: *side, size: *size, limit: *limit };
            }
        }
        let n_legs = u8::try_from(legs.len()).unwrap_or(u8::MAX);
        Ok((registrations, Command::SubmitRequest { requester, deadline, legs: specs, n_legs }))
    }
}
