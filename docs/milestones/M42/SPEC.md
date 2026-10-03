# M42 — Ids beside codes, priced

**Serves:** [BACKLOG](../BACKLOG.md) row 26: "a query is four sequential round trips and D-34
allows three … the way to three is a **format** change … and is therefore a measurement rather
than an opinion".

## What is true today

A cold query is four sequential rounds:
1. HEAD;
2. the open (footer and centroids);
3. the legs (for a dense leg, the probed lists' rabitq and sq8 codes);
4. the row blocks holding each hit, for its id and attributes.

That fourth round also serves M9c's shadow check. Row 26's alternative stores each row's id
beside the codes a dense leg already reads, trading an id per **hit** for one per
**candidate**.

- A **candidate** is an index row whose rabitq code the leg reads: replicas included, but none
  are configured by default.
- The assumed encoding is the id's bytes plus a 1-byte length prefix.

**What it could reach, and what it could not:**
- **Three rounds only for dense, id-only queries** on a clustered segment, with `rerank`
  `none` or `fast`.
- Not for:
  - attribute-bearing queries, whose attributes live in the row blocks (M9a);
  - `rerank: exact`, which adds a vectors round of its own;
  - text and sparse legs, whose ids are resolved from row blocks too: they would need ids
    beside their postings, which is unpriced here;
  - an exact-scan segment below 25,000 rows (D-10), which reads `Vectors`. Ids there cost one
    per row of the segment.

**Measured while planning this milestone** with a harness that was not committed: real `Engine`
write, flush and fold, then a cold dense query at `top_k` 10, averaged over 20 queries, at
coalesce gap 0. The corpus is a 128-dimension Gaussian mixture with 36-byte ids.

| rows | p | candidates | total bytes | ids beside codes | net after dropping round 4 |
|---|---|---|---|---|---|
| 100,000 | 8 (default) | 34,411 | 5,612,188 | +1,273,198 (+22.7%) | +21.2% |
| 400,000 | 8 | 49,248 | 8,154,086 | +1,822,189 (+22.3%) | +19.8% |
| 400,000 | 2 | 11,411 | 2,100,118 | +422,209 (+20.1%) | +10.1% |

- Every candidate already carries 160 bytes of codes, so the penalty is about (id + 1) / 160,
  whatever the scale, and it grows with id length and probe width.
- Round 4 reads whole row blocks, which grow with the segment, so the break-even id length
  rises with scale. Ids of about 8 bytes would save bytes at 400,000 rows and p = 2.

**The repo's own rule decides which way this leans** (spec review, blocker). The corpus prices
the round trip, not the byte: "the round trip is the unit of cost, not the byte"
(`cost-and-latency.md` §2).
- The dense leg already makes this exact trade twice: sq8 rides the leg round ("int8 costs
  bytes rather than a fourth hop"), and so do IndexRows.
- No dense per-query byte ceiling exists to breach.
- ⚠️ **But bytes are not free here either** (spec review, round 2). The index's own comments
  say "storage is the cheap resource and query bytes are the scarce one" (`cluster.rs`), and
  that "bytes per query is the cost model's dominant input" (`Query::default`). The change
  raises that input by about a fifth.
- So **building ids beside codes is a cost judgement the corpus already supports, not a line
  crossed**.
- What *would* cross one is keeping four rounds by restating D-34 (three for the ranking, one
  for the results), since that moves a threshold in the weakening direction.

## Delta

1. **The harness is committed**, as `pstore-engine`'s example `id_round`, run on a current-thread
   runtime so no number depends on scheduling. It prints the full table, including:
   - an exact-path row at 20,000 rows;
   - the production backend's 1 MiB coalesce gap, **measured** through `Accounted`'s coalesced
     fetches (M6b), never estimated: the ids estimate is computed on the rabitq rows that
     gap actually fetched.
2. **The harness checks itself**:
   - its per-request log agrees with `DepthCounting`'s depth and `Accounted`'s bytes, or it
     panics;
   - it asserts the 100,000-row, p = 8 constants as integer totals over its 20 queries, never
     float means: candidates, total bytes, depth 4 and round-4 bytes. Drift is then a failure,
     not a comparison by eye.
3. **Row 26 is updated with the price and the recommendation**: build ids beside codes for the
   dense leg as its own milestone. It reaches three rounds only for the queries above, and
   the row names what stays at four.
   - Named and **unmeasured**:
     - a per-index format choice by id length, given the break-even;
     - a fixed-width ordinal beside codes, which serves dedupe and the shadow check but not
       the response, so it never reaches three;
     - ids fetched in round 3 for the nearest few lists only, with round 4 on a miss, which
       needs a hit rate;
     - a per-query or per-index opt-in.

**Not changed:** the format, the query path, D-34, and `a_query_costs_four_round_trips_however_many_segments_it_has`.

## Acceptance criteria

1. **The table is reproduced, and pinned.** `cargo run --release -p pstore-engine --example
   id_round` prints the table and passes its own assertions of the 100,000-row, p = 8
   constants. The ledger records the full table as run. ⚠️ If the run's numbers differ from
   this spec's planning table, the spec is amended, never silently replaced.
2. **The self-check was seen red.** With one request dropped from the harness's log, the run
   panics on the depth and bytes agreement. The ledger names the request dropped.
3. **Row 26 records the price and the recommendation**:
   `grep -n "priced by \[M42\]" docs/milestones/BACKLOG.md` finds the row's new text.
4. **Gates.** `./scripts/gates.sh` is green.

## Test plan

| # | Check | How |
|---|---|---|
| 1, 2 | the harness, run in release, and once with the log mutated | `cargo run --release -p pstore-engine --example id_round` |
| 3 | the row | `grep` |

⚠️ No `cargo test`: a measurement at 100,000 rows runs outside the suite, for the reason
`scripts/recall.sh` gives. The harness compiles under the gates' `--all-targets`.

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any | 0 | 0 | 0 | 0 | unchanged: no product change |

## Risks

- **`provisional`**, under the non-negotiable that numbers measured on WSL2 or against an
  emulator are `provisional`: an in-memory store counts bytes and requests exactly, and says
  nothing of latency.
- **Synthetic 36-byte ids:** real ids may be shorter, which the break-even prices.

## Tasks

- **M42.1** — The harness, its run, and its self-check seen red.
- **M42.2** — The ledger, and row 26 updated.
