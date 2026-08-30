# An RFQ venue with on-chain settlement

A requester opens a multi-leg request, makers compete on each leg, the requester accepts, and
the resulting positions are escrowed and paid out when an oracle resolves them.

## Running it

The host needs only Docker.

```
docker compose run --rm test         # the whole suite
docker compose run --rm scenarios    # six end-to-end traces, printed
docker compose run --rm lint         # clippy, warnings denied
```

The image carries the toolchain only and the working tree is mounted, so a run without
`--build` compiles what is on disk. An image with the source baked in can report a pass for
code that no longer exists, and the difference is invisible in the output.

## The deliverables

| | |
|---|---|
| `docs/state-machine.md` | Every state and transition, who can trigger each, and the leg-2-of-3 trace |
| `docs/failure-modes.md` | Races and partial failures handled, one page |
| `docs/resolution-design.md` | How funds unlock; disputed, delayed and ambiguous outcomes |
| `docs/design-note.md` | What changes if quotes live for days rather than seconds |
| `docs/known-limitations.md` | What is bounded rather than closed, what the tests do not establish, what is not built |
| `docs/failure-modes-full.md` | All 26 hazards with their covering tests, for diffing against the suite |

`SPEC.md` is the design and is authoritative; §16 lists the scope cuts and argues each.
`PLAN.md` is the order it was built in, with estimates against actuals.

## Who owns what

**Matching** is `crates/core`: requests, legs, quotes, selection, and the accept that turns
winning quotes into an intent. Single-writer, no I/O, no allocation on the command path, and
no knowledge that a chain exists.

**Custody** is `crates/chain`: balances, escrows, nonces, the oracle, the chain log. It holds
the money and knows nothing about requests, quotes or legs.

**Reservation** bridges them and belongs to neither. Capital is reserved in the engine's own
ledger and settled in custody; the two reconcile only by message. The engine reaches custody by
emitting an intent an adapter submits, custody reaches the engine through the chain log and the
indexer. There is no direct call either way, and a build script fails the compile if `core`
ever declares a dependency on `chain`. Each holds its own clock, because venue time and chain
time genuinely differ. The harness in `crates/runtime` owns both and is the only thing that may
read both — which is why conservation and claim coverage are asserted there.

## On the choice of structures

The reservation ledger is a preallocated slab of claims on intrusive doubly-linked chains: one
per account ordered by expiry for reserved claims, one per request for committed ones. A global
expiry structure swept by a background pass was rejected for two reasons — it needs a thread
mutating state outside a command, forfeiting the single-writer property that makes invariants
checkable at one point in time, and it does work proportional to the whole venue when the only
capital a command can need belongs to the accounts it touches. Expiry is a predicate evaluated
against a single sampled `now`, and reclamation happens inline in the command that needs it.
Nothing sweeps, so no test can pass because a sweeper happened to run first.

Claim links are an enum rather than a struct with optional fields, so a committed claim has no
`expires_at` at all — the state that must not be reachable is not representable. Slab handles
carry a generation, so a handle to a retired slot is rejected rather than resolving to whatever
now occupies it.

## Reading a trace

One line in the scenario output looks wrong and is not. After a maker's quote expires, the
balance table can still show that maker holding the capital it reserved — in the happy path,
Beta reads 96,000,000,000 reserved several steps after its leg-B quote died. Nothing has touched
Beta's account since, and there is no sweeper: the stored total is the sum of the account's
chain, not of its live claims, and it reconciles on the next command that needs Beta's capital.

## Where the mock publishes conclusions instead of facts

Custody is a mock, and in three places it hands the indexer an answer where a real chain would
emit a fact. Balance entries carry *availability* rather than the raw deposit and withdrawal
events an indexer would fold into it; `SettlementResolved` carries a status rather than a
receipt to be interpreted; and only terminal answers are logged, so "still pending" is not an
event. Each is defensible for a mock, but it means the indexer does not carry the weight it
would against a real chain, and the interpretation logic that would live there is untested
because it does not exist.
