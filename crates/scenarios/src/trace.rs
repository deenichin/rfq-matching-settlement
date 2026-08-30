//! Printing a scenario so a reader can follow the money.
//!
//! **Amounts print as grouped minor units, never decimals.** A decimal point means dividing
//! by `UNIT`, and there is no division in the money path (CLAUDE 15, SPEC §2.1) — the
//! representation was chosen to make the rounding question unaskable, and a formatter that
//! reintroduces it would be answering it in the one place nobody audits. Grouping is done by
//! walking the digits `u64`'s own `Display` produced; this module performs no arithmetic on
//! money at all.

use rfq_core::account::AccountIdx;
use rfq_core::types::{Amount, Price, Size, Ts};

/// `65000000000` → `65,000,000,000`.
#[must_use]
pub(crate) fn minor_units(amount: Amount) -> String {
    group(&amount.0.to_string())
}

/// A price, in minor units per contract. Also never a decimal.
#[must_use]
pub(crate) fn price(price: Price) -> String {
    group(&price.0.to_string())
}

/// A size, in contracts.
#[must_use]
pub(crate) fn size(size: Size) -> String {
    group(&size.0.to_string())
}

/// An instant, in milliseconds since the venue's epoch.
#[must_use]
pub(crate) fn at(now: Ts) -> String {
    format!("t={}", group(&now.0.to_string()))
}

/// Group already-rendered digits in threes. No arithmetic, no division: this walks a string.
fn group(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    let leading = digits.len() % 3;
    for (index, ch) in digits.chars().enumerate() {
        if index != 0 && index % 3 == leading {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// The names a scenario prints instead of account indices.
pub(crate) struct Cast {
    names: Vec<(AccountIdx, &'static str)>,
}

impl Cast {
    /// A cast of named accounts.
    #[must_use]
    pub(crate) fn new(names: Vec<(AccountIdx, &'static str)>) -> Self {
        Self { names }
    }

    /// The name for an account, or its index.
    #[must_use]
    pub(crate) fn name(&self, account: AccountIdx) -> String {
        self.names
            .iter()
            .find(|(index, _)| *index == account)
            .map_or_else(|| format!("account {}", account.0), |(_, name)| (*name).to_owned())
    }

    /// Everyone, in order.
    #[must_use]
    pub(crate) fn everyone(&self) -> &[(AccountIdx, &'static str)] {
        &self.names
    }
}
