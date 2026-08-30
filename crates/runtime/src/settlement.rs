//! The settlement adapter: the one path from the engine to custody (SPEC §13.1).
//!
//! The engine emits [`Event::SubmitIntent`]; the adapter turns it into a [`Bundle`] and
//! hands that to custody. Nothing else crosses, in either direction. Custody cannot call
//! back into the engine, and no engine path can reach a balance.
//!
//! The conversion is deliberately a *conversion* and not a shared type. `IntentLeg` belongs
//! to the engine's event vocabulary and `BundleLeg` to the chain's transaction vocabulary;
//! making them one type would be the two systems sharing a definition, which is the first
//! step to sharing memory. Writing the translation out is what keeps the seam visible — and
//! in v2 this function is where the wire format goes.

use rfq_chain::bundle::{Bundle, BundleLeg};
use rfq_core::config::MAX_LEGS;
use rfq_core::event::Event;

/// Turn a `SubmitIntent` into a settlement transaction.
///
/// Returns `None` for every other event: the adapter subscribes to the whole stream and
/// picks out the one thing custody is entitled to see.
#[must_use]
pub fn bundle_from(event: &Event) -> Option<Bundle> {
    let Event::SubmitIntent { nonce, requester, legs, n_legs, .. } = event else {
        return None;
    };
    let mut converted = [BundleLeg::default(); MAX_LEGS];
    for (index, leg) in legs.iter().take(usize::from(*n_legs)).enumerate() {
        if let Some(slot) = converted.get_mut(index) {
            *slot = BundleLeg {
                contract: leg.contract,
                side: leg.side,
                size: leg.size,
                maker: leg.maker,
                fill_price: leg.fill_price,
                quote_expiry: leg.quote_expiry,
            };
        }
    }
    Some(Bundle { nonce: *nonce, requester: *requester, legs: converted, n_legs: *n_legs })
}
