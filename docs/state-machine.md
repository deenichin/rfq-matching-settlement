# The request lifecycle

A request is the unit of work: a requester asks for one or more legs, makers quote each leg,
the requester accepts, and the resulting positions are escrowed and later paid out. Four state
machines are involved — request, quote, contract, escrow. A request's own transitions depend
on quotes, at accept, and on the settlement nonce; contract and escrow states are downstream
of a request that has already reached a terminal state and never move it again.

Every claim below names the test that proves it. Tests live in `crates/core/tests/` and
`crates/runtime/tests/`.

## Who can trigger anything at all

Commands reach the engine from exactly **two** producers. Nothing else touches engine state.

| Producer | Commands it can raise | On behalf of |
|---|---|---|
| Gateway | `RegisterContract`, `SubmitRequest`, `RejectRequest`, `AcceptRequest`, `SubmitQuote`, `CancelQuote` | Requesters and makers |
| Indexer | `CreditAccount`, `PollSettlement`, `SettleEscrow`, `ReportOracleStatus` | The chain, after confirmation depth |

The oracle is not a third producer. Its status reaches the engine the way every other outside
fact does: the oracle adapter appends an `OracleStatusReported` entry to the chain log, and the
indexer translates it into `ReportOracleStatus`, subject to the same confirmation depth and the
same `(tx_hash, log_index)` dedup as a balance change. The same is true of `SettleEscrow`,
which anyone may send and which therefore arrives as a log entry rather than by a privileged
path.

The "requester only" and "oracle only" attributions below are designed and not enforced — see
*Authorisation* at the end.

## Request

```
                      RejectRequest (requester)
              ┌──────────────────────────────────────────► Rejected ▪
              │
  ▸ Open ─────┤
     │        │   AcceptRequest (requester)                PollSettlement{Settled}   (indexer)
     │        └──────────────────────────► Settling ──┬──────────────────────────────► Escrowed ▪
     │                                        │       │
     │                                        │       │   PollSettlement{Reverted}   (indexer)
     │                                        │       └──────────────────────────────► SettlementFailed ▪
     │                                        │
     │                                        └── PollSettlement{Unknown | Pending} moves nothing
     │
     └── Expired: derived from now ≥ deadline. Never stored, not a node.
```

- **Open → Rejected**, by the requester. Every standing quote on every leg is released and its
  maker told by name. `market.rs::rejecting_a_request_releases_every_standing_quote`.
- **Open → Settling**, by the requester, via `AcceptRequest`. The requester states the prices
  they expect; selection re-runs at accept time and the fill binds at-or-better, so a better
  quote arriving in between fills at the better price
  (`market.rs::a_better_quote_arriving_before_the_accept_fills_at_the_better_price`) and a
  worse one rejects and mutates nothing
  (`market.rs::a_worse_selection_at_accept_time_rejects_and_mutates_nothing`).
- **What prevents Open → Settling.** A leg with no eligible quote aborts the whole request, and
  each cause returns its own variant (`market.rs::every_leg_failure_cause_returns_its_own_variant`):
  no quote ever arrived; every quote on the leg is dead; every live quote is priced outside the
  leg's limit; no eligible quote covers the full size. The last is unreachable — admission
  refuses an undersized quote — and the variant is kept to name that closure rather than hide it.
- **Expired is derived, not stored.** There is no `Expired` node and no sweeper: a request past
  its deadline is expired by predicate and admits nothing.
  `market.rs::a_request_past_its_deadline_is_expired_by_derivation_and_admits_nothing`.
- **Settling → Escrowed / SettlementFailed**, by the indexer only, carrying the *nonce's*
  terminal fate. `settlement.rs::a_settled_nonce_moves_the_request_to_escrowed_and_discharges_the_claims`
  and `::a_reverted_nonce_fails_the_settlement_and_returns_every_claim_to_free`.
- **Settling under `Unknown` or `Pending` moves nothing**, deliberately. Both sides' capital
  stays committed rather than being released on a guess, because releasing early is the
  double-spend path. `settlement.rs::an_unknown_nonce_holds_every_claim_and_commits_nothing_twice`.
- **The settling deadline releases nothing.** Reaching it raises an alert. The engine does not
  ask the chain anything — it waits to be told, so the deadline is a signal to an operator
  rather than a transition. `settlement.rs::reaching_the_settling_deadline_alerts_and_releases_nothing`.

## Quote

```
  ▸ Active ──┬── won at accept ──────────────────────────────► Consumed ▪
             └── outbid / replaced / request rejected /
                 found dead during an accept ────────────────► Released ▪
```

Live means `Active && now < expires_at` — half-open, always, evaluated against the single `now`
sampled for the command. **Expiry is not a transition and emits nothing**: a quote whose time
has passed is still stored as `Active` and is simply never selected. Its capital is reclaimed
by the next command touching that maker's account. It is written to `Released` only when a
command sweeps it — an accept or a rejection walking the leg, or the maker replacing it — so a
dead quote on a leg that never fills stays `Active` until one of those happens.

- **Active → Consumed** for the winner; the slot stays alive because a committed claim names it
  as owner and that owner must resolve.
  `settlement.rs::a_committed_claim_names_a_consumed_quote_at_every_step_of_settling`.
- **Active → Released** by replacement
  (`market.rs::a_second_quote_from_one_maker_replaces_the_first_and_occupies_one_slot`), and a
  replacement that is worse is refused with the standing quote surviving
  (`market.rs::a_worse_replacement_is_refused_and_the_standing_quote_survives`).
- **A maker cannot cancel.** `CancelQuote` is a *rejected transition, not an absent one*: the
  quote is looked up so that an unknown quote and an irrevocable one return different errors.
  `market.rs::cancel_quote_is_a_rejected_transition_not_an_absent_one`.

## Contract, and the oracle status beside it

```
  ▸ Unresolved ── ReportOracleStatus{Final(o)} ──► Resolved(o) ▪

  oracle status, monotonic:   ▸ Silent ──┬──► InProgress ──► Final ▪
                                         └──► Final ▪
```

Both paths to `Final` are real edges. An oracle that reports a decision without first announcing
it was working is not regressing, so `Silent → Final` is admissible; `InProgress → Final` is the
ordinary case, a proposal resolving after its window. Monotonicity is enforced by rank
comparison and `Final` is terminal against itself — an identical re-report is refused, because
"identical, so harmless" is the reasoning that lets a real overwrite through later.
`resolution.rs::the_oracle_cannot_walk_its_status_backwards`,
`::a_final_contract_refuses_every_later_report`.

**Void is never stored.** A contract that is `Silent` past `event_date + stall_grace` yields
`Void` by derivation at the moment of use. Only a `Final` report is written to state.

## Escrow

```
  ▸ Locked ── SettleEscrow, once the contract has an outcome ──► Settled ▪
```

Only `Locked` escrows hold money, which is why conservation counts them and not `Settled` ones
(`custody.rs::conservation_counts_locked_escrows_only`). Settling twice is a no-op
(`resolution.rs::settling_the_same_escrow_twice_is_a_no_op`) and an escrow cannot be settled
under a different contract's outcome
(`resolution.rs::an_escrow_cannot_be_settled_under_another_contracts_outcome`).

## Leg 2 of 3 fails to fill after leg 1 has provisionally matched

This is the case the design is built around, and the answer is that **"provisionally matched"
is not a state**. It is a local variable inside the PLAN phase of `AcceptRequest`, and it never
becomes observable.

`AcceptRequest` runs in three phases. PLAN walks every leg and selects the best eligible live
quote on each, purely, mutating nothing. CHECK validates the whole plan — every leg filled,
every price at-or-better than expected, enough event-buffer headroom. Only then does COMMIT
run, and COMMIT is infallible: no fallible call, no allocation, and no `?` appears after it
begins. A leg with no eligible quote fails in PLAN, before a single byte has moved.

Concretely, with legs 0 and 2 quoted and leg 1 left unquoted:

- the command returns `NoEligibleQuote { leg: 1, reason: NoQuotes }`;
- the ledger is **byte-identical** to its post-normalisation state, compared by a debug hash
  taken after normalisation, because normalisation runs before PLAN and is not part of the
  command's effect;
- the quotes on legs 0 and 2 are still `Active`. Their makers are never told they nearly
  traded, because they did not;
- **no event is emitted at all**, so nothing leaked to any maker;
- the request is still `Open`, so a later accept succeeds if a quote arrives.

`market.rs::leg_two_of_three_has_no_eligible_quote_and_the_whole_request_aborts` asserts all
five. `market.rs::the_same_rejection_with_an_expired_reservation_still_compares_equal` covers
the same abort when normalisation has reclaimed capital in between, which is the case where a
naive before-and-after comparison would report a false mutation.

There is no partial-fill state, no unwind path, and no compensating transaction — because there
is nothing to compensate.

## The reachability property

`crates/core/tests/reachability.rs` enumerates all fifteen states across the request, quote,
contract, oracle-status and escrow machines, and every transition the engine can actually
perform, then asserts that the set of non-terminal states with **no reachable exit** is exactly
what the file declares. The graph is written out by hand rather than derived from the command
handlers, because a graph derived from the code it checks would agree with any bug that code
contains.

Four tests:

- `every_non_terminal_state_has_a_reachable_exit` — the declared set is **empty**. The assertion
  is an *equality, not a subset*: a state appearing means a real exit is missing, a state
  disappearing means the declaration has gone slack.
- `settling_now_has_both_of_its_exits_and_the_declared_set_is_empty` — `Settling` leaves by
  `PollSettlement` to `Escrowed` or `SettlementFailed`, and both edges exist.
- `every_terminal_state_really_is_terminal` — the other half. A test that only looked for missing
  exits would pass on a machine that let a settled request go back to `Open`.
- `the_two_declared_exceptions_of_spec_15_9_are_still_exactly_two`.

**The two declared exceptions**, both stuck-by-choice:

1. `Settling` under indefinite `Unknown` — the nonce has no terminal answer yet.
2. An escrow whose oracle parks in `InProgress` — contested and not yet ruled on.

Both are about **when an exit is taken, not whether one exists**, and the test asserts exactly
that: it requires a transition out of `Request::Settling` and a transition out of
`Escrow::Locked` to be present in the graph. Neither is a hole. `Settling` has both its exits
and simply does not take them while the answer it needs is missing; a `Locked` escrow has its
exit and does not take it while the contract has no outcome. That distinction is the whole
reason the exceptions are tolerable — a missing edge is a design defect, whereas an untaken edge
is a policy, and the policy in both cases is that waiting is cheaper than guessing wrong about
money. The count is asserted so a third cannot be added quietly.

## Authorisation

Signature verification is out of scope, so nothing binds a command to an identity beyond the
dense index the gateway assigns. The "requester only" and "oracle only" attributions above are
properties of the gateway's admission, not of the engine: a forged `AcceptRequest` identical to
a real one would be obeyed. The permission model is a boundary that exists on paper.

This is a named absence rather than an oversight — see SPEC §16. Note what is and is not proved:
`dependency_seam.rs::the_engine_depends_on_nothing` and
`::no_path_in_the_engine_can_read_a_custody_balance` establish that the seam between the two
systems is real. Neither says anything about authorisation, and no test does, because there is
nothing to assert.
