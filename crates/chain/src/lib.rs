//! Custody, and the chain-shaped machinery around it.
//!
//! The other half of the SPEC §13.1 seam. Custody is authoritative for balances, escrows,
//! nonces and the withdrawal timelock. It holds no reference to the engine, knows nothing
//! of requests, quotes, legs, reservations or claims, and has never heard of a "committed"
//! bucket. It shares no memory with the engine: the only way in is a bundle handed to it
//! by the settlement adapter, and the only way out is an entry in the chain log.
//!
//! It depends on `rfq-core` for shared value types — [`Amount`], [`Ts`], [`Clock`] — and
//! for nothing else. That direction is the permitted one; the reverse is refused at
//! compile time by `rfq-core`'s build script.
//!
//! Stage S0 establishes custody as a system with **its own clock**. Balances, escrows,
//! nonces and the timelock land in S3; the oracle in S5; the indexer in S6.
//!
//! [`Amount`]: rfq_core::types::Amount
//! [`Ts`]: rfq_core::types::Ts
//! [`Clock`]: rfq_core::clock::Clock

pub mod bundle;
pub mod custody;
pub mod escrow;
pub mod indexer;
pub mod log;
pub mod oracle;

pub use bundle::{Bundle, BundleLeg};
pub use custody::{
    Balance, Custody, CustodyError, CustodyLedger, IncludedTx, SettleEntryHook, SettleError,
    SettleReceipt, SubmitAck,
};
pub use escrow::{Escrow, EscrowState};
pub use indexer::Indexer;
pub use log::{ChainEvent, ChainLog, ChainPayload, TxHash};
pub use oracle::{Oracle, OracleError};
