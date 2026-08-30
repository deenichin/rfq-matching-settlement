# PLAN.md

Staged build. Every stage ends at a **gate**: a passing test, not a judgement call.
One commit per gate. A stage that overruns its box is cut to its gate and the remainder
is moved to the write-up as designed-not-implemented.

## Budget

Estimates below were calibrated against a throwaway prototype of S0–S2, built to test the
design before committing to it. The prototype came in at 93 minutes against 215 estimated —
a ratio of 0.43, with S2 alone at 33 against 110 — and surfaced several design corrections
that are folded into SPEC.md. It was discarded; this build starts clean.

| | Stages | Estimate | Prototype actual |
|---|---|---|---|
| Foundation | S0, S1, S1.5 | 105m | **60m** |
| Matching | S2 | 110m | **33m** |
| Custody & settlement | S3, S4 | 90m | — |
| Resolution & boundary | S5, S6 | 60m | — |
| Packaging | S7 | 40m | — |
| Write-ups | S8 | 60m | — |

Projecting the remainder at 0.45, S0–S7 lands near **3h**, so S4 (asynchronous
settlement) and S6 (indexer) are **in scope**, not extensions. They were cut from
the core on budget grounds alone; the budget no longer requires it.

The write-ups do not scale with the rest — S8 is an hour of writing whatever the
code cost, and is a third of total effort. That is the right proportion:
deliverables 3, 4 and 5 are where judgment is read.

The overshoot is a property of the design being settled before any code was written, not
of the stages being trivial: every decision inside S0–S2 had already been made and argued.

---

## Workspace

```
crates/
  core/        zero dependencies — the ENGINE. Types, ledger, reservations, quotes,
               request machine, contract state, engine::apply. Holds the balance
               mirror and EscrowIds; cannot read a balance from custody.
  chain/       CUSTODY and its neighbours — escrow contract, tx pool, withdrawal
               timelock, event log, mocked oracle, indexer. Knows nothing of
               requests, quotes, legs, reservations or claims.
  runtime/     clock impls, command channel, publisher thread, the settlement
               adapter, and the HARNESS that owns one engine + one custody and
               drives the wires between them (SPEC §13.1).
  scenarios/   binary — named end-to-end scenarios with printed traces
Dockerfile, compose.yml        # repo root; services: test, scenarios, lint
```

`core` depends on nothing. `chain` depends on `core` types only. `runtime` wires them.
Nothing depends on `scenarios`.

---

## S0 — Skeleton and harness · 40m

- Workspace: `core` (zero deps), `chain`, `runtime`, `scenarios`. `proptest` the only
  dev-dependency. Workspace lints per CLAUDE.md.
- `Amount(u64)`, `Price(u32)`, `Size(u64)` newtypes; `Ts(u64)` instants and `Dur(u64)`
  durations as **separate** types with only the operations of SPEC §4.0 defined
- `Clock` trait + `TestClock` (settable) + `MonotonicClock`. **Custody gets its own
  instance** (SPEC §9.1) — two clocks from the start, offsettable in tests
- Slab: preallocated, free list, **generation-counted `u32` handles**
- `MAX_LEGS`, `MAX_QUOTES_PER_LEG`, `MAX_QUOTE_TTL`, `MAX_REQUEST_TTL`, `MIN_HORIZON`,
  `MAX_SETTLING_TIME`, `STALL_GRACE` and the four §9.3 timelock terms as **injectable
  config**, not hard constants. Two startup assertions: the four-term timelock inequality,
  and `MIN_HORIZON > MAX_REQUEST_TTL + MAX_SETTLING_TIME`. Lag terms default to zero and
  must be settable so a violating config is constructible in a test
- Committed `Cargo.lock` (the Docker build uses `--locked`)
- `Command` and `Event` enums — stubs
- **Harness skeleton** (SPEC §13.1): a structure owning one engine and one custody instance,
  advancing both clocks independently, with the cross-system assertion hooks stubbed.
  `core` must not depend on `chain` — assert that direction at compile time in this stage,
  because once it is violated every later claim about the seam becomes untrue quietly
- Dockerfile + compose at the repo root: `test`, `scenarios`, `lint` services.
  **The image is the toolchain only; the working tree is mounted at `/app` and build
  artifacts go to a named volume.** Do not bake sources into the image: `docker compose run`
  reuses an existing image unless `--build` is passed, so a baked image silently re-runs the
  previous stage's binaries and reports green with the new tests simply absent. That failure
  is near-undetectable — the suite passes, and only the test *count* reveals it.
  Do not use the dummy-source dependency-cache trick either: `rm -rf crates` leaves
  fingerprints in `target/`, and Cargo's mtime check then links the real sources against a
  stale empty library. Cache `cargo fetch` only; the dependency compile is seconds.

**Gate** — `docker compose run --rm test` green and `--rm lint` clean, on a machine with
no host toolchain. Confirm the harness cannot report a stale pass: add a failing test, run
*without* `--build`, and see it fail. Slab property test: allocate/free/reallocate N times; capacity never
exceeded; **a handle from a freed slot is rejected by generation mismatch** (this is the
property the accept path and the settlement nonce depend on — index-reuse alone proves
nothing). Startup assertion test: a config violating the timelock inequality fails to
start.

---

## S1 — Ledger and reservations · 35m

- Balance mirror `Vec<MirroredBalance>` indexed by `AccountIdx` — projection, not authority
- Reservation slab, preallocated, generation-counted `ResIdx`, carrying
  `owner: ResOwner{Quote|Request}` and **two chain roles**: the account's expiry-ordered
  chain while `reserved`, the request's committed list while `committed` (SPEC §3, §4.3).
  All slabs carry generation counters — the request slab's supplies the settlement nonce
  (SPEC §8.1). No allocating container anywhere — CLAUDE 9
- `release_expired(acct, now)` — walks the account's chain from the head, stops at the first
  live entry. The sole reclamation mechanism
- `reserve()`, `release()` (O(1) unlink), `commit()` (unlink from expiry chain, link into
  the request's committed list, in one step), `free(acct, now)`
- Normalisation emits **no events** (SPEC §4.3) — it is unbounded in count and runs before
  the CHECK phase can verify event-buffer headroom

**Gate** — property test over randomised reserve/release/expire sequences asserting SPEC §15
invariants 1 (chain integrity) and 2 (normalisation). These are the two forms the old single
invariant collapsed into; asserting a global expiry predicate against a stored total is
unsatisfiable and must not be attempted (CLAUDE 16).
Explicit tests: (a) after `release_expired`, no expired entry remains on the chain, and
`account.reserved` equals the chain sum; (b) a reservation expiring at exactly `now` is
reclaimed, not deferred; (c) a stale `ResIdx` from a freed slot is rejected by generation
mismatch; (d) **slab length and chain capacity are unchanged after N randomised operations**
— the allocation proxy from CLAUDE 25.

---

## S1.5 — Runtime and the single-writer proof · 30m

The concurrency claim is load-bearing (SPEC §13) and currently proved by nothing.

- Bounded `sync_channel` command queue; engine thread busy-spins and is the sole writer
- Publisher thread consumes an event buffer; no I/O anywhere in `apply`
- Normalisation (`release_expired`) runs inside `apply`, before PLAN, from the sampled
  `now`. No loop-level sweep and no global expiry structure exist (SPEC §4.3)

**Gate** — the one declared thread exception (CLAUDE 28). Several **client** threads submit
concurrently on the channel; the engine stays single-threaded. Assertions, all decidable:
(a) every submitted command appears exactly once in the command log, in channel order;
(b) replaying that log through a fresh engine single-threaded reproduces the final state
byte-for-byte — this is the real content of the single-writer claim, and is stronger than
"the result is one of N! legal orders", which degenerates to asserting conservation;
(c) conservation holds throughout.
No allocation-counting harness (CLAUDE 25); the proxy assertion lives in S1's gate (d).

---

## S2 — Request, quote, gateway · 110m · **the stage that will overrun**

- **Gateway** (owned by this stage, SPEC §3, §5.3): external ids → dense indices; contract
  identity by **byte equality** over description + event date + resolution source; the
  `description_bytes → ContractIdx` map. Descriptions never cross into the core. No hashing
  anywhere
- `SubmitRequest` — legs each carrying contract, **side (`Yes`/`No`)**, size and limit
  price; `deadline − now <= MAX_REQUEST_TTL`
  (`DeadlineTooFar`); `now < event_date − MIN_HORIZON` (`ContractTooNear`); requester
  reservation at `Σ size × limit_price`, owned by the request (`ResOwner::Request`)
- `SubmitQuote` — admission checks (SPEC §6): **no price-based rejection**; maker reservation
  against `leg.size`; **one live quote per maker per leg**, a new quote releasing the
  maker's previous one in the same command; intrusive per-leg quote chain
- `RejectRequest`, derived `Expired`
- Selection: best live quote per leg, price then arrival sequence
- `BestSelectionChanged` published on every change; `AcceptRequest` carries expected
  per-leg prices with at-or-better semantics
- `AcceptRequest` plan/check/commit. Commit is local and infallible: quotes marked
  Consumed/Released, reservations released, request → `Settling`, `SubmitIntent` emitted.
  **No custody call inside commit** (CLAUDE 18). Escrow does not exist at the end of S2.
- `CancelQuote` present as a rejected transition with a test asserting the rejection
- `QuoteRejected{Outbid}` / `QuoteExpired` emitted to losing makers

**Gate** — **three-leg** request throughout, matching the deliverable's own example.
(a) happy path: **3 legs with mixed sides** — at least one `Yes` and one `No`, so the
spread from the design's motivating example is exercised rather than a parlay — 5 quotes,
accept, correct winners, all losing reservations released
with `QuoteRejected` emitted, over-reservation released. Assert **both** SPEC §2.2
invariants separately: conservation (custody-side only — `reserved` and `committed` are
claims against `custody.free`, never summed with it) and claim coverage
(`custody.free(a) ≥ reserved(a) + committed(a)`). The request ends in `Settling`, so both
contributions must be in `committed` and coverage must still hold.
(b) **leg 2 of 3** has no eligible quote → `NoEligibleQuote{leg:2}`, state byte-identical
**to the post-normalisation state** (SPEC §15.4 — take the hash after normalisation, not
before the command), legs 1 and 3's quotes still `Active`, no maker notified.
(b2) the same rejection with the requester holding an expired reservation: normalisation
reclaims it, the command still rejects, and the hash comparison still passes — proving the
invariant is stated against the right baseline.
(c) each of the four leg-failure causes returns its own distinct error variant. The
outside-limit case is `NoEligibleQuote{leg, reason: OutsideLimit}` — the quotes were
**admitted and reserved**, and are excluded at selection, not at admission. Gate (f) below
asserts the admission half of the same behaviour; they describe one design, not two.
(d) a better quote arrives between presentation and accept → fills at the better price.
(d2) `RequestOpened` carries each leg's contract description, side and size to makers, and
**does not carry the limit price** (SPEC §5.2) — without this event no maker learns a request
exists and the venue is not an RFQ.
(d3) the best quote expires with nothing arriving: no event is published, the requester's
view is stale, and an accept against it fails `PresentationStale` rather than filling badly.
This pins the feed as eventually-consistent-but-safe (SPEC §7.1.1).
(e) a worse selection at accept time → `PresentationStale`, nothing mutated.
(f) a quote priced above the leg limit is **admitted and reserves capital**, then is
ineligible at selection — no price-based rejection exists, so no free probing channel
(SPEC §6, §7.1). `BestSelectionChanged` never publishes a price above the leg limit.
(f2) a second quote from the same maker on the same leg **replaces** the first, releasing its
reservation; N quotes from one maker occupy one slab slot, not N (SPEC §6).
(f3) a quote at `price == UNIT` reserves zero and is still bounded by (f2) — the zero-cost
spam channel is closed by replacement, not by a capital floor.
(f4) a replacement at a **worse** price is rejected with `WorseReplacement` and the existing
quote stands — replacement improves or does nothing, so it is not a cancel primitive
(SPEC §6).
(f5) `MAX_QUOTES_PER_LEG` is enforced, so the commit phase's event count is statically
bounded and `EventBufferFull` is reachable only by construction, never by quote volume.
(g) exhaustive state-graph reachability test: every non-terminal state in the request, quote
and contract machines has at least one reachable exit, with the **two** declared exceptions
of SPEC §15.9 — `Settling` under indefinite `Unknown`, and an escrow whose oracle parks in
`InProgress`. Both are stuck-by-choice and argued in their sections. CLAUDE rule 30 forbids
adding a third to make this pass.

---

## S3 — Custody mock · 45m

- Balances, `Deposit` / `RequestWithdrawal` / `ExecuteWithdrawal`
- Withdrawal timelock; the **four-term** startup assertion of SPEC §9.3, all lag terms
  injectable and zero by default
- Atomic `settle`: validate all → revert wholesale, or debit all and form escrows
- Escrow stores both contributions separately
- Nonce set, consumed inside the settling transaction
- `on_settle_entry` test hook
- **Harness wiring completed** (SPEC §13.1): the harness pumps engine events → settlement
  adapter → custody, and later the chain log → indexer → engine commands. Nothing calls
  across directly, in either direction.
- Conservation and claim coverage become assertable now that custody exists. **They live in
  the harness**, the only structure that can see both systems — they are not, and cannot be,
  core assertions. Run them after every command in every test and scenario.

**Gate** — five scenarios, plus a separation check: no path in `core` can read a custody
balance, and the harness is the only structure holding both.
(a) all funds present → settles;
(b) maker withdrew before accept → pre-check rejects, nothing committed;
(c) **withdrawal lands between pre-check and settle** via `on_settle_entry` → settle
reverts, whole basket aborts, conservation holds, zero capital committed.
(c2) **custody clock ahead of the engine clock**: a quote the engine believes live is
expired at the custody layer → settle reverts, basket aborts safely. Proves venue time and
chain time are not assumed equal (SPEC §9.1).
(d) the timelock, asserted as three concrete properties: funds are excluded from settlement
availability immediately on `RequestWithdrawal`; execution occurs at exactly
`T + WITHDRAWAL_DELAY` **regardless of quote state**; and a quote admitted at `T − ε` with
`MAX_QUOTE_TTL` lifetime has settled or expired strictly before execution. The third is the
property the four-term inequality exists to guarantee.
(e) a config violating the four-term inequality **fails to start** — with a non-zero lag term
injected, since with all terms zero the inequality cannot be violated and the test would
prove nothing.

---

## S4 — Asynchronous settlement · 45m

- `submit` → `SubmitAck`; `status(nonce) -> Unknown | Pending | Settled | Reverted`
- Chain mock holds a pending queue the test advances explicitly
- `Settling{nonce, deadline}` request state; reservations held throughout
- `PollSettlement` command
- Timeout policy: never release on `Unknown`; escalate, do not abort

**Gate** — four scenarios:
(a) Pending → Settled → `Escrowed`;
(b) Pending → Reverted → `SettlementFailed`, all reservations released, conservation holds;
(c) `Unknown` for N polls then Settled — reservations held throughout, no double-commit;
(d) deadline reached with nonce unconsumed → stays `Settling`, alert event emitted,
reservations **not** released.
(e) **submit, lose the ack, resubmit the same nonce** → applied exactly once, conservation
holds. This is the path §8 exists to defend and the one the other four do not cover.
(f) invariant 3 (SPEC §15) holds *throughout* `Settling`, not only at the endpoints —
`committed` capital references a `Consumed` quote legally (SPEC §2.5).

> **If this stage overruns 60m, cut it.** (On the measured pace this is unlikely.) Because settlement already sits outside the
> commit phase (SPEC §7.2), cutting S4 breaks nothing structural: `SubmitIntent` is handled
> synchronously instead, and the three-way failure analysis moves to the failure-mode notes
> as designed-not-built. The write-up carries nearly the full signal.

---

## S5 — Resolution boundary · 30m

The engine models **no** proposal/dispute/bond lifecycle (SPEC §10.1). That machinery is
the oracle's and lives in the mock, in `chain`.

- `ContractState { Unresolved, Resolved(Outcome) }` in core — two states, nothing more
- `ReportOracleStatus { Silent | InProgress | Final(Outcome) }`
- `outcome()` derived predicate, stall exit conditioned on `Silent`
- `SettleEscrow` — O(1), idempotent, `Yes` / `No` / `Void` payouts
- Mocked optimistic oracle in `chain`: propose → window → final, with contestation

**Gate** — five scenarios:
(a) `Final(Yes)` → settle → winner paid;
(b) settle while `InProgress` → `NotYet`, nothing moves;
(c) contested → escalation authority reports `Final` → settle;
(d0) payout maps through leg side: `Final(Yes)` pays the requester on a `Yes` leg and the
maker on a `No` leg, asserted on one escrow of each in the same settlement.
(d) `Silent` past the grace period → `Void`, **each side's own contribution returned**,
    asserted by amount, explicitly not 50/50;
(e) **`InProgress` held indefinitely → still `NotYet`**: the stall exit is unreachable, so
    contesting cannot buy a free unwind. Escrows stay locked — named in the write-up as an
    oracle-liveness risk, not an engine defect.
(f) **monotonicity**: `InProgress → Silent` is rejected with `OracleStatusRegression`, and
the stall exit stays unreachable. Without this the contest-then-retract path re-opens the
free unwind that (e) exists to close.
(g) **`Final` is immutable**: `Final(Yes)` then `Final(No)` is rejected. Assert the failure
mode it prevents by constructing it in a test double — settle one escrow, overwrite the
outcome, settle a second, and show the contract would otherwise pay both ways with
conservation still holding. That is a correctness failure no invariant in §15 can see, which
is why monotonicity is a rule and not hygiene.
Plus: double `SettleEscrow` is a no-op.

---

## S6 — Indexer and chain boundary · 30m

- `ChainEvent { block, tx_hash, log_index, payload }`, append-only log
- Indexer: cursor, confirmation depth, dedup on `(tx_hash, log_index)`

**Gate** — four scenarios:
(a) replay from cursor 0 → balances unchanged, conservation holds;
(b) reorg below confirmation depth with a different outcome → engine never saw the first;
(c) `EscrowSettled` delivered before the `OracleStatusReported{Final}` it depends on →
    rejected, safety does not depend on delivery order;
(d) indexer lag → engine view stale but never wrong; no phantom credit. **Lag must be
injectable and set non-zero for this test** — with lag hard-wired to zero the test passes
without exercising the property, which proves only that it is untested.

---

## S7 — Scenario runner and packaging · 50m · **this is deliverable 2 — never cut**

- `scenarios` binary: named scenarios printing a readable command/event/balance trace
- Every scenario runs the **full chain end-to-end**: matching → acceptance → escrow →
  resolution → payout. A scenario that stops at escrow does not satisfy the deliverable.
- Required set — one happy path and five failure/recovery paths.

  **The happy path must exercise the design, not merely traverse it.** One maker per leg
  means selection never selects and demonstrates nothing. It follows SPEC Appendix A
  exactly, and the trace must show all of:

  1. **happy** — the September/October calendar spread, `Yes` leg and `No` leg, four makers:
     - **competition**: three quotes on leg A, the best one wins
     - **arrival-order tiebreak**: two quotes at the same price, earlier arrival wins
     - **over-limit admission**: a quote above the leg limit is admitted and reserves
       capital, is never published in `BestSelectionChanged`, and is never eligible — no
       price-based rejection exists anywhere
     - **expiry mid-auction**: the best quote on leg B expires before acceptance and is not
       selected, with no event published when it dies
     - **replacement that changes the winner**: a maker requotes lower and takes the leg,
       holding one slab slot throughout however many times they requote
     - **losing release**: every non-winning quote released with `QuoteRejected{Outbid}`
     - **over-reservation release**: requester reserved at limit, filled better, difference
       returned
     - **both payout directions**: both contracts resolve `Yes`, so the requester wins the
       `Yes` leg and the maker wins the `No` leg — the full payout mapping in one settlement
     - conservation and claim coverage asserted at every step

  2. **leg 2 of 3 fails** — request aborts whole, no capital committed, the other two legs'
     quotes still standing and their makers never notified
  3. **settle reverts mid-flight** — withdrawal lands between pre-check and settle
  4. **lost acknowledgement** — submit, lose the ack, resubmit the same nonce **after** the
     original was already included: applied exactly once, and the retry's revert on the
     consumed nonce is **not** read as settlement failure (SPEC §8.1)
  5. **contested resolution** — oracle reports `InProgress`, settle refused, escalation
     authority reports `Final`, settle
  6. **stalled → void** — oracle stays `Silent`, grace elapses, each side's own contribution
     returned (asserted by amount, not by outcome label — a 50/50 split also "resolves to
     Void" and moves money between the parties)
- Amounts print as grouped **minor units**, never decimals — a decimal point requires a
  division, and there is no division in the money path (CLAUDE 15)
- Accounts funded to **exactly** their contributions, so a misplaced claim overdraws rather
  than being absorbed (CLAUDE 38)
- README: build, run, one paragraph on who owns what (matching / custody / reservation), and
  a note that the container mounts the working tree so a run cannot report a stale pass
- README carries the structure-selection note (why an ordered per-account chain rather than
  a timer wheel) and the scope-cut summary pointing at SPEC §16

**Gate** — `docker compose run scenarios` prints all six traces on a clean machine with no
host toolchain. The happy path's figures match SPEC Appendix A exactly; a mismatch is a bug
in one of them and must be reconciled, not adjusted. Conservation **and claim coverage** asserted at every step of every scenario,
not only at the end.

---

## S8 — Write-ups · ~60m · written by hand, not generated

1. **State machine** — request, quote, escrow, contract; who triggers each edge; the
   leg-2-of-3 trace
2. **Failure-mode notes** (1p max) — every race and partial failure, each naming the test
   that proves it; anything cut is listed as designed-not-implemented with the specific
   capital-leak path and how the design prevents it
3. **Resolution design** — unlock mechanics; disputed / delayed / ambiguous; why `Void`
   refunds contributions rather than splitting; the stall-exit asymmetry
4. **Design note** (½p) — seconds vs days; what is invariant, what is not, what was built
   so either is cheap; the custody-timelock framing

---

## Requirements traceability

Checked before the first commit and again before submission. Every row must point at a
gate or a write-up section.

| Requirement | Where satisfied |
|---|---|
| Request: contract description, side, notional size, response deadline | S2 · SPEC §5, §5.3, §2.1 |
| Quotes: price + size, each with own expiry | S2 · SPEC §6 |
| Best quote selected and **presented**; accept or reject | S2 gate (d)(e) · SPEC §7.1.1 |
| On acceptance both sides lock into escrow | S2, S3 · SPEC §9 |
| Escrow pays out to the winning side on resolution | S5 · SPEC §10.3 |
| Multi-leg atomic: all legs or reject | S2 gate (b) · SPEC §7.2 |
| Acceptance rejects competing quotes on the leg and releases their capital | S2 gate (a) — `QuoteRejected{Outbid}` |
| Chain, payments, oracle all mocked | S3 (custody), S5 (oracle), S6 (chain boundary) |
| **D1** state machine, every state, every transition, who triggers each | S8.1 · SPEC §5 table, §6, §9.2, §10.2 (contract), chain mock docs (oracle) |
| **D1** leg 2 of 3 fails after leg 1 provisionally matched | S2 gate (b) · SPEC §7.2 |
| **D2** runnable, one happy path + ≥2 failure paths end-to-end | S7 — six scenarios, all through payout; the happy path exercises competition, tiebreak, over-limit admission, expiry, replacement and both payout directions (SPEC Appendix A) |
| **D3** failure-mode notes, 1 page, diffed against code | S8.2 · CLAUDE 24, 29 |
| **D4** resolution: unlock, disputed, delayed, ambiguous | S5 gates (a)–(e) · S8.3 · SPEC §10, §5.3 |
| **D5** design note: seconds vs days | S8.4 · SPEC §14 |
| Money-state correctness: lost / duplicated / stuck | SPEC §2.2, §15 · every gate asserts conservation |
| Adversarial robustness | SPEC §11 — **every row names its test**; unimplemented rows are marked accepted-risk in the failure-mode notes. Anchor gates: S2 (f), S3 (c)(c2), S4 (e), S5 (e) |
| Judgment under open requirements — thin interfaces | SPEC §8.2, §12, §7.1 traits |
| Scope discipline | Cut order below |
| Out of scope: UI, auth, KYC, real chain, pricing/risk | SPEC §16 |

---

## Cut order

Cut ungraded work before graded work. No deliverable row maps to S6, and S7 *is*
deliverable 2 — the earlier ordering had that backwards.

On the measured pace this order is unlikely to be needed. It stands as the
contingency, not the plan.

1. **S6 indexer** — no deliverable requires it. Keep the event log, drop confirmation
   depth, dedup and the reorg tests; describe them in the design note.
2. **S4 async settlement** — handle `SubmitIntent` synchronously; move the three-way
   failure analysis to the failure-mode notes.
3. **S5 mocked oracle internals** — keep `OracleStatus` and the stall exit; drop the
   propose/window/contest machinery in the mock and report status directly.

Never cut: S1 invariants, S1.5's single-writer proof, S2's plan-check-commit atomicity and
its reachability gate, S3 scenarios (c) and (c2), the conservation assertion, **S7**, or
S8. Those are the submission.

---

## Time record

~2h design and specification before the first commit. The commit history covers
implementation only. Recorded here rather than implied by the log.
