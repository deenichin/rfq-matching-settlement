//! The settlement bundle: what custody is asked to include (SPEC §9.1).
//!
//! Custody's own type, over shared value types only. It is not the engine's `SubmitIntent`
//! event — the settlement adapter converts one into the other, and that conversion is where
//! the seam is visible. Custody has never heard of a request, a quote or a claim; a bundle
//! is a transaction with legs, which is the transaction's own structure.
//!
//! **One nonce per bundle, not per leg.** The transaction is atomic and legs can never
//! settle separately, so a per-leg nonce would carry no information the bundle nonce does
//! not.
//!
//! The bundle is complete: every value custody validates against travels with it, including
//! each leg's quote expiry, which custody rechecks against **its own clock**. Custody cannot
//! reach back into the engine for anything it is missing.

use rfq_core::account::AccountIdx;
use rfq_core::config::MAX_LEGS;
use rfq_core::contract::ContractIdx;
use rfq_core::request::Nonce;
use rfq_core::types::{Amount, Price, Side, Size, Ts, UNIT};

/// One leg of a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BundleLeg {
    /// The contract the escrow forms on.
    pub contract: ContractIdx,
    /// The side the requester bought.
    pub side: Side,
    /// How many contracts.
    pub size: Size,
    /// The maker who takes the other side.
    pub maker: AccountIdx,
    /// The fill price. Both contributions derive from it exactly.
    pub fill_price: Price,
    /// When the maker's quote stops binding. Rechecked against custody's clock (§9.1).
    pub quote_expiry: Ts,
}

impl BundleLeg {
    /// The requester's contribution, `size × price`.
    #[must_use]
    pub fn requester_contribution(&self) -> Option<Amount> {
        self.size.requester_contribution(self.fill_price)
    }

    /// The maker's contribution, `size × (UNIT − price)`.
    #[must_use]
    pub fn maker_contribution(&self) -> Option<Amount> {
        self.size.maker_contribution(self.fill_price)
    }

    /// The notional, `size × UNIT`.
    #[must_use]
    pub fn notional(&self) -> Option<Amount> {
        self.size.checked_mul(UNIT)
    }
}

/// An atomic multi-leg settlement transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bundle {
    /// Unique and deterministic without hashing (§8.1). Consumed inside the transaction.
    pub nonce: Nonce,
    /// Who is debited `Σ size × fill_price`.
    pub requester: AccountIdx,
    /// The legs.
    pub legs: [BundleLeg; MAX_LEGS],
    /// How many of them are real.
    pub n_legs: u8,
}

impl Bundle {
    /// The legs that are real.
    #[must_use]
    pub fn legs(&self) -> &[BundleLeg] {
        self.legs.get(..usize::from(self.n_legs)).unwrap_or(&[])
    }
}
