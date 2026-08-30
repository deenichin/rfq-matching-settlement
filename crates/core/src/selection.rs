//! Selection: a pure function over live quotes (SPEC §7.1).
//!
//! **No stored decision.** The filling set is computed fresh from the live quote set every
//! time it is needed, so it cannot go stale. §7.1.1's published selection is a *publication
//! cache* for the presentation feed, never an input to a fill.
//!
//! The order of operations is the design:
//!
//! 1. **Eligibility first.** A quote is eligible only if its price is at or better than the
//!    leg's limit. This is the **sole** enforcement point for the limit price (§5.2). An
//!    ineligible quote is never selected and never published, so `BestSelectionChanged` can
//!    never present a price the requester has not authorised — and nothing is refused at
//!    *admission* on price, which is what keeps the limit private.
//! 2. **Rank by price ascending.** The price is always the price of the side the requester
//!    is buying (§2.1), so lowest-is-best holds for `Yes` and `No` legs alike and selection
//!    needs no side-specific branch.
//! 3. **Tiebreak by arrival.** Deterministic, and it denies a maker any gain from spamming
//!    identical quotes.
//! 4. **Full size or nothing.** No aggregation across makers on one leg in v1.

use crate::ledger::Ledger;
use crate::quote::QuoteIdx;
use crate::request::{Leg, Selection};
use crate::slab::Handle;
use crate::types::Ts;

/// Why a leg has no fill.
///
/// Four causes, four variants (§7.2, CLAUDE 24). A single `NoQuote` would tell a requester
/// that a leg failed and nothing about whether to wait, re-price, or give up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoQuoteReason {
    /// No maker ever quoted this leg.
    NoQuotes,
    /// Makers quoted, and every quote is dead at `now`.
    AllExpired,
    /// Live quotes exist and every one is priced outside the leg's limit, so the eligible
    /// set is empty. Distinct from `NoQuotes`: makers responded, their quotes are live and
    /// **reserving capital**, and they are excluded at selection rather than at admission.
    OutsideLimit,
    /// Live, eligible quotes exist and none covers the leg's full size.
    ///
    /// **Unreachable by construction**, and left here deliberately. §6 refuses
    /// `size < leg.size` at *admission* (`QuoteTooSmall`), so no undersized quote is ever on
    /// a chain to be excluded here. Deleting the variant would hide the fact that §7.2 names
    /// four causes and admission closes one of them; keeping it names the closure.
    SizeTooSmall,
}

/// The best eligible live quote on `leg`, or why there is none.
///
/// Pure: reads state, mutates nothing, and takes the single sampled `now` so that every leg
/// of a multi-leg accept agrees about what time it is (§4.1).
///
/// # Errors
///
/// [`NoQuoteReason`], naming which of §7.2's causes applies.
pub fn best_on_leg(
    ledger: &Ledger,
    leg: &Leg,
    now: Ts,
) -> Result<Selection, NoQuoteReason> {
    let mut best: Option<Selection> = None;
    let mut best_arrival = u64::MAX;
    let mut saw_quote = false;
    let mut saw_live = false;
    let mut saw_big_enough = false;

    let mut cursor = leg.quotes_head;
    let mut steps: u32 = 0;
    while let Some(index) = cursor.index() {
        let Some(handle) = ledger.quote_handle_at(index) else { break };
        let Some(quote) = ledger.quote(handle) else { break };
        cursor = quote.next_on_leg;
        steps = steps.saturating_add(1);
        debug_assert!(u8::try_from(steps).is_ok(), "a leg chain must be bounded");

        saw_quote = true;
        if !quote.is_live_at(now) {
            continue;
        }
        saw_live = true;
        if quote.size() < leg.size() {
            continue;
        }
        saw_big_enough = true;
        // Eligibility first (§7.1): at or better than the limit, or it is not a candidate.
        if quote.price() > leg.limit() {
            continue;
        }

        let better = match best {
            None => true,
            // Strictly lower price wins; equal price falls to the earlier arrival.
            Some(current) => {
                quote.price() < current.price
                    || (quote.price() == current.price && quote.arrival() < best_arrival)
            }
        };
        if better {
            best = Some(Selection { quote: handle, price: quote.price() });
            best_arrival = quote.arrival();
        }
    }

    best.ok_or({
        if !saw_quote {
            NoQuoteReason::NoQuotes
        } else if !saw_live {
            NoQuoteReason::AllExpired
        } else if saw_big_enough {
            NoQuoteReason::OutsideLimit
        } else {
            NoQuoteReason::SizeTooSmall
        }
    })
}

/// The live quote this maker already has standing on `leg`, if any.
///
/// One live quote per maker per leg (§6). A new quote replaces it — but only if the new
/// price is at least as good for the requester, or replacement becomes a cancel primitive in
/// disguise: requote at `price == UNIT`, which reserves zero, and the maker has withdrawn
/// liquidity they promised was irrevocable.
#[must_use]
pub fn standing_quote_of(
    ledger: &Ledger,
    leg: &Leg,
    maker: crate::account::AccountIdx,
) -> Option<QuoteIdx> {
    let mut cursor = leg.quotes_head;
    while let Some(index) = cursor.index() {
        let handle: QuoteIdx = ledger.quote_handle_at(index)?;
        let quote = ledger.quote(handle)?;
        cursor = quote.next_on_leg;
        if quote.maker() == maker && matches!(quote.state(), crate::quote::QuoteState::Active) {
            return Some(handle);
        }
    }
    None
}

/// Every quote on `leg`, oldest chain position first, as handles.
///
/// Used by the commit phase, which must visit each of them exactly once. The chain is walked
/// rather than collected, so nothing allocates: the caller is handed one handle at a time.
pub fn walk_leg<F: FnMut(QuoteIdx)>(ledger: &Ledger, leg: &Leg, mut visit: F) {
    let mut cursor = leg.quotes_head;
    while let Some(index) = cursor.index() {
        let Some(handle): Option<Handle<crate::quote::Quote>> = ledger.quote_handle_at(index)
        else {
            break;
        };
        let Some(quote) = ledger.quote(handle) else { break };
        cursor = quote.next_on_leg;
        visit(handle);
    }
}
