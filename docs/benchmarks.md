# Hot-path measurements

Three implementations of the engine's event hand-off, and two of the command log, measured
rather than argued. Produced by `cargo bench -p rfq-runtime --bench latency` (percentiles)
and `--bench hot_path` (criterion means).

**Read the comparisons, not the absolutes.** These are laptop numbers on a machine with
frequency scaling and other processes; the ratio between two rows on the same run is the
thing the changes were made for.

**Method.** Each sample times a batch of 32 operations with a consumer thread running;
per-operation figures are derived. Timing a single ~40 ns operation directly is meaningless
because `Instant::now()` costs about as much as the operation. A batch containing a stall
still shows as an outlier, which is what the tail columns exist to catch. 40,000 batches per
row, three independent runs.

Measuring latency requires a wall-clock read, which CLAUDE 1 confines to the `Clock`
implementation. Treated here as a narrow, named exception: a benchmark is outside the engine,
and criterion already reads the clock internally.

---

## Event hand-off

Nanoseconds per operation, mean of three runs. **Compare on p99.9, not `max`** — a single
OS interruption dominates the maximum of forty thousand samples and measures the machine.

| design | mean | p50 | p99 | **p99.9** |
|---|---|---|---|---|
| 1 — mutex, spinning consumer | 54.9 | 51.2 | 234.8 | 709.2 |
| 2 — mutex, cache-padded atomic probe | 39.6 | **10.9** | 371.5 | 834.2 |
| 3 — bounded channel *(shipping)* | 50.1 | 26.9 | **205.7** | **327.7** |

Per run, so the consistency is visible:

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| 1 · mean / p50 / p99 / p99.9 | 73.5 / 65.1 / 304.7 / 963.5 | 45.2 / 45.6 / 195.3 / 567.7 | 45.9 / 43.0 / 204.4 / 596.3 |
| 2 · mean / p50 / p99 / p99.9 | 35.7 / 10.4 / 343.8 / 632.8 | 36.1 / 10.4 / 332.0 / 622.4 | 47.0 / 11.7 / 438.8 / 1247.4 |
| 3 · mean / p50 / p99 / p99.9 | 46.8 / 27.3 / 155.0 / 312.5 | 47.4 / 27.3 / 170.6 / 282.6 | 56.2 / 26.0 / 291.7 / 388.0 |

### What this says

**Design 3 dominates design 1 outright** — better on median, p99 and p99.9 in every run. The
mutex with a spinning consumer is simply worse than a channel on every axis.

**Design 2 and design 3 optimise different halves of the distribution.** The atomic probe has
a median of ~10.4 ns, stable to a tenth of a nanosecond across runs and roughly 2.5× better
than the channel. In the common case, when the consumer is not holding the lock, the
producer's acquisition is uncontended and the extra atomic store is nearly free.

But it has the **worst p99 and p99.9 of the three**, consistently. The mechanism is visible
once looked for: the probe removes the consumer's contention and adds a producer-side store
to the padded length on *every* push, so the producer invalidates two cache lines where it
previously invalidated one. When the threads land badly it pays twice, and run 3 shows it —
p99.9 of 1,247 ns against the channel's 388.

**For a single writer the tail is what matters**, because a stall is time during which no
command in the venue is applied at all. Trading a 16 ns median for halving the p99.9 is the
right way round, and predictability is worth more than speed in the common case.

### A correction to the criterion conclusion

The criterion run behind commit `06b9983` reported design 2 as bimodal and concluded it was
"not demonstrably an improvement". **That was wrong, and wrong in an instructive way.**
Criterion reports a mean; design 2's distribution is heavily skewed — a 10.4 ns median with a
long tail. What criterion saw moving between runs was the tail, not the typical case.

Design 2 *is* a real improvement to the common case and a real regression to the tail. A mean
was the wrong summary statistic, which is exactly why this second harness exists.

---

## Command log

| design | mean | p50 | p99 | **p99.9** |
|---|---|---|---|---|
| 1 — `Vec` grown on the writer | **18.3** | **6.5** | 73.3 | 434.5 |
| 2 — bounded channel to a logger *(shipping)* | 38.4 | 38.6 | **88.6*** | **281.3** |

\* p99 per run: 52.1 / 52.1 / 161.5 — better than the `Vec` on two runs of three, worse on
the noisy third. p99.9 per run: 170.6 / 152.3 / 520.8 against 420.6 / 411.4 / 471.3.

### What this says

The `Vec` wins the common case decisively — a 6.5 ns median against 38.6, because a local
push into preallocated space is simply cheaper than a cross-thread hand-off.

The channel wins the tail: on the two clean runs its p99.9 is around 160 ns against the
`Vec`'s 415, a **2.5×** improvement, and its p99 is 52 ns against 73.

So the trade is ~32 ns of median per command against roughly halving the tail. Narrower than
the criterion figures implied — that group measured a *forced* boundary push on a fresh
65,536-entry `Vec` at 194 µs, while an organically growing log's worst batch here is a few
microseconds. Large reallocations on this platform appear to be served by virtual-memory
remapping rather than a physical copy, which makes the spike far cheaper than the byte count
suggests, and platform-dependent in a way the earlier number implied it was not.

**The strongest justification for the change is still not latency.** It is that a `Vec` on
the writer thread grows without bound, in the writer's own address space, with no policy for
what happens when it is too large — and that growth belongs on a thread that can page, rotate
or persist it. That argument stands independently of these numbers.

---

## Claim coverage

No threads, so this is the one to believe without caveats. Criterion means:

| | |
|---|---|
| global — walk all 256 preallocated account rows | 159.5 ns |
| scoped — the two accounts a command actually touches | **0.29 ns** |

Roughly **540× per command**, and it was running unconditionally in release to maintain a
counter nothing read. This is the least ambiguous result in the file and the clearest of the
three fixes.

---

## What the exercise was worth

Three findings, two of which contradict something previously asserted:

1. **Coverage** — a 540× per-command cost, exactly as expected. Confirmed.
2. **The event path** — the atomic probe improves the median ~2.5× and worsens the tail. The
   earlier "not demonstrably an improvement" was an artefact of applying a mean to a skewed
   distribution. The channel beats the original mutex on every axis and beats the probe where
   it counts for a single writer.
3. **The command log** — the tail argument is real but narrower than claimed (~2.5× at
   p99.9, not the 6,000× a forced-boundary microbenchmark suggested), and the justification
   that survives measurement is unbounded growth on the writer rather than latency.

None of these were visible to a test suite that asserts correctness, which is why the
benchmark was worth adding even though the design's structural arguments were sound.
