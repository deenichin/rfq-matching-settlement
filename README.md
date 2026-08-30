# An RFQ venue with on-chain settlement

A request-for-quote venue: a requester opens a multi-leg request, makers compete on each
leg, the requester accepts, and the resulting positions are escrowed on chain and paid out
when an oracle resolves them.

`SPEC.md` is the design and is authoritative. `PLAN.md` is the order it was built in. This
file is how to run it.

## Running it

Everything runs in a container; the host needs only Docker.

```
docker compose run --rm test         # the whole test suite — every stage gate in PLAN.md
docker compose run --rm scenarios    # the six end-to-end traces, printed
docker compose run --rm lint         # clippy over all targets, warnings denied
```

The image carries the toolchain and nothing else. `compose.yml` mounts the working tree at
`/app` rather than copying it into the image, so a `docker compose run` without `--build`
compiles the files that are on disk right now. This matters more than it looks: an image
that had baked in a copy of the source would happily report a pass for code that no longer
exists, and the difference is invisible in the output. Mounting the tree means a green run
is a statement about the current working tree. The S0 gate checks this directly, by adding
a failing test, running without `--build`, and confirming it fails.

Build artifacts live in a named volume shared by the three services, so running `test`
after `scenarios` reuses the compilation rather than repeating it.

## Who owns what

Three concerns are kept in three places, and the separation is enforced rather than
intended. **Matching** lives in `crates/core`: requests, legs, quotes, selection, and the
accept that turns a set of winning quotes into an intent. It is a synchronous single-writer
state machine with no I/O, no allocation on the command path, and no knowledge that a chain
exists. **Custody** lives in `crates/chain`: balances, escrows, nonces, the oracle, and the
chain log. It holds the money and knows nothing about requests, quotes or legs. **Reservation**
is the bridge between them and belongs to neither: capital is reserved in the engine's own
ledger when a quote is admitted and committed when a request is accepted, but it is *settled*
in custody, and the two ledgers are reconciled only by messages. The engine reaches custody
by emitting a `SubmitIntent` event that an adapter turns into a submission; custody reaches
the engine by writing to the chain log, which an indexer translates into commands. There is
no direct call in either direction, and `crates/core` declares no dependency on
`crates/chain` — a build script fails the compile if it ever does. Each system holds its own
clock, because venue time and chain time genuinely differ and the design has to survive that.
The harness in `crates/runtime` owns both and is the only thing that may read both; that is
why conservation and claim coverage are asserted there and not inside either system.

## On the choice of structures

The reservation ledger is a preallocated slab of claims threaded onto intrusive doubly-linked
chains: one chain per account, ordered by expiry, holding its reserved claims, and one list
per request holding its committed ones. The obvious alternative — a global expiry structure,
a timer wheel or a priority queue, swept by a background pass — was rejected for two reasons.
It needs a thread that mutates state outside a command, which forfeits the single-writer
property that makes every invariant checkable at one point in time; and it does work
proportional to the whole venue when the only capital a command can possibly need is the
capital of the accounts that command touches. Expiry here is a predicate, `Active && now <
expires_at`, evaluated against a single sampled `now`; reclamation happens inline, in the
command that needs it, by walking one account's chain from the front and stopping at the
first live claim. Nothing sweeps, so no test can pass by accident because a sweeper happened
to run first.

The claim links are an enum rather than a struct with optional fields, so a committed claim
does not have an `expires_at` at all — the state that must not be reachable is not
representable. Handles into the slabs carry a generation counter, so a handle to a retired
slot is rejected rather than silently resolving to whatever now occupies it, and a slot whose
generation would wrap is retired instead. Where an ordered map is genuinely unavoidable, at
the gateway boundary that maps external identifiers onto dense indices, it is a `BTreeMap`:
iteration order there is part of the observable behaviour, and hash order is not a thing a
deterministic system may depend on.

## What was left out

`SPEC.md` §16 lists the cuts and argues each one. They are decisions, not omissions, and
the reasoning is worth more than the list.
