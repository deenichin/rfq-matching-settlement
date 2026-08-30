//! S1 gate: randomised sequences against SPEC §15 invariants 1 and 2, and against the
//! property that normalisation cannot reach committed capital.
//!
//! The invariants are asserted in the **chain-sum** form. The expiry-predicate form —
//! `reserved == Σ` over claims with `now < expires_at` — is unsatisfiable against a stored
//! total for any un-normalised account and must not be attempted (CLAUDE 16). §15.2 is the
//! predicate one, and it is scoped to the account just normalised, which is what makes it
//! true.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use proptest::prelude::*;
use rfq_core::account::AccountIdx;
use rfq_core::config::Config;
use rfq_core::ledger::{Ledger, LedgerError, SlabKind};
use rfq_core::request::ReqIdx;
use rfq_core::reservation::{ResIdx, ResOwner};
use rfq_core::types::{Amount, Ts};

const ACCOUNTS: u32 = 3;
const CAPACITY: u32 = 12;
/// Each account is funded to exactly `CAPACITY` claims of one unit, so the slab runs out
/// at roughly the same time the money does and both rejection paths are exercised. No
/// surplus: slack would absorb a claim released twice (CLAUDE 38).
const FUNDING: u64 = CAPACITY as u64;

fn fresh_ledger() -> Ledger {
    let config = Config {
        max_accounts: ACCOUNTS,
        max_reservations: CAPACITY,
        max_requests: 4,
        max_quotes: CAPACITY,
        ..Config::default()
    };
    let mut ledger = Ledger::new(&config);
    for account in 0..ACCOUNTS {
        ledger.apply_mirror_update(AccountIdx(account), Amount(FUNDING)).unwrap();
    }
    ledger
}

/// One randomised step.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// Reserve one unit on `account` expiring at `expires_at`.
    Reserve { account: u32, expires_at: u64 },
    /// Release the live claim at this position in the live set.
    Release(usize),
    /// Commit the live claim at this position into the request at that position.
    Commit(usize, usize),
    /// Normalise `account` at `now`.
    Normalise { account: u32, now: u64 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u32..ACCOUNTS, 1u64..40).prop_map(|(account, expires_at)| Op::Reserve { account, expires_at }),
        2 => any::<usize>().prop_map(Op::Release),
        3 => (any::<usize>(), any::<usize>()).prop_map(|(c, r)| Op::Commit(c, r)),
        4 => (0u32..ACCOUNTS, 0u64..45).prop_map(|(account, now)| Op::Normalise { account, now }),
    ]
}

/// What the test believes is outstanding, so it can tell a correct rejection from a bug.
struct Model {
    /// Claims still on an expiry chain, with the account and expiry they were made under.
    reserved: Vec<(ResIdx, u32, u64)>,
    /// Claims moved to a committed list, with the expiry they *used to* have.
    committed: Vec<(ResIdx, u32, u64)>,
}

proptest! {
    /// PLAN S1 gate: invariants 1 and 2 over randomised reserve/release/commit/expire, plus
    /// gate (d) — the allocation proxy of CLAUDE 25.
    #[test]
    fn invariants_1_and_2_hold_over_randomised_sequences(ops in proptest::collection::vec(op(), 0..200)) {
        let mut ledger = fresh_ledger();
        let capacity = ledger.reservation_capacity();
        let requests: Vec<ReqIdx> = (0..4).map(|_| ledger.open_request().unwrap()).collect();
        let mut model = Model { reserved: Vec::new(), committed: Vec::new() };

        for op in ops {
            match op {
                Op::Reserve { account, expires_at } => {
                    let account = AccountIdx(account);
                    let Ok(quote) = ledger.open_quote() else { continue };
                    match ledger.reserve(account, Amount(1), Ts(expires_at), ResOwner::Quote(quote)) {
                        Ok(claim) => model.reserved.push((claim, account.0, expires_at)),
                        // The only admissible refusals, and each must be true of the model.
                        Err(LedgerError::SlabExhausted { slab: SlabKind::Reservation }) => {
                            prop_assert_eq!(ledger.reservation_count(), capacity);
                        }
                        Err(LedgerError::InsufficientFree) => {
                            let entry = ledger.account(account).unwrap();
                            prop_assert_eq!(entry.available(), Amount::ZERO);
                        }
                        Err(other) => prop_assert!(false, "unexpected reserve rejection: {:?}", other),
                    }
                }
                Op::Release(pick) => {
                    if !model.reserved.is_empty() {
                        let (claim, ..) = model.reserved.remove(pick % model.reserved.len());
                        prop_assert_eq!(ledger.release(claim), Ok(Amount(1)));
                    }
                }
                Op::Commit(claim_pick, request_pick) => {
                    if !model.reserved.is_empty() {
                        let entry = model.reserved.remove(claim_pick % model.reserved.len());
                        let request = requests[request_pick % requests.len()];
                        prop_assert_eq!(ledger.commit(entry.0, request), Ok(()));
                        model.committed.push(entry);
                    }
                }
                Op::Normalise { account, now } => {
                    let account = AccountIdx(account);
                    let now = Ts(now);
                    let reclaimed = ledger.release_expired(account, now).unwrap();

                    // §15.2, scoped to the account just normalised.
                    prop_assert_eq!(ledger.check_normalised(account, now), Ok(()));

                    // The model agrees about exactly which claims died.
                    let before = model.reserved.len();
                    model.reserved.retain(|(_, owner, expires_at)| {
                        !(*owner == account.0 && now.0 >= *expires_at)
                    });
                    prop_assert_eq!(reclaimed as usize, before - model.reserved.len());
                }
            }

            // §15.1 in the chain-sum form, plus chain structure, plus §15.3's
            // resolve-and-point-back. Asserted after every single step, not only at the end.
            prop_assert_eq!(ledger.check_invariants(), Ok(()));

            // Gate (d), the allocation proxy: no container grew (CLAUDE 25).
            prop_assert_eq!(ledger.reservation_capacity(), capacity);
            prop_assert!(ledger.reservation_count() <= capacity);

            // Every live claim resolves; every released one does not.
            for (claim, ..) in model.reserved.iter().chain(model.committed.iter()) {
                prop_assert!(ledger.reservation(*claim).is_some());
            }
        }

        // The stored totals still equal the model's, independently of the chain sums.
        for account in 0..ACCOUNTS {
            let entry = ledger.account(AccountIdx(account)).unwrap();
            let reserved = model.reserved.iter().filter(|(_, a, _)| *a == account).count() as u64;
            let committed = model.committed.iter().filter(|(_, a, _)| *a == account).count() as u64;
            prop_assert_eq!(entry.reserved(), Amount(reserved));
            prop_assert_eq!(entry.committed(), Amount(committed));
        }
    }

    /// The added gate: **after a commit, normalisation must not touch the committed claim** —
    /// driven from randomised interleavings, not a hand-built case.
    ///
    /// The sharp form is differential. Every committed claim is created alongside a sibling
    /// on the same account with the **same expiry**, and the sequence ends with a
    /// normalisation past every expiry generated. The sibling must be reclaimed and the
    /// committed claim must not: same account, same instant, same expiry, opposite fate.
    /// Without the sibling the test could pass on a ledger that normalises nothing at all.
    #[test]
    fn committed_capital_survives_every_normalisation(
        pairs in proptest::collection::vec((0u32..ACCOUNTS, 1u64..30), 1..5),
        interleaved_nows in proptest::collection::vec(0u64..30, 0..8),
    ) {
        let mut ledger = fresh_ledger();
        let request = ledger.open_request().unwrap();
        let mut committed: Vec<(ResIdx, u32, u64)> = Vec::new();
        let mut siblings: Vec<(ResIdx, u32, u64)> = Vec::new();

        for (account, expires_at) in pairs {
            let account = AccountIdx(account);
            let (Ok(quote_a), Ok(quote_b)) = (ledger.open_quote(), ledger.open_quote()) else {
                continue;
            };
            let Ok(claim) = ledger.reserve(account, Amount(1), Ts(expires_at), ResOwner::Quote(quote_a))
            else {
                continue;
            };
            let Ok(sibling) = ledger.reserve(account, Amount(1), Ts(expires_at), ResOwner::Quote(quote_b))
            else {
                // The committed claim needs its twin, or the comparison has nothing to say.
                ledger.release(claim).unwrap();
                continue;
            };
            prop_assert_eq!(ledger.commit(claim, request), Ok(()));
            committed.push((claim, account.0, expires_at));
            siblings.push((sibling, account.0, expires_at));
        }

        // Precondition (CLAUDE 39): the property has something to be about.
        prop_assert!(!committed.is_empty(), "the run must actually commit something");

        let committed_total: Vec<Amount> = (0..ACCOUNTS)
            .map(|a| ledger.account(AccountIdx(a)).unwrap().committed())
            .collect();

        // Normalisations at arbitrary instants, interleaved with the commits already made.
        let last = interleaved_nows.iter().copied().max().unwrap_or(0);
        for now in interleaved_nows.into_iter().chain(std::iter::once(last.max(30) + 1)) {
            for account in 0..ACCOUNTS {
                let account = AccountIdx(account);
                ledger.release_expired(account, Ts(now)).unwrap();
                prop_assert_eq!(ledger.check_normalised(account, Ts(now)), Ok(()));
            }
            prop_assert_eq!(ledger.check_invariants(), Ok(()));

            // Every committed claim still resolves, is still committed, and still has no
            // expiry — at every `now`, not merely at the end.
            for (claim, _, _) in &committed {
                let reservation = ledger
                    .reservation(*claim)
                    .ok_or(TestCaseError::fail("a committed claim was reclaimed"))?;
                prop_assert!(reservation.is_committed());
                prop_assert_eq!(reservation.expires_at(), None);
                prop_assert_eq!(reservation.committed_to(), Some(request));
            }
            // And the stored total never moved.
            for account in 0..ACCOUNTS {
                prop_assert_eq!(
                    ledger.account(AccountIdx(account)).unwrap().committed(),
                    committed_total[account as usize]
                );
            }
        }

        // The differential half: every sibling is gone. Same account, same expiry, and the
        // final normalisation is past all of them — so the only thing that saved the
        // committed claims is that they were not on the chain being walked.
        for (sibling, _, expires_at) in &siblings {
            prop_assert!(
                ledger.reservation(*sibling).is_none(),
                "a reserved claim expiring at {} survived normalisation past it",
                expires_at
            );
        }
        for account in 0..ACCOUNTS {
            prop_assert_eq!(ledger.account(AccountIdx(account)).unwrap().reserved(), Amount::ZERO);
        }
    }
}
