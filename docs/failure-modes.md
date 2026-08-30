# Failure modes

Twenty-six hazards are handled, covered by seventy-two tests. This page carries the
non-obvious ones.

What the design bounds rather than eliminates, what the tests do not establish, and what is
not built are in `known-limitations.md`. The complete hazard listing with covering tests is in
`failure-modes-full.md`.

## Races

**A settlement is included and the acknowledgement is lost.** The engine retries and is
rejected, because the nonce is used. That rejection says nothing about the trade: the chain
reports the fate of the *nonce*, which still reads settled. A retry bouncing off its own
consumed nonce is evidence the original **succeeded**, and reading it as failure would release
capital sitting in escrow.

**A withdrawal matures between the pre-flight check and the settlement.** The pre-check has no
correctness role; it only saves a doomed submission. Settlement validates every leg against
live balances inside the transaction and debits all or none, so the basket reverts whole.

**A participant tries to withdraw capital their own live quote has promised.** Withdrawals
execute only after a delay exceeding the longest a quote or request can bind, plus every lag
between chain and engine, so the claim always dies first. A venue configured otherwise refuses
to start — which also makes insufficient funds at settlement unreachable unless the engine's
view is allowed to lag.

**Venue time and chain time disagree.** Each system holds its own clock, so a quote the engine
believes live may be expired at custody. Settlement is judged by the chain's clock and the
basket reverts safely.

**The quotes change between being shown and being accepted.** The acceptance carries the prices
the requester saw and selection re-runs then: better fills better, worse rejects and changes
nothing. An expiring best quote publishes nothing — the view is stale by design — and the
acceptance re-derives from live quotes rather than filling at a dead price.

**Chain events arrive out of order**, a payout request before the resolution authorising it.
The payout re-derives the outcome as it pays, so it is refused now and succeeds later. Nothing
is queued on the engine's behalf.

**The oracle changes its answer.** Status moves forward only and a final answer is never
replaced. Otherwise two makers holding identical positions on one contract would be paid
opposite results depending on who claimed first — with conservation still holding, since each
escrow pays its own notional. Contesting buys no unwind either: a contested contract never
times out, and only the escalation authority ends one.

**Several clients submit at once.** One thread owns all state behind a bounded channel; every
command lands once in channel order, and replaying the log rebuilds the same state exactly.

## Partial failures

**One leg of a three-leg basket cannot be filled after the others matched.** Selection happens
in a phase that mutates nothing, so the whole request aborts before any capital moves: ledger
unchanged, no event emitted, the other legs' quotes untouched, request still open. There is no
partial-fill state and nothing to unwind.

Capital could leak in exactly one place — between reserving one leg's fill and finding another
unfillable. What prevents it is that acceptance mutates nothing until every leg is selected and
validated, after which the commit phase is a type exposing only infallible operations, so the
leak has no window to open in.

**A maker's second quote would exceed what their first has already reserved.** Admission refuses 
it — the ledger maintains free ≥ reserved + committed and cannot be talked out of it. A maker 
never holds claims they cannot cover, so a bundle they cannot afford never forms.

**The engine's view of balances falls behind the chain.** Availability drops the moment a withdrawal 
is requested, and until the indexer delivers that entry the engine can admit a quote against 
money already leaving. The timelock closes it, not the mirror: indexer lag is a term in the 
withdrawal-delay inequality, so a quote admitted while the engine was over-optimistic expires before 
the money can move.
