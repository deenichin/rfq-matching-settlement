//! The chain's append-only log (SPEC §12).
//!
//! What a real chain gives an observer: entries at a block height, identified by
//! `(tx_hash, log_index)`, and a head that moves forwards. Not modelled: block production,
//! gas, mempool ordering, signature verification, EIP-712 encoding, ERC-20 semantics.
//!
//! The log is the **only** path from custody back to the engine. Nothing calls across; a
//! fact is written here, and the indexer turns it into a command.

use rfq_core::account::AccountIdx;
use rfq_core::contract::{ContractIdx, OracleStatus, Outcome};
use rfq_core::escrow::EscrowId;
use rfq_core::request::Nonce;
use rfq_core::settlement::TxStatus;
use rfq_core::types::Amount;

/// A transaction identity.
///
/// Opaque bytes, not a digest. A real chain's hash is one, but nothing here depends on that:
/// the indexer uses it for equality only, and hashing inside this repository would be a
/// dependency bought for no property (§5.3's argument, one layer down).
pub type TxHash = [u8; 32];

/// What happened, in terms the engine can be told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainPayload {
    /// An account's spendable balance moved.
    ///
    /// Carries **availability**, not balance: it is the number admission is entitled to
    /// lend against (§9.1), and the mirror is what admission reads.
    BalanceChanged {
        /// Whose.
        account: AccountIdx,
        /// Balance minus pending withdrawals.
        available: Amount,
    },
    /// The oracle said something about a contract.
    OracleStatusReported {
        /// Which contract.
        contract: ContractIdx,
        /// What it said.
        status: OracleStatus,
    },
    /// A settlement transaction reached a terminal answer.
    ///
    /// The chain announcing what became of a nonce, which is the only thing that can move a
    /// request out of `Settling`. "Still pending" is not an event and never appears here: a
    /// poller that asks anyway gets `Unknown` or `Pending`, and neither moves anything.
    SettlementResolved {
        /// Which nonce. `(ReqIdx, req_generation)`, so it names the request and the
        /// generation without anyone constructing a handle (§8.1).
        nonce: Nonce,
        /// Its fate. Terminal, always — monotonic and immutable from here.
        status: TxStatus,
    },
    /// Somebody sent a transaction asking for an escrow to be paid out.
    ///
    /// **Inbound, not a confirmation.** Custody does not log its own payouts: an entry that
    /// became a `SettleEscrow` command which produced another payout which logged another
    /// entry would be a loop, and the engine has no use for the confirmation anyway — it
    /// holds an `EscrowId` and no escrow state.
    ///
    /// The indexer turns this into the engine's own `SettleEscrow`, which re-derives
    /// admissibility from the contract state the engine holds. **Safety does not depend on
    /// delivery order**: arriving before the resolution it needs simply fails the
    /// admissibility check, rather than paying out on an outcome the engine has not seen.
    EscrowSettled {
        /// Which escrow.
        escrow: EscrowId,
        /// The contract it rests on.
        contract: ContractIdx,
        /// The outcome it was settled under, for the record. The engine re-derives its own.
        outcome: Outcome,
    },
}

/// One log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainEvent {
    /// The block it landed in. Confirmation depth is measured against this.
    pub block: u64,
    /// Which transaction. With `log_index`, the dedup key.
    pub tx_hash: TxHash,
    /// Position within that transaction.
    pub log_index: u32,
    /// What happened.
    pub payload: ChainPayload,
}

/// An append-only log with a moving head.
#[derive(Debug, Default)]
pub struct ChainLog {
    entries: Vec<ChainEvent>,
    head: u64,
    /// Transactions minted so far, so each gets a distinct identity.
    next_tx: u64,
}

impl ChainLog {
    /// An empty log at block zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current block height.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// Mine a block. Nothing else moves the head — there is no timer and no producer.
    pub const fn advance_block(&mut self) {
        self.head = self.head.saturating_add(1);
    }

    /// Every entry, oldest first.
    #[must_use]
    pub fn entries(&self) -> &[ChainEvent] {
        &self.entries
    }

    /// Append one entry at the current head, in a transaction of its own.
    pub fn append(&mut self, payload: ChainPayload) -> ChainEvent {
        let mut tx_hash = [0_u8; 32];
        tx_hash[..8].copy_from_slice(&self.next_tx.to_be_bytes());
        self.next_tx = self.next_tx.saturating_add(1);
        let event = ChainEvent { block: self.head, tx_hash, log_index: 0, payload };
        self.entries.push(event);
        event
    }

    /// Append several entries as one transaction, so they share a hash and differ by index.
    pub fn append_transaction(&mut self, payloads: &[ChainPayload]) {
        let mut tx_hash = [0_u8; 32];
        tx_hash[..8].copy_from_slice(&self.next_tx.to_be_bytes());
        self.next_tx = self.next_tx.saturating_add(1);
        for (index, payload) in payloads.iter().enumerate() {
            let log_index = u32::try_from(index).unwrap_or(u32::MAX);
            self.entries.push(ChainEvent {
                block: self.head,
                tx_hash,
                log_index,
                payload: *payload,
            });
        }
    }

    /// Discard every entry at or above `block`, and move the head back to just below it.
    ///
    /// A reorg. Confirmation depth **avoids** these rather than recovering from them: an
    /// entry the indexer has not delivered yet can vanish without the engine ever having
    /// known about it. Deeper-reorg recovery — roll back and reapply from the command log —
    /// is designed and not built (§12), and a reorg deeper than the configured depth would
    /// need it.
    pub fn reorg(&mut self, block: u64) {
        self.entries.retain(|entry| entry.block < block);
        self.head = block.saturating_sub(1);
    }
}
