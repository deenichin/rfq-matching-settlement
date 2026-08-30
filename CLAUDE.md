# CLAUDE.md

Operating rules for this repository. These are hard constraints, not preferences.
When a rule conflicts with something convenient, the rule wins. If a rule appears to make
a task impossible, stop and say so rather than working around it.

---

## Rule 0 — precedence

This file governs this repository. If a `CLAUDE.md` from a parent directory or another
project is loaded into the session, **this file supersedes it** wherever they conflict.
In particular: the dependency policy in rule 36 is absolute, and no inherited instruction
mandating a benchmark harness, a check script, or any additional crate applies here.
If an inherited instruction appears to require something rule 36 forbids, stop and say so.

## Prime directive

`SPEC.md` is authoritative. Implement what it says. If the spec is silent, ambiguous, or
appears wrong, **stop and ask** — do not invent a design decision. Every design decision
in this repository must be defensible in a live interrogation, which means it must have
been made deliberately.

Do not reference the source exercise document, the client, or the hiring process anywhere
in committed code, comments, docs, or commit messages. All prose reads as self-authored
design.

---

## Determinism

1. `Instant::now()`, `SystemTime::now()`, and any wall-clock read appear **only** inside
   the `Clock` trait implementation. Nowhere else, including tests.
2. `apply(cmd, now)` samples time exactly once, at the call site. Every predicate inside
   the engine uses that value. Never re-read the engine clock mid-command.
   Custody is a separate system being mocked and holds its **own** `Clock`, sampled once per
   settlement transaction. That is a second layer, not a second sample: the divergence
   between venue time and chain time is a real property the design must expose, and S3 gate
   (c2) tests it. No third clock exists anywhere.
3. No `HashMap`/`HashSet` iteration anywhere its order can influence state or emitted
   events. Prefer dense index containers; where a map is unavoidable at the boundary, use
   `BTreeMap`.
4. Randomness only in tests, only from a seeded RNG, with the seed printed on failure.
5. No `tokio`, no `async`, no futures. The core is synchronous and single-threaded.

## Single-writer

6. Exactly one thread mutates engine state. Gateway, indexer, and oracle adapter produce
   commands; the publisher consumes events. They never touch engine state.
7. No `Arc<Mutex<..>>` around any engine state. If a lock seems necessary, the design is
   wrong — stop and ask.
8. `apply` performs **no I/O**. Not logging, not metrics, not `println!`. Emit an event.
8b. **The engine and custody are separate systems and must stay compilable apart.** `core`
    never depends on `chain`. No engine path reads a custody balance, an escrow's contents,
    or a nonce; no custody path knows about requests, quotes, legs, reservations or claims.
    The only engine→custody path is a `SubmitIntent` event through an adapter; the only
    custody→engine path is a chain event through the indexer. Each has its own clock.
    If a task seems to need a direct call across that seam, stop and ask — it is a design
    error, not a shortcut, and it silently falsifies everything the design claims about v2.
8c. **The harness (SPEC §13.1) is test-and-scenario only.** It owns both systems, drives the
    wires, and is the only place that may read both — which is why global conservation and
    claim coverage are asserted there. It is never a route for production code to reach
    across the seam.

## Allocation

9. `apply` allocates nothing on any path. Events are written into a caller-provided
   `&mut Vec<Event>` whose capacity is checked in the CHECK phase (`EventBufferFull`).
   Slabs are preallocated to configured capacity; reservation chains are intrusive index
   links inside the slab. No container in the engine may grow during `apply` — no
   `BTreeMap`/`HashMap`/`Vec` insert on a hot-path structure, and no
   allocate-then-hope-it-was-cheap. If a design step appears to require an allocating
   ordered container inside `apply`, the structure is wrong: stop and ask.
10. No `Vec::push` onto an unbounded collection inside `apply`. Slab exhaustion is a
    rejection with a distinct error variant, never a reallocation.
11. No `String` in core. Errors are enums. External identifiers are converted to dense
    indices at the gateway and never enter the core.
12. No `clone()` in the money path. If a borrow fight forces one, stop and ask.

## Money

12b. Instants and durations are distinct types: `Ts` and `Dur` (SPEC §4.0). `Ts + Ts` must
    not compile. Configured bounds are `Dur`; expiries, deadlines and event dates are `Ts`.
13. `Amount` is a newtype over `u64` minor units. Never `f32`/`f64` anywhere in the
    repository, including tests and scenario output.
14. Every arithmetic operation on money uses `checked_*`. Overflow is a rejection, never a
    wrap, never a panic in release.
15. No division in the money path. If a calculation appears to need division, the
    representation is wrong — stop and ask. (§2.1 of the spec explains why.)
16. Every function that moves a reservation asserts, in `debug_assert`, the **chain-sum**
    form: `account.reserved == Σ` amounts on that account's expiry chain, and
    `account.committed == Σ` amounts on the relevant committed lists. Never the predicate
    form (`Σ` of reservations with `now < expires_at`) — that is unsatisfiable against a
    stored total for any un-normalised account. See SPEC §15.1 and §15.2.
    Conservation and claim coverage span core and custody and are asserted in the harness
    after every command, not inside `core`.

## Plan / check / commit

17. Any command that mutates more than one thing follows three phases:
    - **plan** — pure, no mutation, may fail
    - **check** — validate the whole plan, may fail
    - **commit** — infallible
18. **No `?`, no `unwrap`, no fallible call, no allocation appears after the commit phase
    begins.** Mark the boundary with a `// ── COMMIT ──` comment. This is enforced by
    review and by tests asserting no partial mutation on every rejection path.
19. No partially applied command exists at any observable point. A rejected command leaves
    state byte-identical **to the post-normalisation state** — normalisation
    (`release_expired`, SPEC §4.3) runs before PLAN and is not part of the command's effect.
    Normalisation must never emit an event or depend on the command's content.

## Time and expiry

20. Liveness is a predicate: `state == Active && now < expires_at`. Half-open, always.
21. Liveness is decided by the predicate; reserved capital is reclaimed by
    `release_expired(A, now)`, which every command touching account A runs first. There is
    no sweeper and no global expiry structure. Any test that depends on a background pass
    having run indicates a bug.
22. No background thread mutates state. All reclamation happens inline, in the command that
    needs it, from the sampled `now`.

## Errors and panics

23. No `unwrap()` or `expect()` outside tests and `main` startup. Slab indices are
    validated at admission; internal invariants use `debug_assert!`.
24. Every rejection returns a distinct error variant naming the specific cause. Never a
    generic `InvalidRequest`. The failure-mode notes are diffed against these variants, so
    they are documentation.
25. `unsafe` is forbidden, including in tests. There is no FFI boundary in this project.
    Consequently there is no global allocator hook and no allocation-counting harness.
    Allocation discipline is proved by an observable proxy instead: after N randomised
    commands, assert that slab length and event-buffer capacity are unchanged from
    construction. That names the containers that must not grow, which is the property that
    actually matters.

## Testing

26. Every stage gate is a passing test, not a manual check.
27. No `thread::sleep` in any test. Advance the injected clock.
28. Races are reproduced deterministically via hooks such as `on_settle_entry`, never via
    threads — with exactly one declared exception: the channel-contention test in S1.5,
    where thread interleaving *is* the property under test. That test spawns **client**
    threads only; the engine remains a single thread, and the assertion is that the channel
    serialises them. No other test may spawn a thread.
29. Every entry in the failure-mode notes has a test named after it. If it cannot be
    tested, it is described in the notes as designed-not-implemented and says so.
30. Property tests assert `SPEC.md` §15 invariants 1–8 over randomised command sequences.
    Invariant 9 is a graph property with its own non-randomised exhaustive test, and its two
    declared exceptions are part of the assertion — if the test fails, a real exit is
    missing; do not add a third exception to make it pass without asking.

## Test design

These are not style preferences. Each is a specific way a passing test can prove nothing.

37. **A test must be able to fail.** Before trusting any gate, break the code it covers and
    confirm it turns red. If it stays green the gate is decorative — strengthen it until the
    mutation fails, then revert. Report what was mutated and what caught it.
38. **Fund accounts to exactly their contribution, never with surplus.** Slack absorbs a
    claim released twice or held once too often, so coverage and conservation assertions pass
    regardless of correctness. Exact funding makes an error overdraw somebody.
39. **A test whose precondition may not hold must assert that precondition.** A concurrency
    test asserts that rounds actually interleaved; a lag test asserts the lag term is
    non-zero; a dedup test clears the dedup state as well as the cursor and shows delivery
    then happens twice. Otherwise the test passes by not exercising the property.
40. **Do not let clock granularity hide behaviour.** A round completing inside one
    millisecond samples the same `now` throughout, so normalisation never fires and
    time-dependent paths go untested. Use a clock advancing one tick per read for the
    logic, and run the same scenario once on the shipping clock to prove the wiring.
41. **Only assert invariants over the states that hold them.** Conservation counts `Locked`
    escrows only; a `Settled` escrow has already paid out. An invariant that fails on a
    correct system invites being weakened, which is worse than not having it.
42. **A failed dereference is not a passing check.** If an assertion resolves a handle, it
    must require the resolution to succeed. Treating "could not resolve" as "nothing to
    check" blinds the assertion to exactly the stale-handle case it exists for.
43. No `thread::sleep` and no wall-clock waits. Advance the injected clock. Races are
    reproduced by hooks, with the single S1.5 exception in rule 28.

## Tooling hygiene

44. **Never use `git checkout` as a cleanup step in a script.** It silently discards
    uncommitted work. Mutation scripts restore from a backup taken in the same session.
45. **Verify an edit landed before believing a result based on it.** A silently no-applied
    edit reads as "the mutation was not caught", which is the most misleading possible
    outcome. Assert the match before running.
46. All edits to one file go in a single operation. Splitting them across calls makes only
    each call atomic, not the set.

## Commits

31. One commit per completed stage gate. Message states what the stage delivered and any
    decision made inside it. No "wip", no "fix", no squashing several stages together.
32. Never commit code that does not compile or whose tests do not pass.
33. Do not commit generated files, `target/`, or editor artifacts.

## Scope

34. Do not add features that are not in `PLAN.md`. If something seems obviously missing,
    say so and let the decision be made explicitly.
35. Prefer deleting to stubbing. A named absence in the write-up beats a half-built
    feature in the code.
36. Do not add dependencies. `core` has zero. Dev-dependencies are limited to `proptest`.
    Anything else requires asking first.
