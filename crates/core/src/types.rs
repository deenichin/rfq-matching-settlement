//! The money and time newtypes.
//!
//! Every one of them is a distinct type rather than a `u64` alias, so a size cannot be
//! passed where a price is expected and a timestamp cannot be added to an amount. None of
//! them implement `Add`: SPEC §2.5 and CLAUDE 14 require every arithmetic operation to be
//! checked, and an infix operator that can wrap or panic is exactly what that forbids.

/// Payout per contract to the winning side, in minor units (SPEC §2.1).
///
/// `UNIT` is a [`Price`] because that is what it is dimensionally: the price of a contract
/// that has already won. It appears in `maker_contribution = size × (UNIT − price)` and in
/// `escrowed_notional = size × UNIT`, both of which take a price on the right.
pub const UNIT: Price = Price(1_000_000);

/// An amount of the settlement asset, in minor units.
///
/// 6dp, USDC-shaped. There is exactly one implicit asset (SPEC §16); no asset id appears
/// in any structure. Never a float, here or anywhere else in the repository (CLAUDE 13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(pub u64);

impl Amount {
    /// No money at all.
    pub const ZERO: Self = Self(0);

    /// Sum, or `None` on overflow. Overflow is a rejection, never a wrap (CLAUDE 14).
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// Difference, or `None` if it would go negative.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(diff) => Some(Self(diff)),
            None => None,
        }
    }
}

/// A price in minor units per contract, always the price of the side the requester is
/// buying (SPEC §2.1), so lowest-is-best holds for `Yes` and `No` legs alike.
///
/// Admissible range is `[0, UNIT]`, checked at admission in S2 rather than by the type:
/// the bound is market structure, and a type that enforces it would have to be fallible
/// to construct in the commit phase, where nothing fallible may appear (CLAUDE 18).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Price(pub u32);

/// A number of contracts.
///
/// Contribution is `size × price` with the product taken in `u128` and checked on
/// narrowing (SPEC §2.5), which is why size and price are separately typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size(pub u64);

/// A point in time, in **milliseconds** since an arbitrary epoch (SPEC §4.0).
///
/// Quote lifetimes are seconds and escrow lifetimes are months; a month is ~2.6e9 ms, so
/// `u64` is not a range concern and the resolution is finer than any decision the system
/// makes.
///
/// `Ts` also carries durations — `MAX_QUOTE_TTL`, `STALL_GRACE` and the §9.3 timelock
/// terms are all `Ts`. SPEC §4.0 defines milliseconds as the unit and does not name a
/// second type, so none is invented here; a distinct duration newtype would be a design
/// decision, and the design decisions in this repository are made in `SPEC.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ts(pub u64);

impl Ts {
    /// The epoch itself.
    pub const ZERO: Self = Self(0);

    /// `self + duration`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, duration: Self) -> Option<Self> {
        match self.0.checked_add(duration.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self + duration`, clamped at the end of time.
    #[must_use]
    pub const fn saturating_add(self, duration: Self) -> Self {
        Self(self.0.saturating_add(duration.0))
    }

    /// The duration from `earlier` to `self`, or zero if `self` is not later.
    ///
    /// Saturating rather than checked because the caller of a *difference* between two
    /// instants always wants a duration, and a negative one is not representable; every
    /// ordering question is asked with `<` against the sampled `now` instead (SPEC §4.2).
    #[must_use]
    pub const fn saturating_sub(self, earlier: Self) -> Self {
        Self(self.0.saturating_sub(earlier.0))
    }

    /// `self × count`, or `None` on overflow. Used for `CONFIRMATIONS × block_time`.
    #[must_use]
    pub fn checked_mul(self, count: u32) -> Option<Self> {
        self.0.checked_mul(u64::from(count)).map(Self)
    }
}
