# Resolution and payout

How money gets out of escrow, and what the engine does when the answer is disputed, late, or
unanswerable. Tests are in `crates/runtime/tests/`.

## How funds unlock

A confirmed settlement leaves one escrow per leg. Each holds exactly `size × UNIT` — the full
notional — contributed by both parties: `price × size` from the requester, `(UNIT − price) ×
size` from the maker. The two sum to the notional exactly, which is why the money path needs
no division and produces no dust. Only `Locked` escrows hold money, so only they count toward
conservation (`custody.rs::conservation_counts_locked_escrows_only`).

Unlocking is one command, `SettleEscrow { escrow, contract }`, delivered by the indexer when
somebody sends a payout transaction. The engine asks the contract for its outcome **at that
moment**, refuses with `OutcomeNotYet` if there is none, maps the outcome through the leg's
side to decide who is paid, and emits an intent that an adapter turns into a payout crediting
the winner the whole notional and marking the escrow `Settled` in the same mutation.

No escrow stores an outcome. Deriving at the moment of payment is what makes delivery order
irrelevant — a payout request arriving before the resolution it depends on is refused now and
succeeds later, rather than paying out against an answer the engine has not seen
(`indexer.rs::a_settlement_request_arriving_before_its_resolution_is_refused_and_works_afterwards`).

Payout maps through the leg's side. The requester wins exactly when the outcome matches the
side they bought and the maker wins otherwise — there is no fixed convention that one party is
the buyer. One `Yes` outcome therefore pays in both directions across a spread: the requester
takes the leg they bought `Yes`, the maker takes the leg the requester bought `No`
(`resolution.rs::one_outcome_pays_the_requester_on_a_yes_leg_and_the_maker_on_a_no_leg`).

## Disputed

The engine has no concept of a dispute. It sees one status per contract — `Silent`,
`InProgress`, `Final` — and contestation lives entirely in the oracle, which is a system the
venue does not control and deliberately does not model.

Mechanically, a contested contract sits at `InProgress`. No outcome is derivable, so every
`SettleEscrow` on it is refused and its escrows stay `Locked`, indefinitely if necessary. The
only thing that ends it is the escalation authority reporting `Final`
(`resolution.rs::a_contested_contract_ends_only_when_the_escalation_authority_rules`).

**Status only moves forward**, and `Final` is terminal even against an identical re-report
(`::the_oracle_cannot_walk_its_status_backwards`, `::a_final_contract_refuses_every_later_report`).
This is the rule most worth defending, because nothing else catches what it prevents. Suppose
a resolved contract could be overwritten, and two escrows on it — identical positions, same
side, size and price, different makers — are settled either side of the overwrite. One pays
the requester, the other pays the maker: two makers who wrote the same trade receive opposite
answers, decided by nothing but who claimed first.

And every invariant passes. No unit is duplicated, each escrow pays its own notional out of
its own contributions, and both layers stay internally consistent — so conservation cannot see
it (`::final_is_immutable_and_the_failure_it_prevents_is_invisible_to_every_invariant`, which
constructs the failure by calling custody directly and settling two escrows on one contract
under opposite outcomes).

Nothing else catches it because nothing else *can*. Custody validates two things about a
payout — that the escrow belongs to the named contract, and that it is still `Locked` — and
holds no contract state at all, so it pays whatever outcome it is handed. The guarantee is
therefore positional rather than structural: it holds because the engine is the only path to
the money in this wiring. Were custody a real escrow contract reading the oracle itself, the
engine's contract state would become a projection like the balance mirror, and the same rule
would have to be enforced in the contract instead.

## Delayed

If the oracle says nothing at all and `now > event_date + stall_grace`, the outcome derives to
`Void` and each side is refunded its own contribution. The exit is triggerable only by the
passage of time; no participant can call it.

It conditions on `Silent`, not on elapsed time. An oracle that has said *anything* is
`InProgress` and never times out, however long it sits
(`::an_oracle_parked_in_progress_never_times_out_into_void`). Otherwise a losing party would
contest and wait, converting the safety valve for a dead oracle into a free option to cancel a
trade already lost.

**`Void` refunds contributions; it is not a 50/50 split.** A leg of 100,000 filled at 0.61
holds 61,000 from the requester and 39,000 from the maker. `Void` returns exactly those,
restoring the pre-trade allocation. Splitting the notional would credit 50,000 each — a
transfer of 11,000 from the requester to the maker, decided by nothing but the oracle having
failed, and worse the further the fill sits from the midpoint. Refunding contributions is the
only unwind that leaves no party better off for the failure, which is what makes it safe to
trigger on a timer. Note that the side mapping is irrelevant here: each party gets back what
they put in regardless of which side they bought
(`::a_silent_oracle_past_the_grace_period_returns_each_side_its_own_contribution`).

## Ambiguous wording

The venue cannot decide that two wordings mean the same thing and does not try. Contract
identity is byte equality, so a single byte's difference is a different contract with its own
escrows and its own resolution. There is no dispute state, no re-wording path, and no way to
merge two indices that turn out to mean the same thing.

Mechanically it terminates like any other unresolvable contract: the escalation authority
rules, and where the wording genuinely cannot be resolved the ruling is `Void` — both sides
get their contributions back and nobody profits from the ambiguity. The venue's answer is not
an interpretation; it is an unwind that leaves the parties where they started.

## The escalation authority

A **single unbonded key** that can set any outcome, including `Void`, on any contested
contract, with no bond to slash and no appeal above it. Everything else in this design is
mechanical; this is not.

It is also the only route to a forced unwind that stays open. The stall exit requires `Silent`,
monotonicity blocks retraction and overwrite, and `InProgress` never times out — three routes
closed by engine rules, one left as a trust assumption. A real optimistic oracle makes a false
answer expensive by slashing a bond; this makes it free. Whether the key should be a multisig,
a committee or a bonded set is a governance decision left open. What the design commits to is
that there is exactly one such route and it is visible.
