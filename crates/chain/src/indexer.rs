//! The indexer (SPEC §12).
//!
//! A separate component translating log entries into engine commands. Three properties, and
//! each exists for a failure the others do not cover:
//!
//! - **cursor** — a resumable position, so a restart does not begin from block zero.
//! - **confirmation depth** — entries are delivered only once `block + CONFIRMATIONS <= head`.
//!   This *avoids* reorgs rather than recovering from them: an entry that vanishes before it
//!   is deep enough was never delivered, so there is nothing to undo.
//! - **dedup** on `(tx_hash, log_index)` — restart replays are harmless. The cursor alone is
//!   not enough: a cursor can be lost or rewound, and then the same entries arrive again.
//!   Dedup is what makes that a no-op, and the two are tested separately because clearing
//!   only the cursor proves nothing about either.
//!
//! It holds no engine state and cannot write any. Like the gateway, it *produces commands*.

use std::collections::BTreeSet;

use rfq_core::command::Command;

use crate::log::{ChainEvent, ChainLog, ChainPayload, TxHash};

/// Cursor, confirmation depth and dedup.
#[derive(Debug)]
pub struct Indexer {
    cursor: usize,
    confirmations: u64,
    delivered: BTreeSet<(TxHash, u32)>,
}

impl Indexer {
    /// An indexer waiting `confirmations` blocks before believing anything.
    #[must_use]
    pub fn new(confirmations: u64) -> Self {
        Self { cursor: 0, confirmations, delivered: BTreeSet::new() }
    }

    /// The highest block the indexer will act on, given the log's head.
    ///
    /// Everything above it exists and is not yet believed.
    #[must_use]
    pub const fn horizon(&self, head: u64) -> Option<u64> {
        head.checked_sub(self.confirmations)
    }

    /// How far through the log the cursor has read.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Entries delivered so far, by dedup key.
    #[must_use]
    pub fn delivered_count(&self) -> usize {
        self.delivered.len()
    }

    /// Rewind the cursor to the beginning, keeping the dedup set.
    ///
    /// A restart that lost its position. Every entry is read again and every one is
    /// recognised, so nothing is delivered twice.
    pub const fn rewind(&mut self) {
        self.cursor = 0;
    }

    /// Rewind the cursor **and** forget what has been delivered.
    ///
    /// Not a restart — a restart keeps its dedup state, because that is what the state is
    /// for. This models losing both, and it is how a test shows which of the two is
    /// load-bearing: with dedup intact a replay changes nothing, and without it the same
    /// entries arrive a second time.
    pub fn forget_everything(&mut self) {
        self.cursor = 0;
        self.delivered.clear();
    }

    /// Read every confirmed, undelivered entry and translate it into a command.
    ///
    /// The cursor advances past entries that are confirmed; an unconfirmed entry stops the
    /// walk, because everything after it is at least as young.
    pub fn drain(&mut self, log: &ChainLog) -> Vec<Command> {
        let Some(horizon) = self.horizon(log.head()) else {
            // The chain is younger than the confirmation depth. Nothing is deep enough yet.
            return Vec::new();
        };

        let mut commands = Vec::new();
        let entries = log.entries();
        while let Some(entry) = entries.get(self.cursor) {
            if entry.block > horizon {
                break;
            }
            self.cursor = self.cursor.saturating_add(1);
            if !self.delivered.insert((entry.tx_hash, entry.log_index)) {
                // Seen before. A replay is a no-op, which is the whole point of dedup.
                continue;
            }
            commands.push(Self::translate(entry));
        }
        commands
    }

    /// One log entry, as an engine command.
    ///
    /// Translation, not interpretation: nothing here decides anything. `EscrowSettled`
    /// becomes a `SettleEscrow` the engine will refuse if it has not yet seen the resolution
    /// — safety does not depend on the log's delivery order.
    fn translate(entry: &ChainEvent) -> Command {
        match entry.payload {
            ChainPayload::BalanceChanged { account, available } => {
                Command::CreditAccount { account, free: available }
            }
            ChainPayload::OracleStatusReported { contract, status } => {
                Command::ReportOracleStatus { contract, status }
            }
            ChainPayload::SettlementResolved { nonce, status } => {
                Command::PollSettlement { nonce, status }
            }
            ChainPayload::EscrowSettled { escrow, contract, .. } => {
                Command::SettleEscrow { escrow, contract }
            }
        }
    }
}
