# Known limitations

Not part of the deliverables. This is what the design contains rather than closes: hazards it
bounds instead of eliminating, properties the tests do not actually establish, and things it
does not do at all. `failure-modes.md` is the summary of what *is* handled;
`failure-modes-full.md` is the complete hazard listing with covering tests.

Written down because a limitation named by the author is a decision, and the same limitation
found by a reader is an oversight.

## Bounded, not eliminated

Eight hazards are contained rather than closed. Which is which matters more than a list that
appears complete.

| Hazard | Bound | What stays open |
|---|---|---|
| Custody has resolved a settlement; the engine has not learned | Closed when the terminal answer reaches the engine through the chain log | Claim coverage genuinely does not hold inside the window — asserted by name, not ignored. The engine cannot shorten it: it waits to be told, and the settling deadline only alerts |
| Settlement indefinitely unknown | Nothing bounds it | Capital held indefinitely, on both sides — the requester's contribution and every winning maker's. Releasing on a guess is the duplication path, so the deadline alerts and moves nothing |
| Oracle parked mid-contest | Only the escalation authority ends it | Escrows locked indefinitely |
| A quote can expire between acceptance and inclusion | Nothing bounds it — selection checks only that a quote is live now, not that it outlives the settlement round trip | The fill reverts safely, but the requester's capital is immobilised for the settling window for a fill they could not have known was doomed, and a maker quoting a very short TTL can impose that at symmetric cost. `max_settlement_inclusion_time` exists in config and is consulted nowhere near selection; quote TTL has a maximum and no minimum. Not built |
| Reorg deeper than the confirmation depth | Confirmation depth avoids shallower ones entirely — nothing is delivered until it is buried, so an orphaned entry is never applied | A deeper reorg would leave an applied outcome that never really happened, and outcomes are immutable, so the engine would be permanently wrong. Rolling back and reapplying from the command log is designed, not built |
| Event loss | Buffer capacity; gaps visible via sequence numbers, which are assigned to dropped events too | No resynchronisation for a consumer that fell behind |
| Slab or buffer exhaustion | Configured capacity, plus admission bounds that cap occupancy by participants and legs rather than by message volume | A full slab rejects honest traffic until claims expire. Nothing degrades or signals as capacity is approached: the venue is fine until it is refusing everything |
| Escalation authority | One designated identity — and that is the whole of the bound | A single unbonded key with no appeal, free to rule any outcome including `Void` on any contested contract. A real optimistic oracle makes a false answer expensive by slashing a bond; this makes it free. The design's one trusted component. A forced unwind could be reached four ways; engine rules close three of them, and this is the one left open |
## What the tests do not establish

**The lost-acknowledgement case is invisible to every invariant.** Both layers stay internally
consistent while the model comes apart, so conservation and claim coverage both pass. Carrying
the fate on the nonce prevents it; nothing catches it afterwards.

**Quote eligibility is tested, but not by the end-to-end scenarios.** An over-limit quote is
also worse-priced, so ignoring eligibility would change an outcome only where no eligible quote
exists at all, a path no scenario reaches. A direct unit test covers it.

## Not built

**Cross-basket atomicity.** Within a basket every leg settles in one custody transaction
against one nonce and reverts whole. Across baskets there is no atomicity, and none is claimed.

**Authorisation**, out of scope. Nothing binds a command to an identity, so "requester
only" and "oracle only" are gateway properties rather than engine ones: a forged acceptance
identical to a real one would be obeyed.
