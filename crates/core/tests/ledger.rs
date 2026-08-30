//! S1 gate: the ledger's explicit tests (PLAN S1 (a)–(d)) and the claim-relationship
//! invariants of SPEC §15.1–§15.3.
//!
//! The randomised gates live in `ledger_properties.rs`. These are the named cases: each one
//! pins a specific sentence of the specification that a property test would exercise only by
//! accident.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_core::account::AccountIdx;
use rfq_core::config::Config;
use rfq_core::ledger::{Ledger, LedgerError, SlabKind};
use rfq_core::reservation::ResOwner;
use rfq_core::types::{Amount, Ts};

const ALICE: AccountIdx = AccountIdx(0);
const BOB: AccountIdx = AccountIdx(1);

/// A ledger with small slabs and `funded` mirrored to Alice, so exhaustion and insufficiency
/// are both reachable. Accounts are funded to **exactly** what a test spends (CLAUDE 38):
/// slack absorbs a claim released twice and makes coverage pass regardless of correctness.
fn ledger(funded: u64) -> Ledger {
    let config = Config {
        max_accounts: 4,
        max_reservations: 8,
        max_requests: 4,
        max_quotes: 8,
        ..Config::default()
    };
    let mut ledger = Ledger::new(&config);
    ledger.apply_mirror_update(ALICE, Amount(funded)).unwrap();
    ledger
}

/// A quote owner holding no claim yet.
fn quote_owner(ledger: &mut Ledger) -> ResOwner {
    ResOwner::Quote(ledger.open_quote().unwrap())
}

// ───────────────────────────── PLAN S1 gate (a) ─────────────────────────────

#[test]
fn release_expired_reclaims_the_expired_prefix_and_leaves_the_total_equal_to_the_chain_sum() {
    let mut ledger = ledger(600);
    let owners: Vec<ResOwner> = (0..3).map(|_| quote_owner(&mut ledger)).collect();

    // Deliberately inserted out of order, so the chain has to sort them.
    ledger.reserve(ALICE, Amount(300), Ts(3_000), owners[2]).unwrap();
    ledger.reserve(ALICE, Amount(100), Ts(1_000), owners[0]).unwrap();
    ledger.reserve(ALICE, Amount(200), Ts(2_000), owners[1]).unwrap();
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(600));
    assert_eq!(ledger.account(ALICE).unwrap().available(), Amount(0), "funded exactly");

    let reclaimed = ledger.release_expired(ALICE, Ts(2_000)).unwrap();

    // Precondition (CLAUDE 39): the walk actually did something. A normalisation that
    // reclaimed nothing would satisfy every assertion below without exercising anything.
    assert_eq!(reclaimed, 2, "the 1_000 and 2_000 claims are both dead at now = 2_000");
    assert_eq!(ledger.reservation_count(), 1);
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(300));
    assert_eq!(ledger.account(ALICE).unwrap().available(), Amount(300));

    // §15.2, scoped to the account just normalised.
    assert_eq!(ledger.check_normalised(ALICE, Ts(2_000)), Ok(()));
    // §15.1 in the chain-sum form, plus every structural invariant.
    assert_eq!(ledger.check_invariants(), Ok(()));

    // The released owners no longer point at anything; the survivor still does.
    assert!(ledger.quote(match owners[0] { ResOwner::Quote(q) => q, ResOwner::Request(_) => unreachable!() }).unwrap().claim().is_none());
    assert!(ledger.quote(match owners[2] { ResOwner::Quote(q) => q, ResOwner::Request(_) => unreachable!() }).unwrap().claim().is_some());
}

// ───────────────────────────── PLAN S1 gate (b) ─────────────────────────────

#[test]
fn a_claim_expiring_at_exactly_now_is_reclaimed_not_deferred() {
    // Liveness is half-open: live iff `now < expires_at` (SPEC §4.2). A claim expiring at
    // exactly `now` is dead, and the boundary is documented so it can never double-count.
    let mut ledger = ledger(100);
    let owner = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(100), Ts(5_000), owner).unwrap();

    assert_eq!(ledger.release_expired(ALICE, Ts(4_999)).unwrap(), 0, "one ms early: still live");
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(100));

    assert_eq!(ledger.release_expired(ALICE, Ts(5_000)).unwrap(), 1, "at exactly now: dead");
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount::ZERO);
    assert_eq!(ledger.reservation_count(), 0);
}

// ───────────────────────────── PLAN S1 gate (c) ─────────────────────────────

#[test]
fn a_stale_claim_handle_from_a_freed_slot_is_rejected_by_generation_mismatch() {
    let mut ledger = ledger(100);
    let first_owner = quote_owner(&mut ledger);
    let stale = ledger.reserve(ALICE, Amount(100), Ts(1_000), first_owner).unwrap();
    assert_eq!(ledger.release(stale).unwrap(), Amount(100));

    // Reissue the slot to a different owner and a different amount.
    let second_owner = quote_owner(&mut ledger);
    let live = ledger.reserve(ALICE, Amount(60), Ts(2_000), second_owner).unwrap();

    // Precondition (CLAUDE 39): the slot really was reused, so the handle is stale rather
    // than merely dangling into empty space.
    assert_eq!(stale.index(), live.index(), "the test needs the slot to be reused");
    assert_ne!(stale.generation(), live.generation());

    let request = ledger.open_request().unwrap();
    assert_eq!(ledger.reservation(stale), None);
    assert_eq!(ledger.release(stale), Err(LedgerError::StaleReservation));
    assert_eq!(ledger.commit(stale, request), Err(LedgerError::StaleReservation));

    // And the stale operations did not disturb the live claim.
    assert_eq!(ledger.reservation(live).unwrap().amount(), Amount(60));
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(60));
    assert_eq!(ledger.check_invariants(), Ok(()));
}


// ───────────────────────────── the two-variant distinction ─────────────────────────────

#[test]
fn releasing_committed_capital_is_a_different_error_from_a_stale_handle() {
    // SPEC §4.3: a handle denoting committed capital is refused with `ReservationCommitted`,
    // distinct from `StaleReservation`. They are different bugs in the caller — one is
    // holding a reference past its lifetime, the other is trying to release capital that
    // may not be released on a guess (§8.3) — and must not share a variant.
    let mut ledger = ledger(100);
    let owner = quote_owner(&mut ledger);
    let request = ledger.open_request().unwrap();
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), owner).unwrap();
    ledger.commit(claim, request).unwrap();

    assert_eq!(ledger.release(claim), Err(LedgerError::ReservationCommitted));
    assert_ne!(ledger.release(claim), Err(LedgerError::StaleReservation));
    // The handle still resolves — it is committed, not gone.
    assert!(ledger.reservation(claim).is_some());
    assert_eq!(ledger.account(ALICE).unwrap().committed(), Amount(100));
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount::ZERO);
}

#[test]
fn commit_moves_a_claim_between_chains_in_one_step() {
    let mut ledger = ledger(300);
    let a = quote_owner(&mut ledger);
    let b = quote_owner(&mut ledger);
    let request = ledger.open_request().unwrap();
    let first = ledger.reserve(ALICE, Amount(100), Ts(1_000), a).unwrap();
    let second = ledger.reserve(ALICE, Amount(200), Ts(2_000), b).unwrap();

    ledger.commit(first, request).unwrap();

    // Off the expiry chain, on the committed list, and with no expiry at all.
    let committed = ledger.reservation(first).unwrap();
    assert!(committed.is_committed());
    assert_eq!(committed.expires_at(), None, "a committed claim has no expiry (§2.4)");
    assert_eq!(committed.committed_to(), Some(request));

    // The totals moved together.
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(200));
    assert_eq!(ledger.account(ALICE).unwrap().committed(), Amount(100));

    // The survivor is still reserved and still expiring.
    assert_eq!(ledger.reservation(second).unwrap().expires_at(), Some(Ts(2_000)));
    assert_eq!(ledger.check_invariants(), Ok(()));

    // Committing twice is refused, and refused as *committed*, not as stale.
    assert_eq!(ledger.commit(first, request), Err(LedgerError::ReservationCommitted));
}

#[test]
fn normalisation_cannot_reach_committed_capital() {
    // The named case; `ledger_properties.rs` drives the same property from randomised
    // interleavings. Two claims, same account, **same expiry** — one committed, one not.
    let mut ledger = ledger(300);
    let committed_owner = quote_owner(&mut ledger);
    let reserved_owner = quote_owner(&mut ledger);
    let request = ledger.open_request().unwrap();
    let committed = ledger.reserve(ALICE, Amount(100), Ts(1_000), committed_owner).unwrap();
    let reserved = ledger.reserve(ALICE, Amount(200), Ts(1_000), reserved_owner).unwrap();
    ledger.commit(committed, request).unwrap();

    let reclaimed = ledger.release_expired(ALICE, Ts(9_999)).unwrap();

    assert_eq!(reclaimed, 1, "only the reserved sibling — the same expiry, a different fate");
    assert_eq!(ledger.reservation(reserved), None);
    assert!(ledger.reservation(committed).is_some(), "committed capital survives its own expiry");
    assert_eq!(ledger.account(ALICE).unwrap().committed(), Amount(100));
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount::ZERO);
    assert_eq!(ledger.check_invariants(), Ok(()));
}

// ───────────────────────────── ordering and structure ─────────────────────────────

#[test]
fn insertion_orders_the_chain_by_expiry_whatever_order_claims_arrive_in() {
    let mut ledger = ledger(500);
    let owners: Vec<ResOwner> = (0..5).map(|_| quote_owner(&mut ledger)).collect();
    // Arrival order 5, 1, 4, 2, 3 — every insertion position exercised: after the tail,
    // before the head, and three times into the middle.
    for (i, expiry) in [5_000_u64, 1_000, 4_000, 2_000, 3_000].into_iter().enumerate() {
        ledger.reserve(ALICE, Amount(100), Ts(expiry), owners[i]).unwrap();
    }
    assert_eq!(ledger.check_invariants(), Ok(()), "chain-order is part of check_invariants");

    // Reclaiming from the head must come out in expiry order, one step at a time.
    for (now, expected_remaining) in [(1_000, 4), (2_000, 3), (3_000, 2), (4_000, 1), (5_000, 0)] {
        assert_eq!(ledger.release_expired(ALICE, Ts(now)).unwrap(), 1);
        assert_eq!(ledger.reservation_count(), expected_remaining);
    }
}

#[test]
fn unlinking_the_only_claim_clears_both_endpoints() {
    // A head-only fix-up leaves a dangling tail, which the next insertion then walks back
    // from into a freed slot. The chain-integrity check is what notices.
    let mut ledger = ledger(200);
    let first = quote_owner(&mut ledger);
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), first).unwrap();
    ledger.release(claim).unwrap();
    assert_eq!(ledger.check_invariants(), Ok(()));

    let second = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(100), Ts(500), second).unwrap();
    assert_eq!(ledger.check_invariants(), Ok(()));
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(100));
}

#[test]
fn chains_are_per_account_and_do_not_interfere() {
    let mut ledger = ledger(200);
    ledger.apply_mirror_update(BOB, Amount(300)).unwrap();
    let alice_owner = quote_owner(&mut ledger);
    let bob_owner = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(200), Ts(1_000), alice_owner).unwrap();
    let bob_claim = ledger.reserve(BOB, Amount(300), Ts(1_000), bob_owner).unwrap();

    assert_eq!(ledger.release_expired(ALICE, Ts(5_000)).unwrap(), 1);
    assert_eq!(ledger.account(BOB).unwrap().reserved(), Amount(300), "Bob is untouched");
    assert!(ledger.reservation(bob_claim).is_some());
    assert_eq!(ledger.check_invariants(), Ok(()));
}

// ───────────────────────────── admission and exhaustion ─────────────────────────────

#[test]
fn a_claim_beyond_the_mirrored_balance_is_refused() {
    // Claim coverage (§15.6) is maintained by the ledger rather than asserted after the
    // fact: `free ≥ reserved + committed` cannot be broken by a caller.
    let mut ledger = ledger(100);
    let first = quote_owner(&mut ledger);
    let second = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(100), Ts(1_000), first).unwrap();

    assert_eq!(
        ledger.reserve(ALICE, Amount(1), Ts(1_000), second),
        Err(LedgerError::InsufficientFree)
    );
    // Rejection mutates nothing (§15.4).
    assert_eq!(ledger.reservation_count(), 1);
    assert_eq!(ledger.account(ALICE).unwrap().reserved(), Amount(100));
    assert!(ledger.quote(match second { ResOwner::Quote(q) => q, ResOwner::Request(_) => unreachable!() }).unwrap().claim().is_none());
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn slab_exhaustion_is_a_rejection_naming_the_slab() {
    let config = Config {
        max_accounts: 2,
        max_reservations: 2,
        max_requests: 1,
        max_quotes: 8,
        ..Config::default()
    };
    let mut ledger = Ledger::new(&config);
    ledger.apply_mirror_update(ALICE, Amount(300)).unwrap();
    for _ in 0..2 {
        let owner = quote_owner(&mut ledger);
        ledger.reserve(ALICE, Amount(100), Ts(1_000), owner).unwrap();
    }

    let overflow = quote_owner(&mut ledger);
    assert_eq!(
        ledger.reserve(ALICE, Amount(100), Ts(1_000), overflow),
        Err(LedgerError::SlabExhausted { slab: SlabKind::Reservation })
    );
    assert_eq!(ledger.reservation_capacity(), 2, "a rejection never grows the slab");

    ledger.open_request().unwrap();
    assert_eq!(ledger.open_request(), Err(LedgerError::SlabExhausted { slab: SlabKind::Request }));
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn one_owner_holds_at_most_one_claim() {
    // A quote backs exactly one reservation; a request backs exactly one requester-side
    // reservation (§2.4). A second is a caller bug with its own variant.
    let mut ledger = ledger(200);
    let owner = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(100), Ts(1_000), owner).unwrap();
    assert_eq!(
        ledger.reserve(ALICE, Amount(100), Ts(1_000), owner),
        Err(LedgerError::OwnerAlreadyClaimed)
    );

    let request = ledger.open_request().unwrap();
    let requester = ResOwner::Request(request);
    ledger.reserve(ALICE, Amount(100), Ts(1_000), requester).unwrap();
    assert_eq!(
        ledger.reserve(ALICE, Amount(0), Ts(1_000), requester),
        Err(LedgerError::OwnerAlreadyClaimed)
    );
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn a_requester_side_claim_has_no_quote() {
    // The reason `ResOwner` is an enum: the requester's `Σ size × limit_price` is reserved
    // at SubmitRequest, before any price exists and before any quote arrives (§4.3, §5.2).
    let mut ledger = ledger(100);
    let request = ledger.open_request().unwrap();
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), ResOwner::Request(request)).unwrap();

    assert_eq!(ledger.reservation(claim).unwrap().owner(), ResOwner::Request(request));
    assert_eq!(ledger.request(request).unwrap().claim(), Some(claim));
    assert_eq!(ledger.check_invariants(), Ok(()));

    // Committing the requester's own claim into its own request's list is the accept path
    // of §7.2, and the owner still points back afterwards.
    ledger.commit(claim, request).unwrap();
    assert_eq!(ledger.request(request).unwrap().claim(), Some(claim));
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn a_claim_against_an_unknown_account_is_refused() {
    let mut ledger = ledger(100);
    let owner = quote_owner(&mut ledger);
    assert_eq!(
        ledger.reserve(AccountIdx(99), Amount(1), Ts(1_000), owner),
        Err(LedgerError::UnknownAccount)
    );
    assert_eq!(ledger.release_expired(AccountIdx(99), Ts(1)), Err(LedgerError::UnknownAccount));
    assert_eq!(ledger.free(AccountIdx(99), Ts(1)), Err(LedgerError::UnknownAccount));
}

// ───────────────────────────── invariant 3, both directions ─────────────────────────────

#[test]
fn an_owner_holding_capital_cannot_be_closed_out_from_under_it() {
    // The guard that keeps §15.3's "the owner must resolve" half from ever being needed:
    // freeing a quote slot while a claim names it would leave that claim pointing at
    // whatever is reissued into the slot, which is the stale-handle class the generation
    // counters exist to catch.
    let mut ledger = ledger(100);
    let ResOwner::Quote(quote) = quote_owner(&mut ledger) else { unreachable!() };
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), ResOwner::Quote(quote)).unwrap();

    assert_eq!(ledger.close_quote(quote), Err(LedgerError::OwnerStillClaimed));
    assert!(ledger.quote(quote).is_some(), "a refused close frees nothing");

    // Release first, then close. The ordering is the whole rule.
    ledger.release(claim).unwrap();
    assert_eq!(ledger.close_quote(quote), Ok(()));
    assert_eq!(ledger.check_invariants(), Ok(()));

    // The handle is now stale, and reserving against it is refused on dereference.
    assert_eq!(ledger.quote(quote), None);
    assert_eq!(
        ledger.reserve(ALICE, Amount(1), Ts(1_000), ResOwner::Quote(quote)),
        Err(LedgerError::StaleOwner)
    );
}

#[test]
fn a_request_holding_committed_capital_cannot_be_closed() {
    // Committed capital may not be released on a guess (§8.3), so the request holding it
    // cannot be freed either — its committed list is the only thing that knows about it.
    let mut ledger = ledger(100);
    let owner = quote_owner(&mut ledger);
    let request = ledger.open_request().unwrap();
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), owner).unwrap();
    ledger.commit(claim, request).unwrap();

    assert_eq!(ledger.close_request(request), Err(LedgerError::OwnerStillClaimed));
    assert!(ledger.request(request).is_some());
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn releasing_a_claim_clears_the_owners_back_pointer() {
    // Invariant 3 read from the owner's end. If release left the quote pointing at a freed
    // claim, the claim→owner direction would have nothing to look at — the claim is gone —
    // so only the reverse direction can catch it.
    let mut ledger = ledger(100);
    let ResOwner::Quote(quote) = quote_owner(&mut ledger) else { unreachable!() };
    let claim = ledger.reserve(ALICE, Amount(100), Ts(1_000), ResOwner::Quote(quote)).unwrap();
    assert_eq!(ledger.quote(quote).unwrap().claim(), Some(claim));

    ledger.release(claim).unwrap();
    assert_eq!(ledger.quote(quote).unwrap().claim(), None);
    assert_eq!(ledger.check_invariants(), Ok(()));

    // The same, through bulk reclamation rather than a named release.
    let second = ledger.reserve(ALICE, Amount(100), Ts(2_000), ResOwner::Quote(quote)).unwrap();
    assert_eq!(ledger.quote(quote).unwrap().claim(), Some(second));
    assert_eq!(ledger.release_expired(ALICE, Ts(2_000)).unwrap(), 1);
    assert_eq!(ledger.quote(quote).unwrap().claim(), None);
    assert_eq!(ledger.check_invariants(), Ok(()));
}

#[test]
fn free_normalises_before_answering() {
    // The admission query reclaims first, so no decision about A is ever taken against a
    // stale `reserved` total (§4.3). Without this the answer is wrong by exactly the
    // amount of whatever expired while nobody was looking.
    let mut ledger = ledger(100);
    let owner = quote_owner(&mut ledger);
    ledger.reserve(ALICE, Amount(100), Ts(1_000), owner).unwrap();
    assert_eq!(ledger.account(ALICE).unwrap().available(), Amount::ZERO);

    assert_eq!(ledger.free(ALICE, Ts(1_000)).unwrap(), Amount(100));
    assert_eq!(ledger.reservation_count(), 0, "answering the query performed the reclamation");
}
