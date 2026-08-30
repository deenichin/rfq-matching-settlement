//! The money and time newtypes.
//!
//! Every one of them is a distinct type rather than a `u64` alias, so a size cannot be
//! passed where a price is expected and an instant cannot be added to an instant.
//!
//! **Instants and durations are separate types** (SPEC §4.0). [`Ts`] is a point on the
//! venue's timeline; [`Dur`] is a length of time. The four operations the specification
//! actually uses are the four that exist:
//!
//! | Operation | Spelled |
//! |---|---|
//! | `Ts + Dur -> Ts` | [`Ts::checked_add`], [`Ts::saturating_add`] |
//! | `Ts − Ts -> Dur` | [`Ts::saturating_sub`] |
//! | `Dur + Dur -> Dur` | [`Dur::checked_add`] |
//! | `Dur × u32 -> Dur` | [`Dur::checked_mul`] |
//!
//! Everything else — `Ts + Ts` above all — does not compile, and the doctests on each type
//! assert that it does not. The distinction is load-bearing rather than tidy: the §9.3
//! timelock inequality and the §5.2 horizon inequality are both statements about
//! *durations*, and while they were written against one type they were comparing
//! `deadline − now` with `MAX_REQUEST_TTL` on nothing but the author's attention.
//!
//! None of these types implement `Add` or `Sub`. SPEC §2.5 and CLAUDE 14 require every
//! arithmetic operation to be checked, and an infix operator that can wrap or panic is
//! exactly what that forbids; the permitted operations are therefore spelled as checked
//! methods. That is also why the absence of `Add` is what makes `Ts + Ts` a compile error
//! — there is no operator to reach for, on any pairing.

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

/// Which side of a binary contract the **requester** is buying (SPEC §2.1).
///
/// Every leg carries one. The maker takes the opposite side of that leg.
///
/// Without it a requester could only ever be long `Yes` on every leg, which reduces
/// multi-leg to a parlay and cannot express a spread — and the spread is the motivating
/// multi-leg product. It is also what makes one contribution formula cover both sides:
/// buying `No` at `q` is economically identical to selling `Yes` at `UNIT − q`, and the
/// price is always the price of the side being bought, so selection needs no side-specific
/// branch and lowest-is-best holds on both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    /// The requester is buying `Yes`; the maker takes `No`.
    ///
    /// `Default` only so that a fixed-size leg array can be initialised before its real legs
    /// are written; no admission path ever reads an unwritten slot, because `n_legs` bounds
    /// every walk.
    #[default]
    Yes,
    /// The requester is buying `No`; the maker takes `Yes`.
    No,
}

impl Side {
    /// The side the maker takes.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Yes => Self::No,
            Self::No => Self::Yes,
        }
    }
}

/// Which leg of a request. Dense, `0 .. request.n_legs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegId(pub u8);

/// A number of contracts.
///
/// Contribution is `size × price` with the product taken in `u128` and checked on
/// narrowing (SPEC §2.5), which is why size and price are separately typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Size(pub u64);

/// A **point in time**, in milliseconds since an arbitrary epoch (SPEC §4.0).
///
/// Quote expiries, request deadlines, contract event dates and the sampled `now` are all
/// `Ts`. Quote lifetimes are seconds and escrow lifetimes are months; a month is ~2.6e9 ms,
/// so `u64` is not a range concern and the resolution is finer than any decision the
/// system makes.
///
/// Adding one instant to another is meaningless, and does not compile:
///
/// ```compile_fail,E0369
/// use rfq_core::types::Ts;
/// let _ = Ts(1) + Ts(2);
/// ```
///
/// Nor by the checked spelling — [`Ts::checked_add`] advances an instant by a *duration*:
///
/// ```compile_fail,E0308
/// use rfq_core::types::Ts;
/// let _ = Ts(1).checked_add(Ts(2));
/// ```
///
/// The permitted forms do compile:
///
/// ```
/// use rfq_core::types::{Dur, Ts};
/// assert_eq!(Ts(1_000).checked_add(Dur(500)), Some(Ts(1_500)));
/// assert_eq!(Ts(1_500).saturating_sub(Ts(1_000)), Dur(500));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ts(pub u64);

impl Size {
    /// `size × price`, in minor units, or `None` on overflow (SPEC §2.5).
    ///
    /// The product is taken in `u128` and checked on narrowing, which is the only place the
    /// two dimensions meet. There is no division anywhere in the money path, so there is no
    /// rounding rule, no dust, and no "who eats the remainder" question — the representation
    /// makes it unaskable rather than answering it (§2.1).
    #[must_use]
    pub fn checked_mul(self, price: Price) -> Option<Amount> {
        let product = u128::from(self.0).checked_mul(u128::from(price.0))?;
        u64::try_from(product).ok().map(Amount)
    }

    /// The requester's contribution for this leg: `size × price`.
    #[must_use]
    pub fn requester_contribution(self, price: Price) -> Option<Amount> {
        self.checked_mul(price)
    }

    /// The maker's contribution for this leg: `size × (UNIT − price)`.
    ///
    /// The two contributions sum to `size × UNIT` **exactly**, by construction.
    #[must_use]
    pub fn maker_contribution(self, price: Price) -> Option<Amount> {
        let complement = Price(UNIT.0.checked_sub(price.0)?);
        self.checked_mul(complement)
    }

    /// The escrowed notional for this leg: `size × UNIT`.
    #[must_use]
    pub fn notional(self) -> Option<Amount> {
        self.checked_mul(UNIT)
    }
}

impl Ts {
    /// The epoch itself.
    pub const ZERO: Self = Self(0);

    /// `Ts + Dur -> Ts`: this instant advanced by `duration`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, duration: Dur) -> Option<Self> {
        match self.0.checked_add(duration.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `Ts + Dur -> Ts`, clamped at the end of time.
    #[must_use]
    pub const fn saturating_add(self, duration: Dur) -> Self {
        Self(self.0.saturating_add(duration.0))
    }

    /// `Ts − Ts -> Dur`: the duration from `earlier` to `self`, or zero if `self` is not
    /// later.
    ///
    /// Saturating rather than checked because the caller of a difference between two
    /// instants always wants a duration, and a negative one is not representable. Every
    /// ordering question is asked with `<` against the sampled `now` instead (SPEC §4.2),
    /// which is why no code needs to distinguish "zero elapsed" from "in the past".
    #[must_use]
    pub const fn saturating_sub(self, earlier: Self) -> Dur {
        Dur(self.0.saturating_sub(earlier.0))
    }
}

/// A **length of time**, in milliseconds (SPEC §4.0).
///
/// Every configured bound is a `Dur`: `MAX_QUOTE_TTL`, `MAX_REQUEST_TTL`, `MIN_HORIZON`,
/// `MAX_SETTLING_TIME`, `STALL_GRACE`, and all four terms of the §9.3 withdrawal timelock
/// inequality. Both startup assertions therefore compare a `Dur` with a `Dur`, which is
/// what they were always doing informally.
///
/// A duration is not an instant, and cannot stand in for one:
///
/// ```compile_fail,E0308
/// use rfq_core::clock::TestClock;
/// use rfq_core::types::Dur;
/// let _ = TestClock::at(Dur(0));
/// ```
///
/// ```compile_fail,E0369
/// use rfq_core::types::Dur;
/// let _ = Dur(1) + Dur(2);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dur(pub u64);

impl Dur {
    /// No time at all.
    pub const ZERO: Self = Self(0);

    /// `Dur + Dur -> Dur`, or `None` on overflow. Overflow is a rejection, never a wrap:
    /// a wrapped sum in the §9.3 inequality would compute a tiny bound and cheerfully
    /// accept a timelock that covers nothing.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `Dur × u32 -> Dur`, or `None` on overflow. This is `CONFIRMATIONS × block_time`,
    /// the balance-mirror lag term of SPEC §9.3.
    #[must_use]
    pub fn checked_mul(self, count: u32) -> Option<Self> {
        self.0.checked_mul(u64::from(count)).map(Self)
    }
}
