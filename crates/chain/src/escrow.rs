//! Escrows (SPEC §9.2).
//!
//! ```text
//!   Locked ──settle──> Settled      (consumed flag set in the same mutation as the credit)
//! ```
//!
//! Each escrow stores **both contributions separately**, not just the notional. The void
//! path returns each side its own contribution (§10.3), and a notional alone cannot say who
//! put in what — refunding half each would be a redistribution disguised as neutrality.
//!
//! It also stores the **requester's side** for the leg. There is no implicit buyer or
//! seller: who wins on `Yes` is a property of the leg, not a convention, and the payout
//! mapping consumes the side (§2.1, §10.3).

use rfq_core::account::AccountIdx;
use rfq_core::contract::ContractIdx;
use rfq_core::types::{Amount, Side, Size};

/// Whether an escrow still holds money.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscrowState {
    /// Holding both contributions. **Only `Locked` escrows hold money** — this is the state
    /// conservation counts (§2.2, CLAUDE 41).
    Locked,
    /// Paid out. Its notional is back in someone's free balance, so counting it in
    /// conservation would make the first payout read as newly created money.
    Settled,
}

/// One leg's worth of locked capital.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Escrow {
    contract: ContractIdx,
    /// The side the **requester** bought. What the payout mapping consumes (§10.3).
    side: Side,
    size: Size,
    requester: AccountIdx,
    maker: AccountIdx,
    requester_contribution: Amount,
    maker_contribution: Amount,
    state: EscrowState,
}

impl Escrow {
    /// A freshly locked escrow.
    #[must_use]
    pub const fn locked(
        contract: ContractIdx,
        side: Side,
        quantity: Size,
        requester: AccountIdx,
        maker: AccountIdx,
        requester_contribution: Amount,
        maker_contribution: Amount,
    ) -> Self {
        Self {
            contract,
            side,
            size: quantity,
            requester,
            maker,
            requester_contribution,
            maker_contribution,
            state: EscrowState::Locked,
        }
    }

    /// The contract this escrow resolves against.
    #[must_use]
    pub const fn contract(&self) -> ContractIdx {
        self.contract
    }

    /// The requester's side.
    #[must_use]
    pub const fn side(&self) -> Side {
        self.side
    }

    /// How many contracts.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// Who bought.
    #[must_use]
    pub const fn requester(&self) -> AccountIdx {
        self.requester
    }

    /// Who sold.
    #[must_use]
    pub const fn maker(&self) -> AccountIdx {
        self.maker
    }

    /// The requester's contribution, `size × price`.
    #[must_use]
    pub const fn requester_contribution(&self) -> Amount {
        self.requester_contribution
    }

    /// The maker's contribution, `size × (UNIT − price)`.
    #[must_use]
    pub const fn maker_contribution(&self) -> Amount {
        self.maker_contribution
    }

    /// The notional, `size × UNIT`. The two contributions sum to it exactly, by
    /// construction — there is no division anywhere in the money path (§2.1).
    #[must_use]
    pub fn notional(&self) -> Amount {
        Amount(self.requester_contribution.0.saturating_add(self.maker_contribution.0))
    }

    /// Whether this escrow still holds money.
    #[must_use]
    pub const fn state(&self) -> EscrowState {
        self.state
    }

    /// Pay out: the consumed flag is set in the same mutation as the credit (§9.2).
    pub(crate) const fn mark_settled(&mut self) {
        self.state = EscrowState::Settled;
    }

    /// Whether it is still `Locked`.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        matches!(self.state, EscrowState::Locked)
    }
}
