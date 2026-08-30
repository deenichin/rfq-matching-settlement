# RFQ Matching & Settlement Engine — Specification

Permissionless request-for-quote venue for binary-outcome contracts. Deterministic
single-writer core; mocked custody and resolution behind explicit seams.

---

## 1. Trust model

The venue is permissionless: there is no identity, no reputation, no credit, and no
out-of-band recourse. Every participant is assumed adversarial.

The only enforcement primitive available is **custody of funds already under control**.
Every rule in this document is enforceable by refusing to release money. Nothing depends
on a participant behaving well later.

Three distinct authorities:

| Concern | Authority |
|---|---|
| Matching — who quoted, which quote wins, whether an accept is admissible | Engine |
| Custody — balances, escrow, payout | Chain (mocked) |
| Reservation — capital committed to a live quote | Engine, as a *soft claim* against custody |

Reservation is soft: the chain has no concept of it. It is only safe because the custody
layer enforces a withdrawal timelock longer than the maximum quote lifetime (§9.3).
This is the load-bearing invariant of the whole design.

---

## 2. Money model

### 2.1 Binary contracts

A contract pays `UNIT` per contract to the winning side and zero to the loser.
`UNIT = 1_000_000` minor units (6dp, USDC-shaped).

**Every leg carries a side.** The requester states, per leg, whether they are buying `Yes`
or `No`; the maker takes the opposite side of that leg. Without this a requester could only
ever be long `Yes` on every leg, which reduces multi-leg to a parlay and cannot express a
spread — and the spread is the motivating multi-leg product ("cuts in September but not in
October").

`price ∈ [0, UNIT]` is expressed in **minor units per contract** and is always the price of
**the side the requester is buying**. Buying `No` at `q` is economically identical to selling
`Yes` at `UNIT − q`, so one formula covers both:

```
requester_contribution = size × price
maker_contribution     = size × (UNIT − price)
escrowed_notional      = size × UNIT
```

The escrow record stores the requester's side for the leg; that is what the payout mapping
consumes (§10.3). Selection is unaffected — the price is already the price of the side being
bought, so lowest-is-best holds on both sides.

The two contributions sum to the notional **exactly**, by construction. There is no
division anywhere in the money path, therefore no rounding rule, no dust, and no
"who eats the remainder" question. This is deliberate: the representation makes the
rounding question unaskable rather than answering it.

Max loss is bounded and known at formation, so positions are fully prefunded. There is
no margin, no leverage, no liquidation, and — after escrow forms — no counterparty risk.

### 2.2 Buckets and conservation

Every minor unit is in exactly one bucket:

```
free ──reserve──> reserved ──commit──> escrowed ──settle──> free (payout)
  ^                   │                     │
  └──────release──────┘                     └──void──> free (contributions returned)
```

Two separate invariants, because `reserved` and `committed` are **claims against**
custody's free balance, not partitions of it. Custody has no knowledge of either (§1), so
`custody.free(a)` still contains every unit a core-side claim refers to. Summing them
together would double-count.

**Conservation — entirely custody-side, and complete on its own:**

```
Σ_accounts custody.free(a) + Σ_{escrows in Locked} notional
    == Σ_accounts deposited − Σ withdrawn
```

**Only `Locked` escrows hold money.** A `Settled` escrow has already paid out; its notional
is back in someone's free balance. Summing every escrow regardless of state makes the first
payout read as newly created money and the invariant fails on a correct system — which is
worse than not having it, because the natural repair is to weaken the assertion.

Money only ever moves between custody's free balances and custody's escrows. Reserving and
committing move nothing; they record that the core has promised a unit will be available.

**Claim coverage — every core-side claim is backed:**

```
∀ a:  custody.free(a)  ≥  core.reserved(a) + core.committed(a)
```

A violation means the core has promised capital custody does not hold — which is the
failure the §9.3 withdrawal timelock exists to prevent, and the reason a maker cannot
quote-and-run.

The core's balance mirror (§2.3) is an input to neither. It is checked separately by the
mirror-agreement assertion below, so drift is detected rather than hidden inside a balanced
sum.

*lost* is the sum falling, *duplicated* is it rising, *stuck* is a unit in `reserved` or
`escrowed` with no reachable transition out.

**Where it is asserted.** Balances and escrows live in the custody layer (§2.3); the core
owns only reservations. No function in the core can therefore compute the global sum, and
none is required to. The split:

| Assertion | Scope | Asserted by |
|---|---|---|
| `account.reserved == Σ` amounts on that account's reservation chain (§15.1) | core | every core function that moves a reservation, `debug_assert` |
| claim coverage: `custody.free(a) ≥ reserved(a) + committed(a)` | both | the harness, after every command |
| escrow contributions sum to notional | custody | every custody mutation |
| mirror agreement: `mirror.free(a) == custody.free(a)` for all `a` | both | the harness, after every command — exact in v1, bounded by lag in v2 |
| global conservation (above) | both halves | the harness, after every command, in tests and scenarios |

The global assertion needs visibility of both layers, which exists only where they are
wired together. Pushing it into the core would require the core to depend on custody and
would collapse the separation the design rests on.

### 2.3 Where balances live, and what the core sees

Custody is authoritative for balances and escrows. **The core holds a read-only
projection of balances, used for admission only.** This mirror is named here because it
is the source of several otherwise-invisible failure modes.

- v1: the mirror is exact. Custody is in-process, confirmation depth is zero, there is no
  indexer lag, and mirror updates are applied in the same command stream.
- v2: custody is remote. The mirror lags by confirmation depth plus indexer lag, and the
  design must tolerate admitting against a stale view. See §9.3 and §12.

The mirror is never authoritative. Admission uses it; settlement revalidates against
custody and reverts wholesale on disagreement (§9.1). A stale mirror can therefore cause
a *failed settlement* — a liveness cost — but never a money-state error.

Reservation is a core-side soft claim against a mirrored balance. It is safe only because
custody enforces a withdrawal delay long enough to cover the mirror's staleness (§9.3).

### 2.4 Committed — the in-flight bucket

Because settlement is asynchronous (§8), a fifth bucket sits between `reserved` and
`escrowed`. This diagram supersedes the simplified one in §2.2:

```
free ──reserve──> reserved ──accept──> committed ──settled──> escrowed ──> free (payout)
  ^                   │                    │                      │
  └──────release──────┘                    └──reverted──> free     └──void──> free
```

**`committed` is all capital attached to a request in `Settling`** — both sides of the
trade:

| Side | Amount | Attached via |
|---|---|---|
| each winning maker | `leg.size × (UNIT − fill_price)` | the maker's `Consumed` quote |
| the requester | `Σ_legs leg.size × fill_price` | the request itself |

Defining it as "capital attached to a `Consumed` quote" would cover only the maker half,
since quotes are maker-side by construction. The requester's contribution has no quote and
must be attached to the request.

Entry is performed in the accept commit phase (§7.2), explicitly, for both sides. It is not
implied by any other transition.

`committed` capital is held by the core, is not yet in custody's escrow, and **may not be
released on a guess** (§8.3). Its only exits are settlement confirming (→ `escrowed`) or
settlement definitively failing (→ `free`).

Naming it separately is what makes the money model valid under asynchrony: without it,
`Settling` capital would have to be counted as either reserved (contradicting invariant 3,
since the maker's quote is `Consumed`) or escrowed (untrue until custody confirms).

### 2.5 Arithmetic

`Amount(u64)` minor units. All products computed in `u128` and checked on narrowing.
`checked_*` everywhere in the money path; overflow is a rejection, never a wrap.
No floats anywhere in the repository.

---

## 3. Identity and data layout

External identifiers — account keys, request ids, and **contract description hashes** —
are opaque byte strings and exist **only at the gateway boundary**. The core is addressed
exclusively by dense `u32` indices assigned on first sight. No hashing, no string
comparison, no `HashMap` on any path reachable from `apply`.

Core storage:

| Structure | Layout |
|---|---|
| Balance mirror | `Vec<MirroredBalance>`, index = `AccountIdx` — projection, not authority (§2.3) |
| Requests | slab, `Vec<Request>` + free list, index = `ReqIdx` |
| Quotes | slab, index = `QuoteIdx`; intrusive singly-linked per `(request, leg)` |
| Reservations | preallocated slab, generation-counted `ResIdx`; **per-account expiry-ordered intrusive doubly-linked chain** threaded through the slab (§4.3) |
| Contracts | slab, index = `ContractIdx`; description bytes held at the gateway |

`MAX_LEGS` and `MAX_QUOTES_PER_LEG` are **compile-time** storage bounds: leg arrays are
fixed-size and cannot take a runtime value. The corresponding config values are checked
`config.max_legs <= MAX_LEGS` at startup, with their own error variants. Config bounds
policy; the constant bounds storage.
| Escrows | owned by custody, not by the core; the core holds `EscrowId` only |

Slabs are preallocated to configured capacity and never grow during `apply`. Exhaustion
is a rejection with a distinct error, not a reallocation.

---

## 4. Time

### 4.0 Units: instants and durations

`Ts` is an **instant** — milliseconds since an arbitrary epoch, `u64`. `Dur` is a
**duration** in the same unit, and is a separate type:

```
Ts  + Dur -> Ts        expires_at = now + ttl
Ts  - Ts  -> Dur       time remaining
Dur + Dur -> Dur       the four-term timelock sum (§9.3)
Ts  + Ts  -> does not exist
```

The separation is not decoration. The timelock inequality sums four durations and compares
the result to a duration; expiry compares two instants; `MIN_HORIZON` guards a gap between
instants. Collapsing both onto one type makes `Ts + Ts` compile while meaning nothing, which
is the same class of error `Amount` exists to prevent on the money side. Every configured
bound — `MAX_QUOTE_TTL`, `MAX_REQUEST_TTL`, `MIN_HORIZON`, `MAX_SETTLING_TIME`,
`STALL_GRACE`, `WITHDRAWAL_DELAY` and its lag terms — is a `Dur`. Every expiry, deadline and
event date is a `Ts`.

`Ts` is `u64` milliseconds. Quote lifetimes are seconds and
escrow lifetimes are months; a month is ~2.6e9 ms, so the range is not a concern and the
resolution is finer than any decision the system makes. Coarser would lose quote-expiry
resolution; finer buys nothing when the maker round trip dominates end-to-end latency.
All durations in this document are milliseconds unless stated otherwise.

### 4.1 Clock authority

A single monotonic clock, owned by the venue, injected as a trait. `Instant::now()`
never appears outside the clock implementation. Participant-supplied timestamps are
advisory and never trusted for any decision.

`apply(cmd, now)` samples `now` **once** per command. Every predicate evaluated during
that command uses that one value, so legs of a multi-leg accept cannot disagree about
what time it is.

### 4.2 Expiry is a predicate, not an event

Liveness is derived at the point of use:

```
live(quote) = quote.state == Active && now < quote.expires_at
```

Half-open by convention: a quote expiring at exactly `now` is dead. Documented so the
boundary can never double-count.

**Correctness never depends on a sweep having run.** If expiry were event-driven, whether
an accept succeeded would depend on scheduler timing, and an accept arriving in the same
tick as an expiry would be a genuine race. As a predicate, that race cannot exist.

### 4.3 Reclamation of reserved capital

Reserved capital must return to `free` when a quote expires unaccepted, or the ledger's
`reserved` total is stale and admission decisions are wrong.

**Structure.** Each account owns an expiry-ordered intrusive doubly-linked chain threaded
through the reservation slab:

```rust
enum ResOwner { Quote(QuoteIdx), Request(ReqIdx) }   // requester-side claims have no quote

struct Reservation {
    account: AccountIdx,
    amount: Amount,
    owner: ResOwner,       // needed for invariant 3 and for releasing by owner
    expires_at: Ts,
    prev: ResIdx,          // chain links: expiry-ordered while reserved,
    next: ResIdx,          //              per-request while committed
    generation: u32,
}
```

**Two chains, never both.** A reservation is linked into exactly one:

| While | Linked into | Ordered by |
|---|---|---|
| `reserved` | the account's expiry chain | `expires_at` |
| `committed` | the request's committed list | insertion |

The commit phase (§7.2) unlinks from the first and links into the second in one step.
`committed` entries have no expiry and are therefore unreachable by `release_expired`,
which is what makes §2.4's "may not be released on a guess" structurally true rather than
merely stated. `account.committed` is a second stored total alongside `account.reserved`.

**Release-on-access is the only mechanism, and it is the correctness path.** Any command
touching account A first runs `release_expired(A, now)`, which walks A's chain from the head
and stops at the first entry with `now < expires_at`. Reclamation therefore always precedes
any decision that depends on it, and nothing is deferred to a sweep.

**Normalisation is a distinct phase, before the command is evaluated:**

```
apply(cmd, now):
    NORMALISE   release_expired for every account the command touches.
                Mutates state. Depends only on (accounts, now) — never on the
                command's content, and never on whether it will be accepted.
    PLAN / CHECK / COMMIT   (§7.2)
```

This is why the no-mutation-on-rejection guarantee (§15.4) is stated **relative to the
post-normalisation state**. Normalisation is time catching up, not the command acting: the
same reclamation happens whether the command is accepted, rejected, or replaced by any
other command touching the same accounts at the same instant. An absolute
byte-identical-to-pre-command claim would be false for any account holding an expired
reservation, and weakening the invariant later to accommodate that is exactly the trap.

**`QuoteExpired` is emitted only from the accept commit phase**, where the count is bounded
by `MAX_LEGS × MAX_QUOTES_PER_LEG`. Expiry outside that path is silent.

**Normalisation emits no events.** The number of reservations it reclaims is bounded only by
how many expired, so emitting per-reservation would put an unbounded write into the event
buffer before the CHECK phase can verify headroom (rule 9). Expiry is a derived fact —
a maker's own liveness predicate tells them their quote is dead — so no notification is
owed. Only the commit phase emits, and its event count is statically bounded by
`MAX_LEGS × MAX_QUOTES_PER_LEG`.

| Operation | Cost |
|---|---|
| insert | short walk back from the tail; near-O(1) since later quotes usually expire later, worst case O(k) over one account's chain |
| early release (quote wins or loses at accept) | O(1) unlink |
| `release_expired(A, now)` | O(number actually reclaimed) |

`release(handle)` names a specific claim and therefore validates it: a handle denoting
committed capital is refused with `ReservationCommitted`, distinct from `StaleReservation`
for a handle whose generation no longer matches. They are different bugs in the caller and
must not share a variant. Note the asymmetry with `release_expired`, which needs **no** such
guard — committed claims are not on the expiry chain, so the bulk traversal cannot reach
them. A guard there would be a filter that can be mis-scoped; its absence is the proof.

**No allocation anywhere.** The slab is preallocated to configured capacity; the chains are
intrusive index links. Slab exhaustion is a rejection (`SlabExhausted`), never a
reallocation. `k` is bounded in practice because every live reservation locks real capital,
so an account cannot hold more open quotes than its balance supports.

There is no global expiry queue and no background sweeper. An untouched account holds its
expired reservations in the slab until something touches it; that costs slab occupancy,
bounded by capacity, and never affects a decision because no decision about A is taken
without first normalising A.

`MAX_QUOTE_TTL` is a **market-structure input** (§14). Nothing in this structure constrains
it in either direction.

> **Why not a timer wheel.** An intrusive single-level timer wheel gives O(1) reclamation
> and is the right structure for an order book, where lifetimes are unbounded and live
> entries number in the millions. Here it was bounding the maximum lifetime of a firm quote
> by its slot count — letting a memory layout make a market-structure decision. The
> per-account chain keeps the intrusive, allocation-free property that mattered and drops
> the slot span that did not. See the README note on structure selection.

---

## 5. Request lifecycle

```
                 SubmitRequest
                      │
                      ▼
                   ┌──────┐  Reject (requester)          ┌──────────┐
                   │ Open │ ───────────────────────────► │ Rejected │
                   └──────┘                              └──────────┘
                      │
                      │  now >= deadline  (derived)      ┌─────────┐
                      ├────────────────────────────────► │ Expired │
                      │                                  └─────────┘
                      │  Accept
                      ▼
              ┌────────────────┐
              │ Settling{nonce}│  ── submission outcome unknown ──┐
              └────────────────┘                                  │
                      │                                           │ poll
        Settled       │        Reverted / deadline+unconsumed     │
      ┌───────────────┴───────────────┐                           │
      ▼                               ▼                           │
┌──────────┐                  ┌──────────────────┐                │
│ Escrowed │                  │ SettlementFailed │ ◄──────────────┘
└──────────┘                  └──────────────────┘
```

Terminal: `Escrowed`, `Rejected`, `Expired`, `SettlementFailed`.

`Expired` is derived, not stored — the request is expired iff `now >= deadline` and no
accept has been made. The requester's reservation is reclaimed by release-on-access
on the next command touching that account (§4.3).

**Who triggers what**

| Transition | Trigger | Authorisation |
|---|---|---|
| → Open | `SubmitRequest` | any account with sufficient free balance |
| Open → Rejected | `RejectRequest` | requester only |
| Open → Expired | time | nobody — derived |
| Open → Settling | `AcceptRequest` | requester only |
| Settling → Escrowed / SettlementFailed | `PollSettlement` | anyone (indexer in practice) |

### 5.1 Acceptance window

The requester may accept from the moment the first quote arrives until the request
deadline. **There is no separate post-deadline acceptance window.**

Rationale: a firm quote is an option the maker has written and given away for free.
Any window extending past the deadline is additional free optionality at maker expense.
The effective binding window is already `min(expiry)` over the selected quotes, which is
maker-controlled — as it should be.

*Reversible.* The sealed-auction variant (no acceptance until the deadline, then a fixed
window) changes one predicate and no state. Noted in the design note.

### 5.2 Requester reservation

Reserved at `SubmitRequest`, before any price is known.

`SubmitRequest` is rejected if `deadline − now > MAX_REQUEST_TTL` (`DeadlineTooFar`), or if
any leg's contract has `now >= event_date − MIN_HORIZON` (`ContractTooNear`).

`MIN_HORIZON > MAX_REQUEST_TTL + MAX_SETTLING_TIME` is a startup assertion. Together the two
bounds guarantee that a request accepted at its last legal instant still forms escrow
strictly before `event_date`, so escrow can never be created on a contract whose stall grace
has already elapsed. Without this a request can be opened, quoted, accepted and settled on a
contract whose stall grace has already elapsed, so both sides' capital enters escrow on a
trade that is immediately `Void`-resolvable — a free capital round-trip against makers.

Each leg of the request carries a **contract reference, a side (`Yes`/`No`), a size, and a
limit price**.

On admission the request emits `RequestOpened { request, legs: [(contract_description, side,
size)], deadline }`, which the publisher fans out to makers. This is the step that makes the
venue an RFQ rather than a private negotiation: without it no maker learns a request exists.
**Limit prices are not included** (§5.2 below) — makers receive the terms they need to price,
and nothing more. Reservation is `Σ_legs size × limit_price`.
Without a limit price the only safe reservation is worst case — full notional per leg —
which is capital-brutal for no benefit. The residual over-reservation is released at
commit.

The limit price does two jobs: it bounds the requester's reservation, and it protects the
requester from a fill at a price they never agreed to. It is enforced **at selection**
(§7.1), never at admission.

**The limit price is never broadcast.** Makers see contract, size, and deadline only. A
revealed reserve price shades quotes toward the limit rather than toward the maker's true
best price.

Enforcing at selection rather than admission is what keeps the limit private. A rejection at
admission would be a free oracle: a maker bisects downward from `UNIT`, and every rejection
— the message that carries the information — costs nothing, because a rejected quote
reserves nothing. Three or four free rejections bracket the limit closely enough to shade
against it. Under selection-time enforcement an over-limit quote is admitted, reserves
capital normally, and simply never wins; the maker learns nothing and pays for the attempt.

> **Judgment call under open requirements.** A limit price is not part of the minimal
> request description. It is added because worst-case reservation is the only alternative.
> Flagged explicitly rather than quietly assumed.

> **Why reserve the requester at all.** Prefunding the taker is an artifact of venue
> custody, not of RFQ: traditional RFQ settles on credit, and non-custodial crypto RFQ has
> the taker sign an exact price at acceptance with no reservation phase. Deferring the
> funding check to acceptance is capital-perfect and fails safe — a failed accept commits
> nothing. It is rejected for two reasons: requesting becomes free, so a zero-capital
> Sybil requester can lock unbounded maker capital and there is no identity to rate-limit
> against; and requester solvency becomes a fallible check inside the commit phase,
> forfeiting the infallible-commit property of §7.2. A partial bond buys the first
> property and not the second.

### 5.3 Contract identity and authorship

A request names the contract it wants to trade. In a permissionless venue there is no
registry and no admin, so **the requester authors the contract description**, and the
venue cannot interpret it.

```rust
// Gateway type. Never crosses into the core.
struct ContractRef {
    description: Bytes,    // the full wording — this IS the identity
    event_date: Ts,
    resolution_source: Bytes,
}

// Core type.
struct Contract { idx: ContractIdx, event_date: Ts, /* resolution state, §10.2 */ }
```

- **Identity is byte equality over the full description**, not a hash of it. Two requests
  trade the same contract iff their descriptions are byte-identical. There is no fuzzy
  matching and no canonicalisation — near-identical wording produces different contracts,
  which is correct: the wording *is* the product.
- No hash is used. A hash would only compress the identity, and compression is not required
  anywhere in this design — nothing puts a contract id on a wire or on a chain in v1. A
  non-cryptographic hash would be strictly worse than byte equality here, because a
  collision would let a trade formed on one contract resolve under another's outcome, and
  contract identity is an adversarial surface (§11). A cryptographic hash would buy nothing
  over byte equality while adding a dependency. If v2 needs a compact on-wire id, the hash
  is then chosen for the chain's requirements, not ours.
- **Matching happens at the gateway**, which owns `description_bytes → ContractIdx`. The
  core is addressed by `ContractIdx` only and never hashes, never compares bytes, and never
  sees the description. This keeps §3 and rule 11 intact; the map lives at the boundary
  where allocation is permitted (rules 3, 9).
- The description is stored at the gateway, republished verbatim to makers, and handed to
  the resolution layer. The engine never parses it.
- First reference allocates the `ContractIdx`; subsequent references resolve to it.

A leg names a contract *and* a side. A maker quoting leg `(contract C, side Yes)` is taking
the `No` side of C; the same contract quoted on two legs with opposing sides is two distinct
exposures, and the venue treats them independently.

**Makers quote exact wording.** The description is broadcast verbatim with the request, and
a maker quoting a contract is asserting they have read and priced that exact byte sequence. Ambiguity
risk sits with the maker, priced into their quote — which is consistent with pricing being
out of scope for the venue.

> **Adversarial consequence.** A requester can author deliberately ambiguous wording,
> creating a contract that is likely to resolve `Void`. Void refunds contributions, so
> ambiguity is a free unwind option for whichever side is losing — and the requester chose
> the wording. See §11.

The venue's structural defence is that **no participant can unilaterally reach `Void`**:
it is reachable only through the escalation authority's ruling, or through the stall exit,
which requires `OracleStatus::Silent` and is closed by monotonicity against
contest-then-retract (§10.1, §10.4). A losing requester can trigger contestation through the
oracle, but contestation cannot produce a void by itself. What remains is a real residual risk that is
priced by makers, not eliminated by the venue — stated rather than hidden.

---

## 6. Quote lifecycle

Stored state: `Active | Consumed | Released`. Liveness is `Active && now < expires_at` (§4.2).

`SubmitQuote` is rejected unless **all** hold:

- request exists and is `Open`
- `now < request.deadline`
- `now < expires_at` and `expires_at − now <= MAX_QUOTE_TTL`
- `size >= leg.size` (§7.1)
- `price` is in `[0, UNIT]`

**Quotes are never rejected for being outside the leg's limit price.** A quote above the
limit is admitted, reserves capital normally, and simply loses at selection (§7.1).

**One live quote per maker per leg, and replacement may only improve.** A maker's new quote
on a leg replaces their previous one — the old quote goes `Active → Released` and its
reservation is freed in the same command — **but only if the new price is at least as good
for the requester as the old one.** A worse replacement is rejected with `WorseReplacement`
and the existing quote stands.

Without that condition, replacement is a cancel primitive in disguise: requote at
`price == UNIT`, which reserves zero, and the maker has withdrawn liquidity they promised
was irrevocable. The improve-only rule preserves irrevocability exactly — a maker can never
reduce what they have committed, only sharpen it — while still bounding slab occupancy per
request at `makers × legs` rather than by message count. It also matches how makers behave:
they update a price, they do not stack quotes.

`MAX_QUOTES_PER_LEG` caps the per-leg quote chain, making the commit phase's event count
statically bounded (§4.3).

The bound matters because admitting all well-formed quotes reopens a spam channel that
admission-rejection previously closed by accident: a maker's contribution is
`leg.size × (UNIT − price)`, which is **zero at `price == UNIT`**. Without the one-quote
rule a zero-capital actor could flood a leg with guaranteed-losing quotes at no cost until
`SlabExhausted` starts rejecting honest makers. Replacement makes flooding self-cancelling.

> Rejecting on price would make the rejection itself a free oracle for the hidden limit: a maker bisects downward from
> `UNIT`, and every rejection — the messages that carry the information — costs nothing,
> because a rejected quote reserves nothing. Three or four free rejections bracket the
> limit closely enough to shade a quote toward it, which is exactly the harm keeping the
> limit private is meant to prevent. Admitting all well-formed quotes removes the
> information channel at its source rather than trying to make probing expensive.
- maker's free balance covers `leg.size × (UNIT − price)`

Reservation is against the **fillable** amount, `leg.size`, not the quoted size. A quote
offering more than the leg needs is admissible, but only the leg's size can ever fill, so
reserving against the quoted size would over-lock maker capital for no purpose.

On admission the maker's contribution moves `free → reserved`, keyed by a reservation slot
linked to the quote. **A quote not backed by reserved capital is a promise, and promises
are worthless here.**

Quotes are **irrevocable until expiry** in v1. The maker's exposure control is the expiry
they chose. `CancelQuote` exists as a rejected transition, not as an absent one — see §11.

Capital reservation is simultaneously the anti-spam mechanism and the memory bound: a
maker cannot hold more live quotes than their balance supports.

---

## 7. Selection and acceptance

### 7.1 Selection

A pure function over live quotes evaluated **at accept time**. No stored decision: the
filling set is computed fresh from the live quote set every time it is needed, so it cannot
go stale. (§7.1.1 retains the last *published* selection to detect changes for the
presentation feed. That is a publication cache, not an input to any fill.)

- **Eligibility first: a quote is eligible only if its price is at or better than the leg's
  limit.** This is the sole enforcement point for the limit price (§5.2). An ineligible
  quote is never selected and never published, so `BestSelectionChanged` can never present
  a price the requester has not authorised.
- Rank the eligible set by price ascending — the price is always the price of the side the
  requester is buying (§2.1), so lowest-is-best holds for `Yes` and `No` legs alike and
  selection needs no side-specific branch — tiebreak by arrival sequence
- Earliest-arrival tiebreak is deterministic and denies a maker any gain from spamming
  identical quotes
- **Full size or nothing.** No aggregation across makers on one leg in v1
- Selection is per-leg and greedy

> Greedy per-leg selection is optimal because legs are priced independently — correlation
> and portfolio risk are out of scope, which is precisely the condition under which
> leg-wise optimal equals basket optimal. If package quoting were introduced this
> assumption breaks and selection becomes an assignment problem.

Aggregation sits behind a `SelectionStrategy` trait: it turns each leg into a miniature
order book and introduces partial-fill-within-a-leg as a new failure surface, so it is a
second implementation rather than a rewrite.

### 7.1.1 Presentation and accept binding

The current best selection is published to the requester as `BestSelectionChanged` on
**quote arrival** — the only point at which the engine emits. This is the "present the best
quote" step, and it is an event, not a stored decision: selection remains a pure function
(§7.1).

**The feed is eventually consistent, and deliberately so.** When the best quote expires and
nothing new arrives, no event is published: normalisation emits nothing (§4.3), so the
change surfaces on the next command touching that request. The requester's view can
therefore name a quote that is already dead.

That is safe rather than merely tolerated, and the reason is §7.1.1's accept binding: an
accept carries the prices the requester saw and fills at-or-better, so a stale view produces
`PresentationStale` and never a bad fill. Publishing expiry-driven changes would require
emitting from normalisation, whose event count is unbounded — a stale feed with a safe
accept is the better trade.

Because selection re-runs at accept time, the set that fills may differ from the set the
requester saw. `AcceptRequest` therefore carries the requester's view:

```rust
AcceptRequest { request, expected: [(LegId, Price); MAX_LEGS], n_legs: u8 }
```

Accept is admissible **at or better than** every expected price, per leg. Strictly better
fills (a superior quote arrived in flight) are accepted silently; any leg that would fill
worse than presented rejects the whole request with `PresentationStale{leg, expected, actual}`.

Without this binding the requester is exposed to anything up to their limit price, and the
limit is a disaster bound rather than a trading decision. At-or-better also removes the
incentive to race the presentation feed: a requester can never be worsened by latency,
only by a stale accept that fails safely.

*Note the interaction with §5.1:* the accept window is short and maker-controlled, so
`PresentationStale` is a normal outcome, not an error. The client's response is to re-read
the feed and re-accept.

### 7.2 Accept — plan, check, commit

```
1. PLAN    pure, no mutation. For each leg select the best live quote against the
           single sampled `now`. Any leg with no eligible quote aborts the whole
           request. Nothing has been touched.

2. CHECK   basket-level. Requester reservation covers Σ contributions. Slab capacity
           available (`SlabExhausted`). Event buffer headroom for the worst case, which is
           bounded because `n_legs <= MAX_LEGS` (`EventBufferFull`). Settlement pre-check
           against the balance mirror (fast reject only, §8.2).

3. COMMIT  infallible. No `?`, no fallible call, no allocation past this line.
             - mark winning quotes Consumed, losing quotes Released
             - release losing reservations
             - move each winning maker's contribution  reserved → committed
             - move the requester's Σ contributions    reserved → committed
             - release the requester's over-reservation (limit − fill) reserved → free
             - transition the request to Settling{nonce}
             - emit SubmitIntent{nonce, bundle}

Both `reserved → committed` moves are explicit steps here. Nothing else in the system
performs them, and the global invariant (§2.2) counts `committed`, so omitting either would
present as a conservation failure at the end of the accept path.
```

**Settlement is never called inside the commit phase.** Commit performs local, infallible
bookkeeping and emits an intent; the submission is a subsequent command produced by the
publisher. This holds in v1 and v2 alike — a fallible custody call inside commit would
violate rule 18 in v1 and be impossible in v2, where settlement is a network round trip.

Consequently escrow does not exist at the end of accept. It appears when settlement
confirms (§8, §9.1). The intermediate ownership of the winning makers' capital during
`Settling` is defined in §2.4.

**Multi-leg atomicity falls out of this and needs no distributed protocol.** Because
quotes are pre-reserved and firm until their own expiry, "provisionally matched" is a
local variable inside the plan phase — never a stored state, never a message to a
counterparty. The maker was bound from the moment they quoted. Aborting leg 1 when leg 2
fails costs nothing and notifies nobody.

*Leg 2 of 3 fails after leg 1 provisionally matched:* the plan loop returns
`NoEligibleQuote{leg: 2}` before any mutation occurs. Leg 1's selected quote is untouched
and still `Active`; its maker is never told it nearly traded. The request stays `Open`
until the deadline — a later accept may succeed if new quotes arrive.

Causes of a leg failing to fill, each with its own error variant: no quotes arrived; all
quotes expired; no quote covers the full leg size; every live quote is priced outside the
leg limit, so the eligible set is empty (`NoEligibleQuote{leg, reason: OutsideLimit}`).
The last is distinct from the first — makers responded and their quotes are live and
reserved; they are simply ineligible under §7.1. Nothing is refused at admission on price.

On a successful commit, every non-winning quote on a filled leg transitions
`Active → Released`, its reservation is released, and `QuoteRejected{quote, reason: Outbid}`
is emitted to its maker. A quote released by expiry emits `QuoteExpired` instead. Makers
are never left inferring the fate of their capital from silence.

The claim "the commit phase contains no fallible operations" is stronger than "wrapped in
a transaction", and is enforced by a test asserting no partial mutation on every
rejection path.

---

## 8. Settlement boundary

The seam where this design would meet a real chain, and the state where money is most
exposed.

### 8.1 Why submission has three outcomes

A synchronous call has two. A transaction submitted to a network you do not control has
three, because between submission and inclusion there is an interval in which **no local
answer exists**: the RPC request times out, the response is lost, the transaction sits in
the mempool underpriced, the including block is orphaned, or the process dies mid-send.

Every local decision during that interval is wrong:

| Guess | Consequence |
|---|---|
| Assume failure, release reservations | maker requotes the same capital, transaction lands → **duplicated** |
| Assume success, record escrow | transaction reverts → escrow that exists nowhere on chain → **invented** |
| Hold indefinitely | node never received it → capital reserved forever → **stuck** |

Resolution: **do not guess.** The bundle carries a nonce that is unique and deterministic
without hashing: `(ReqIdx, req_generation)`, taken from the **request** slab's generation
counter.

**Nonce status is monotonic and terminal.** `status(nonce)` reports the fate of the *nonce*
against final chain state — never the outcome of whichever submission most recently carried
it. Once a nonce reaches `Settled` or `Reverted`, that answer is immutable; a later
submission cannot move it back to `Pending` or `Unknown`.

This is load-bearing, and the failure it prevents is subtle enough to be worth stating.
Suppose the engine submits, the transaction *is included*, and the acknowledgement is lost.
The engine correctly retries — that is what an idempotent nonce is for. The chain refuses
the retry because the nonce is already consumed, and a naive implementation reports
`Reverted`. The engine concludes the settlement failed and releases the committed claims,
for a settlement that actually succeeded.

Note what makes it dangerous: every component told the truth. The retry genuinely did
revert. The damage is a permanent split between the layers — the engine shows the capital
free and will admit quotes against it, while custody holds it in escrow backing a live
position, and an escrow exists that no request points at. **Conservation cannot detect
this**, because each layer remains internally consistent; the sums balance on both sides of
a model that has come apart.

The trap is reading `Reverted` as a property of the submission rather than of the nonce. Two
reverts that look identical mean opposite things: reverted on insufficient funds means the
trade never happened; reverted on a consumed nonce means **the trade already happened**. A
retry bouncing off its own nonce is evidence the original succeeded.

This is oracle monotonicity (§10.1) one layer down — in both cases a status that can regress
lets a later, less-informed observation overwrite an earlier, better-informed one. Every slab in the core carries a generation counter (§3); a request slot reused
after its predecessor was freed yields a different generation, so nonces are never reused
even though indices are.
Content-derivation is deliberately avoided: it would require hashing inside `apply`, which
§3 forbids, and it serves a signature-binding purpose that does not exist in v1. The request sits in `Settling` with reservations held, and the outcome
is polled until definitively known. Idempotency turns "unknown" from a catastrophe into
a delay.

### 8.2 Interface

```rust
trait Settlement {
    fn submit(&mut self, bundle: Bundle) -> SubmitAck;   // "sent" — nothing more
    fn status(&self, nonce: Nonce) -> TxStatus;          // Unknown | Pending | Settled | Reverted
    fn precheck(&self, bundle: &Bundle) -> Result<(), SettleError>;  // gas saver only
}
```

`precheck` is an optimisation with **no correctness role**. Checking then submitting is
TOCTOU: the window between check and inclusion is exactly where a withdrawal lands. The
authoritative validation is inside the settlement transaction itself (§9.1). Stated
plainly because it is a standard probe.

### 8.3 Timeout policy

On `settling_deadline`, reservations are **not** released until the nonce is confirmed
unconsumed. Releasing on a guess is the duplication path. Timeout escalates to continued
polling plus an operator alert, not to abort.

Stuck-but-consistent beats fast-but-wrong when the alternative is losing money.

---

## 9. Custody mock

An in-process model of an escrow contract. It holds balances and escrows; the engine
holds only escrow ids and cannot reach into balances.

### 9.1 Atomic settlement

`submit` → included → the transaction validates **everything** against current chain state
and debits, or reverts entirely:

```
!nonce_used(bundle.nonce)             else revert NonceReused
for leg in bundle:
    now < leg.quote_expiry            else revert QuoteExpired
    balance(maker) >= contribution    else revert InsufficientFunds
balance(requester) >= Σ contributions else revert InsufficientFunds
── no fallible operation past this line ──
debit all, form escrows, consume the nonce
```

**One nonce per bundle, not per leg.** The transaction is atomic and legs can never settle
separately, so a per-leg nonce would carry no information the bundle nonce does not.

**Settlement validates `balance`, not `availability`** — and the distinction is
load-bearing, not pedantry:

| | Uses | Why |
|---|---|---|
| admission (§6) | **availability** = balance − pending withdrawals | forward-looking: never lend against money already on its way out |
| settlement (§9.1) | **balance** = what custody holds now | present-tense: is the money here for this transaction |

If settlement validated availability, `RequestWithdrawal` would instantly kill every basket
already in flight — last look reintroduced through custody, which is exactly what §9.3
exists to prevent. Worse, it would be invisible: the timelock would still appear to
function while every settlement quietly failed. The timelock's promise is that the money
remains *present* for as long as a quote can bind, so presence is what settlement checks.

Custody validates against **its own clock**, not the engine's. In v1 the two are separate
`Clock` instances that happen to agree; the mock allows an offset so a test can demonstrate
the divergence case — a quote the engine believes live is expired at the custody layer, the
transaction reverts, and the basket aborts safely. Without a second clock this assumption is
invisible in v1 and false in v2, and no test could ever surface it. The operational answer
is to submit with expiry headroom.

Same plan-check-commit discipline one layer down. This mirrors EVM semantics: a
transaction is atomic and reverts wholesale, so multi-leg atomicity at the custody layer
is **inherited, not built**. This is why the interface is a single `submit(bundle)` and
not per-leg calls — per-leg settlement would discard the chain's own atomicity and force
a distributed protocol to recover it.

### 9.2 Escrow

```
Locked ──settle──> Settled        (consumed flag set in the same mutation as the credit)
```

Each escrow stores **both contributions separately**, not just the notional — required
for the void path (§10.3). Re-settlement is a no-op, so replay is harmless.

### 9.3 Withdrawal timelock

`RequestWithdrawal` marks funds unavailable for settlement **immediately** and executes
after `WITHDRAWAL_DELAY`.

The configuration invariant, asserted at startup with every term named:

```
WITHDRAWAL_DELAY  >  MAX_QUOTE_TTL                     // how long a quote can bind
                  +  CONFIRMATIONS × block_time        // balance-mirror lag (§2.3)
                  +  max_indexer_lag                   // mirror lag
                  +  max_settlement_inclusion_time     // submit → final
```

**Every term after the first is zero in v1** — custody is in-process, confirmation depth is
zero, there is no indexer lag, inclusion is immediate. The inequality therefore cannot fail
in this build, and no test will ever exercise it. The terms are written into the assertion
anyway, because the shortfall is invisible exactly where it is cheapest to fix: on a real
chain, omitting them lets a maker withdraw out from under a quote that was live when
accepted, which is last look reintroduced through custody.
Any quote signed before a withdrawal request must therefore settle or expire before the
withdrawal lands. **This is what makes soft reservation safe**, and it is the reason quote
lifetime and custody policy are coupled rather than independent knobs.

Nonce cancellation carries the same delay. Instant cancellation is last look wearing a
different hat.

---

## 10. Resolution

### 10.1 The oracle is external, and its lifecycle is not the engine's

The engine has **no oracle dependency and never polls**. An oracle adapter pushes status
into the same command queue as everything else.

Critically, the engine does **not** model proposal, dispute, bonding, or voting. Those
belong to whatever oracle the venue integrates — an optimistic oracle, a trusted signer,
a committee — and importing their lifecycle would couple this state machine to a system it
does not control and cannot fix. The engine's contract state is two values:

```rust
enum ContractState { Unresolved, Resolved(Outcome) }   // Outcome = Yes | No | Void
```

The oracle interface is one status type and one command:

```rust
enum OracleStatus {
    Silent,               // nothing has happened
    InProgress,           // proposed and/or contested — working, but not final
    Final(Outcome),
}

ReportOracleStatus { contract, status }   [oracle adapter only]
SettleEscrow       { escrow }             [anyone]
```

`Void` is an outcome value, not a state, so ambiguity needs no special path.

**`ReportOracleStatus` must be monotonic**, and the engine enforces it:

```
Silent → InProgress → Final(o)
```

Regressions (`InProgress → Silent`) and overwrites (`Final(a) → Final(b)`) are rejected with
`OracleStatusRegression`. `Final` is terminal and immutable.

This is load-bearing, not hygiene. A retraction to `Silent` re-opens the stall exit — the
free unwind §10.4 exists to close. An overwrite of `Final` is worse and invisible to every
invariant in §15: escrows already settled keep their payout (§9.2 makes re-settlement a
no-op) while unsettled escrows on the same contract pay the other side, so one contract
pays both ways, decided by who sent `SettleEscrow` first. No unit is duplicated — each
escrow pays its own notional — so conservation cannot detect it. Monotonicity is the only
thing standing in the way.

**The engine's second requirement on any oracle is a liveness contract:** eventually report
`Final`, or remain `Silent`. An oracle that parks indefinitely in `InProgress` is an
oracle-quality failure, and the escalation authority is the named trust boundary that
resolves it. That authority is a single unbonded key that can assign any outcome including
`Void` — stated plainly, because §10.4 below closes only one route to a forced unwind and
this is the other.

### 10.2 Settlement admissibility

Outcome is **derived at the point of use, never stored as finalised**:

```rust
fn outcome(c: &Contract, now: Ts) -> Result<Outcome, NotYet> {
    if let ContractState::Resolved(o) = c.state { return Ok(o); }
    if c.oracle_status == OracleStatus::Silent
        && now > c.event_date + STALL_GRACE { return Ok(Outcome::Void); }   // stall exit
    Err(NotYet)
}
```

The stall exit conditions on `Silent`, not on the absence of a proposal. `InProgress` never
times out into `Void`, which preserves §10.4's property without the engine knowing anything
about how the oracle reaches finality.

No timer sets a `Finalized` flag. Time gates *admissibility*; an explicit command moves
the money. This is faithful to the layer being mocked: chains have no timers, and a
contract cannot pay spontaneously — someone must send a transaction.

**Resolution attaches to the contract; settlement attaches to each escrow.** One contract
may back thousands of escrows. `ReportOracleStatus` writes one field and touches no
escrows. Each escrow is settled by its own O(1) command. Fan-out would be unbounded work
in one critical section — and on chain would exceed the block gas limit, making settlement
impossible.

### 10.3 Payout

Payout maps through the leg's side, taken from the escrow record (§2.1). There is no
implicit buyer or seller — who wins on `Yes` is a property of the leg, not a convention.

| Outcome | Requester bought `Yes` | Requester bought `No` |
|---|---|---|
| `Yes` | notional → requester | notional → maker |
| `No` | notional → maker | notional → requester |
| `Void` | own contributions returned | own contributions returned |

Void is not a 50/50 split. Refunding contributions restores the exact pre-trade
allocation; splitting the notional moves money between the parties and is a
redistribution disguised as neutrality.

### 10.4 The stall exit, and the attack on it

The stall predicate requires `OracleStatus::Silent`. It is **unreachable once the oracle
reports `InProgress`**.

Otherwise a party who is losing contests a correct outcome, waits out the grace period, and
takes a free unwind — turning contestation into a costless option to cancel a trade they
have already lost. Once the oracle is working, the only exits are `Final` or the escalation
authority.

There are four routes to an arbitrary or forced `Void`. Stating all of them, since closing
one and declaring the attack handled is the specific failure this section exists to avoid:

| Route | Status |
|---|---|
| stall exit reached while contested | **closed** — the predicate requires `Silent` |
| oracle retracts `InProgress → Silent`, re-opening the stall exit | **closed** — monotonicity (§10.1) |
| oracle overwrites `Final(a) → Final(b)` | **closed** — monotonicity (§10.1) |
| escalation authority rules `Void` | **open** — a single unbonded key, named in §10.1 as the design's one trusted component |

Three of four are closed by engine-side rules. The fourth is a trust assumption, not a
mechanism, and is stated as such rather than argued away.

The stall exit is triggerable only by time, never by a participant, and `STALL_GRACE` is
long relative to any plausible honest delay.

The escalation authority is a single designated id — the explicit, named trust boundary
of this design.

---

## 11. Adversarial catalogue

| Attack | Mitigation |
|---|---|
| **Requester free-option / quote fishing** — collect firm quotes, accept only if the market moves | Bounded by maker-controlled expiry and the absence of a post-deadline acceptance window. Requester reservation makes fishing capital-*occupying*, not costly — a requester with idle capital fishes for free. **Partially open**; the honest closure is a per-request fee, out of scope |
| **Requester griefing** — lock maker capital with no intent to trade | An explicit `RejectRequest` releases every standing quote on the request, so a requester cannot lock maker capital and walk away — the grief costs them the full response deadline, not a keystroke. Beyond that, reservation is **not** symmetric: the requester locks `Σ size × limit_price` once, while each of N responding makers locks `leg.size × (UNIT − price)`. The ratio of maker capital locked to requester capital locked grows with N, so griefing gets cheaper per unit of damage as the venue gets more liquid. **Open**, and it is the amplification path for the Sybil row below. Bounded only by the one-quote-per-maker-per-leg rule (§6) and slab capacity |
| **Requester collateral double-spend** — many concurrent requests on one balance | Reserved at `SubmitRequest`, not at acceptance |
| **Replayed / duplicate accept** | Single serialization point plus the state machine: a second `AcceptRequest` finds the request no longer `Open` and is rejected. Chain-boundary dedup on `(tx_hash, log_index)` covers indexer replay only, which is a different path |
| **Accept after expiry via clock claims** | Venue clock only; participant timestamps never trusted |
| **Maker over-commitment / selective default** — quote ten requests off one unit of capital | Reserve at quote submission against the balance mirror; quote rejected without free balance. **Complete in v1 only** — in v2 the mirror lags, so an over-committed quote is admitted and fails at settlement instead. The v2 closure is the §9.3 timelock terms, not this check |
| **Maker quote-and-run** — withdraw after being selected | Irrevocable until expiry; withdrawal timelock satisfies the **four-term** inequality of §9.3, not merely `> MAX_QUOTE_TTL` — two terms are insufficient once the mirror lags |
| **Maker last look via insolvency** | Settlement validates against live custody state atomically and reverts wholesale, so no half-basket can form. Note the residue: since accept no longer touches custody (§7.2), an insolvent maker no longer causes a clean accept-time rejection — it causes a terminal `SettlementFailed` on a request that cannot be re-accepted. Repeated insolvency is therefore a **denial vector against a requester**. Bounded by the §9.3 timelock; not otherwise closed |
| **Quote spam / DoS** | Reservation makes spam capital-costly for any quote that could win — but a quote at `price == UNIT` reserves **zero**, and §6 admits it. Closed by the one-live-quote-per-maker-per-leg rule (§6), which bounds occupancy at `makers × legs` regardless of message volume; slab capacity bounds the remainder |
| **Requester authors ambiguous wording** — craft a contract likely to resolve `Void`, giving a free unwind if the trade goes against them | No participant can reach `Void` through the engine: the stall exit requires `Silent`, and oracle monotonicity blocks retraction and overwrite (§10.1). The escalation-authority route stays open by construction (§10.4). Residual risk is priced by makers (§5.3) |
| **Contract-identity confusion** — near-identical wording passed off as an existing contract | Identity is the hash of the full description; no canonicalisation, no fuzzy matching |
| **Presentation race** — accept lands after selection has moved against the requester | Accept carries expected per-leg prices; at-or-better semantics; worse fills reject with `PresentationStale` (§7.1.1) |
| **Losing party contests to force a void** | Participants cannot set `InProgress` at all — status comes only from the oracle adapter. The real dependency is oracle **monotonicity** (§10.1); without it, contest-then-retract re-opens the stall exit |
| **Oracle lies** | **Not defended, by construction.** The engine does not model proposal, bonding, or voting (§10.1); what stands between a lying oracle and the money is `ReportOracleStatus{Final(o)}` from a single adapter, irreversible for every escrow settled under it. Bonded/optimistic behaviour lives in the oracle being integrated and is mocked in `chain`. This is the design's largest residual trust assumption and is stated, not mitigated |
| **Stale-mirror fill denial** — a maker keeps the engine's balance view stale so baskets containing them abort at settlement | Liveness cost only; no money moves wrongly. Bounded by the §9.3 timelock terms. **Accepted risk** in v1, where mirror lag is zero |
| **Sybil requesters** | **Accepted risk.** Unfixable without identity, which is out of scope. Named rather than hidden; the hook is a per-account deposit minimum |
| **Wash trading / self-dealing** | **Accepted risk.** Requires identity linkage; noted |

---

## 12. Chain boundary and indexer

The mock chain emits an append-only log with real chain characteristics:

```rust
struct ChainEvent { block: u64, tx_hash: [u8; 32], log_index: u32, payload: ChainPayload }
```

The indexer is a separate component translating log entries into engine commands:

- **cursor** — resumable position
- **confirmation depth** — events applied only at `block + CONFIRMATIONS <= head`
- **dedup** on `(tx_hash, log_index)` — restart replays are harmless

Confirmation depth *avoids* reorgs rather than recovering from them. Deeper-reorg recovery
(roll back and reapply from the command log) is designed, not built.

Not modelled: block production, gas, mempool ordering, signature verification, EIP-712
encoding, ERC-20 semantics.

**Authority in v1 vs v2.** In v1 the engine's ledger and the chain mock are both in-process
and the chain is the custody authority already. In v2 the chain is remote, the engine's
view becomes a *projection*, and a reconciler — comparing local escrow state against
contract state — becomes the third component. The engine core does not change: same
commands, same predicates, same state machine. Only the producer of the commands and the
guarantees they carry change. This is the point of the seam.

---

## 13. Concurrency and runtime

Single-writer. One thread owns all mutable state and busy-spins on a bounded MPSC command
channel. Gateway threads and the indexer produce commands; a publisher thread consumes an
event stream and performs all I/O.

The engine performs **no I/O and no allocation** in `apply`. Events are written into a
caller-provided buffer.

**Backpressure is drop-oldest, never block.** The event queue is bounded and carries a
sequence number so consumers detect gaps. Blocking the sole writer on a full queue would let
one slow consumer stall the entire venue — a denial vector strictly worse than the lost
events it prevents. The engine must never be stallable by a consumer.

**If the publisher dies, the engine keeps applying.** The audit trail is best-effort; the
state machine is authoritative. Stopping the sole writer would leave commands unapplied and
the command log incomplete, which is worse than losing event continuity: state stays correct
and replayable either way, while external observers lose their feed. That is a liveness
degradation with an operator alert, the same class of choice as §8.3's stuck-but-consistent,
and it is stated rather than left as a runtime accident.

Consequences, all from one decision:

- most races in §11 are impossible by construction, not guarded by a lock
- the command log gives deterministic replay, an audit trail, and a crash-recovery story
- tests need no sleeps: enqueue commands, advance the injected clock, assert

The single-writer claim is *proved*, not asserted. A test submits from several **client**
threads concurrently — the engine stays single-threaded — and asserts:

1. every submitted command appears exactly once in the command log, in channel order;
2. replaying that log through a fresh engine, single-threaded, reproduces the final state
   byte-for-byte.

The second is the real content of the claim. "The result is one of the legal serial orders"
is not assertable — for N concurrent submits the legal set is N! — and in practice degrades
to re-checking conservation, which any implementation satisfies.

Scaling answer if pressed: shard by request id; the ledger is the cross-cutting resource
and stays single-writer; the real bottleneck is settlement I/O, not the state machine.

### 13.1 Two systems, and the harness that holds them

The engine and custody are modelled as **independent systems that share no memory**. This is
not a layering preference — it is the thing that makes the v1/v2 story true, and it is
enforced structurally rather than by discipline.

| | Owns | Crate |
|---|---|---|
| **Engine** | requests, quotes, reservations, claims, the balance *mirror* | `core` |
| **Custody** | balances, escrows, nonces, the withdrawal timelock | `chain` |
| **Oracle** | proposal, contest, window, escalation | `chain` |
| **Indexer** | cursor, confirmation depth, dedup | `chain` |

Rules that hold in every build:

- The engine holds no reference to custody and cannot read a balance from it. It sees
  `EscrowId` and its own mirror, nothing else.
- Custody holds no reference to the engine, knows nothing of requests, quotes, legs,
  reservations or claims, and has never heard of a "committed" bucket.
- The only path from engine to custody is a `SubmitIntent` event, picked up by an adapter.
  The only path back is a chain event, translated to a command by the indexer.
- Each has its **own clock** (§4.1, §9.1). Neither can read the other's.

Communication is therefore one-directional at both ends and always asynchronous in shape,
even where v1 resolves it in-process. Nothing in the engine's code can be written that would
not compile if custody were on another machine — which is what makes §12's claim that the
core does not change in v2 an actual property rather than an aspiration.

**The harness is a test-and-scenario structure that owns both.** It holds an engine and a
custody instance, drives the wires between them — pumping the event stream to the settlement
adapter, and the chain log to the indexer — and advances both clocks. It exists because two
systems that cannot see each other still need something that can see both:

- **It is the only place global conservation and claim coverage can be asserted** (§2.2),
  since those span both halves. That is why those rows say "the harness" and not "core".
- It is where clock divergence is injected (custody ahead of the engine, §9.1).
- It is where lag, confirmation depth and settlement outcomes are made deterministic.

Two constraints on it:

- **The harness is not a back door.** It may read both systems; no engine code path may. If
  a production path ever needs something only the harness can see, that is a design error,
  not a convenience.
- **It has no production counterpart.** In v2 its wiring is replaced by real transport, and
  its cross-system assertions become the reconciler (§12) — which is a monitoring component
  that can *report* divergence, not an oracle of truth that prevents it.


---

## 14. Quote lifetime: seconds or days

**Invariant to the decision**

- The state machines in §5, §6, §10 — no state or transition changes
- Conservation, reserve-at-quote, plan-check-commit atomicity
- Expiry as an absolute timestamp per quote
- Liveness as a predicate, so correctness is independent of reclamation granularity
- Selection as a pure function over live quotes at time T

**Not invariant**

| | Seconds | Days |
|---|---|---|
| Reclamation | per-account intrusive chain, release-on-access | **same structure** — no span to exceed; capital limits keep each account's chain short at day-scale |
| Persistence | in-memory; a restart loses nothing that matters | durable store; reservations must survive restart |
| Capital cost | negligible | material — makers will not lock capital for days |
| Cancellation | irrevocable is acceptable | cancel/replace becomes mandatory |
| Custody | signature commitments are adequate | timelock must exceed TTL, so days-long quotes force real on-chain locking or bonding |

**Built so either is cheap**

- `Clock` trait, injected; expiry never read from a wall clock inline
- Liveness predicate authoritative; release-on-access is the sole reclamation mechanism and
  is independent of quote lifetime, so no reclamation change is implied by the decision
- `CancelQuote` present as a rejected transition, so enabling it is a policy change
- Storage behind a repository trait whose unit is the transaction, not the row
- `MAX_QUOTE_TTL` is a market-structure input, coupled to `WITHDRAWAL_DELAY` by a startup
  assertion (§9.3). No data structure constrains it in either direction

The sharpest form of the answer: the custody timelock, not the data structure, is what
actually makes long-lived quotes expensive. Reclamation is an implementation detail;
capital lockup is a business decision.

---

## 15. Invariants

Each is labelled with where and when it is asserted. They are not all of one kind, and the
previous single blanket claim was false for two of them.

**Runtime, core, `debug_assert` after every state change:**

1. **Chain integrity.** `account.reserved == Σ` amounts on that account's expiry chain, and
   `account.committed == Σ` amounts on the committed lists of that account's requests and
   consumed quotes. Structural and always true; independent of the clock.
2. **Normalisation.** Immediately after `release_expired(A, now)`, no reservation on A's
   expiry chain has `expires_at <= now`. Scoped to just-normalised accounts, because
   `account.reserved` is a stored value while an expiry predicate shrinks with the passage
   of time alone — asserting the predicate form globally would be unsatisfiable for any
   untouched account, and is the form CLAUDE rule 16 must **not** use.
3. **Reservation/quote coherence.** Every claim's `owner` **must resolve**, and the
   resolved target must point back at the claim. A `reserved` entry references a standing
   quote; a `committed` entry references either exactly one `Consumed` quote on a request in
   `Settling`, or that request itself (the requester side) — §2.4.

   The "must resolve" half is load-bearing. If a failed dereference is treated as *nothing
   to check*, a claim naming a slot that has been freed and reissued passes silently — which
   is precisely the stale-handle class that generation counters exist to catch, and the
   assertion would be blind to the one case it is for.
4. **No mutation on rejection.** A rejected command leaves state byte-identical **to the
   post-normalisation state** (§4.3), checked by a test-only state hash taken after
   normalisation and again after the command. Normalisation is time catching up and is not
   the command acting.

**Runtime, harness, after every command in tests and scenarios:**

5. **Conservation** (§2.2): `Σ custody.free + Σ escrow notional == deposited − withdrawn`.
   Purely custody-side — `reserved` and `committed` are claims against `free`, not
   partitions of it, and adding them here would double-count.
6. **Claim coverage** (§2.2): `∀ a: custody.free(a) ≥ reserved(a) + committed(a)`. A
   violation means the core has promised capital custody does not hold.
7. **Mirror agreement.** `mirror.free(a) == custody.free(a)` for all accounts — exact in
   v1; in v2 this becomes a bounded-drift assertion, and the bound is the §9.3 lag terms.
8. **Escrow contributions** sum to notional, and each escrow stores both sides separately.

**Graph property, asserted once by an exhaustive reachability test:**

9. Every non-terminal state in the request, quote and contract machines has at least one
   reachable exit transition, with **two named exceptions**:
   - `Settling` under indefinite `Unknown` — exits only by operator intervention (§8.3),
     because releasing on a guess is the duplication path
   - an escrow whose oracle parks in `InProgress` with no escalation ruling — exits only
     when the oracle honours its liveness contract (§10.1)

   Both are stuck-by-choice, argued in their sections, and declared here so the reachability
   test asserts a true statement rather than being weakened when it fails.

Property tests drive randomised command sequences against 1–8. Invariant 9 is a separate,
non-randomised test over the state graph.

## 16. Out of scope

Cuts, with the reasoning. A cut that is merely absent reads as an oversight; a cut that is
named and argued is a decision.

Note the method these came from: checking the design against the *product* rather than
against itself. A review that only checks internal consistency cannot find a concept that is
simply missing, because a missing concept contradicts nothing.

### Excluded by the brief

UI, auth, accounts, KYC. Real chain integration or real money movement. Pricing,
correlation, and risk logic.

### Deliberate scope cuts

**Quote aggregation across makers on one leg.** Full size or nothing (§7.1). Aggregation
turns each leg into a miniature order book and adds partial-fill-within-a-leg as a new
failure surface. Behind a `SelectionStrategy` trait, so it is a second implementation.

> Known and permitted consequence: nothing requires legs to name distinct contracts, so a
> requester may split one exposure across several legs at smaller sizes and have different
> makers fill each, atomically — aggregation by construction, bounded by `MAX_LEGS`. This is
> allowed rather than accidental, and it is not a substitute for true aggregation, since the
> requester must choose the split in advance.

**Package quoting on multi-leg requests.** Quotes are per leg, and greedy per-leg selection
is optimal only because legs are priced independently (§7.1). For a spread the package price
*is* the product: per-leg quoting leaves the requester carrying leg risk and gives the maker
no way to price the correlation they are taking. Supporting it makes selection an assignment
problem and requires correlation logic, which the brief excludes.

**Multi-outcome (categorical) markets.** §2.1 fixes a binary payoff. A five-candidate,
mutually-exclusive market can only be expressed here as unlinked binaries with no
mutual-exclusivity constraint and no shared collateral — so it is not supported. Supporting
it means an *n*-outcome escrow where the notional is posted once against a set, which changes
the contribution formula and the payout mapping and nothing else structural.

**A second settlement asset.** One implicit asset, `UNIT`-scaled. No asset field appears in
any structure. A second currency needs an asset id on balances, escrows and legs, and a
policy for cross-asset baskets — which is a product decision, not a mechanical one.

**Position and reconciliation read models.** The engine can answer both questions — escrow
ids are recorded against requests, and every reservation carries `owner: ResOwner{Quote |
Request}`, which is exactly "what is my capital locked against". Neither is *exposed*: there
is no query command, snapshot, or statement, and no resynchronisation path for a consumer
that has fallen behind. Participants must reconstruct from the event stream. This is a
missing surface, not a missing model, and it is the first thing to build after the core.

**Netting.** A participant holding both sides of one contract is collateralised twice.
§2.1's "fully prefunded, no margin, no leverage" is a property of the design, not an argument
against netting — netting is simply absent.

**Fees.** Note where a fee would land: §2.1's guarantee that the two contributions sum to the
notional exactly is what makes the money path division-free and dust-free. Any fee has to
come out of that sum, so introducing one reopens the rounding question the representation was
chosen to make unaskable. That is the reason it is cut, not merely that it was not needed.

**Network transport, sessions, and market data distribution.** The event enum is the wire
format and transport is an adapter (§13). What an adapter would additionally need is not
specified: session and heartbeat semantics, per-consumer sequencing and replay, and selective
subscription. Flow control is one global drop-oldest queue, so a slow consumer's losses are
decided by the venue's ring rather than per connection. There is also no public market data
feed — `BestSelectionChanged` is per-request and private to that requester.

**Signature verification, and therefore authorisation.** §12 excludes signatures. The
consequence is worth drawing: §5's "requester only" and §10.1's "oracle adapter only" are
therefore *unenforceable* inside the system as specified — nothing binds a command to an
identity beyond the index the gateway assigns. The permission model is designed and not
enforced, and that boundary is the gateway's.

**Bond economics for the oracle.** The optimistic-oracle shape is mocked (§10.1); bond sizing
and slashing are the integrated oracle's concern.

### Not applicable, rather than cut

**Two-sided maker quoting.** In an RFQ venue a maker responds to a specific request asking a
specific side; there is no resting two-sided quote because there is no book. A maker showing
"both sides" of a contract means responding to two requests with opposing sides, which is
already supported — quotes are keyed per `(request, leg)` and nothing couples them by
contract.


---

## Appendix A. Worked example

The canonical happy path. Every scenario in the runner traces money at each step; this is
the reference the numbers are checked against. Amounts in USDC; internally everything is
minor units (`UNIT = 1_000_000` per contract).

### The request

A calendar spread — long September, short October:

| Leg | Contract | Side | Size | Limit |
|---|---|---|---|---|
| A | ECB cuts at the September meeting | `Yes` | 100,000 | 0.65 |
| B | ECB cuts at the October meeting | `No` | 100,000 | 0.50 |

Requester reserves `Σ size × limit` = 65,000 + 50,000 = **115,000**, before any price
exists. `RequestOpened` fans the legs out to makers — contract, side, size, deadline. No
limit prices.

### The auction

| t | Event | Reserved | Note |
|---|---|---|---|
| +1s | Alpha quotes A @ 0.62 | 38,000 | |
| +2s | Beta quotes A @ 0.61 | 39,000 | new best; `BestSelectionChanged` |
| +2s | Gamma quotes A @ 0.61 | 39,000 | same price, later arrival — loses the tiebreak |
| +3s | Delta quotes A @ 0.70 | 30,000 | **above the limit**: admitted, reserves, never published, never eligible |
| +3s | Gamma quotes B @ 0.45 | 55,000 | |
| +4s | Beta quotes B @ 0.43, TTL 2s | 57,000 | best — but expires at +6s |
| +5s | Alpha quotes B @ 0.48 | 52,000 | |
| +7s | Alpha **replaces** B @ 0.44 | 56,000 | improvement, so permitted; old claim released |

Six behaviours in one auction: competition, arrival-order tiebreak, over-limit admission
without rejection, expiry removing a best quote, replacement changing the winner, and a
maker holding one slot per leg however many times they requote.

### Acceptance at +8s

Plan selects Beta on A @ 0.61 and Alpha on B @ 0.44. Beta's B quote is dead; Delta is
ineligible; Gamma is outbid on both.

```
requester    115,000 reserved
             → 105,000 committed   (61,000 + 44,000)
             →  10,000 released    (limit − fill)
Beta    A      39,000 reserved → committed
Alpha   B      56,000 reserved → committed
Alpha A 38,000 · Gamma A 39,000 · Delta A 30,000 · Gamma B 55,000
             → released, each maker sent QuoteRejected{Outbid}
```

Escrow forms only when settlement confirms. Each leg holds exactly `size × UNIT` =
**100,000**, split 61,000/39,000 and 44,000/56,000.

### Resolution

Both contracts resolve `Yes` — the ECB cuts at both meetings.

| Leg | Requester side | Outcome | Notional 100,000 → |
|---|---|---|---|
| A | `Yes` | `Yes` | **requester** |
| B | `No` | `Yes` | **Alpha** |

Both directions of the payout mapping in one settlement, which is the point of the mixed
sides. The requester paid 105,000 and received 100,000: down 5,000, exactly the cost of a
spread where one leg won and the other lost.

Conservation holds at every step above, and claim coverage holds throughout.
