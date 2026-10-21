# M119 — CPU on the request path, measured and cut

**Serves:** D-34's latency budget from the CPU side: depth is fixed by design, so what a
query costs beyond its round trips is computation, and nothing measured it. No backlog row:
a review of every shipping crate for per-row, per-candidate and per-request waste.

## What is true today

- **No benchmark measures one server's CPU.** `split-bench.sh` (M57) measures a split
  against no split; `depth.sh` counts round trips and bytes. Neither says where a query's
  milliseconds go.
- **Measured here** (4-core Linux container, release, `provisional`): over 24 segments of
  2,000 rows at 64 dimensions, a dense top-10 takes 10.3 ms p50, a text top-10 3.7 ms, and a
  filter triples a dense query (30.2 ms) and quintuples a text one (20.0 ms).
- **A review found 47 candidate defects,** each traced to a real caller. They fall in five
  kinds: a full sort where a top-k suffices; a per-row or per-candidate allocation (a code
  copied out of its buffer, an attribute cloned to be compared, a key cloned to look up a
  map entry that exists); SipHash maps keyed by dense row numbers; work repeated per query
  that depends on nothing in it (a whole vocabulary merged when BM25 reads the query's
  terms, the centroid table cloned, a sign matrix recomputed per bucket); and JSON parsed
  twice, into a tree and then into the type.

## Delta

1. **`tests/query_bench.rs` and `scripts/query-bench.sh`**: ingest, fold, and seven query
   kinds over one in-memory store, each kind printing p50, p95 and a **digest of every
   answer's ids and scores**. Not a gate: its numbers are relative.
2. **The defects, fixed crate by crate,** each behaviour-preserving to the bit: every
   replaced sort has a total comparator (`total_cmp`, then a unique id), so selecting then
   sorting the prefix returns what sorting everything did; every replaced accumulator adds
   the same terms in the same order from the same `+0.0`.
3. **A filter's mask decodes only what its predicate reads** (found by the bench after 2):
   the id when it names the id, and its own attributes; every other value is read through
   and checked as the full decode checks it, and built never.
4. **Findings rejected on verification** are listed in `VERIFIED.md` with why.

**Not changed:** any blob request, its count, its class or its depth; any wire format; any
answer, its order, or its ties; any error a caller can see. ⚠️ Amended at review: two fixes
that cut round-trip depth (an external column's pin checks, a multi-field scan's reads) and
a write body parsed once into its type (which refused duplicate keys and let a malformed
unknown field through) were taken out, not landed: each changes what this spec fixes.

## Acceptance criteria

1. **The bench runs and pins answers.** `./scripts/query-bench.sh` passes and prints a
   digest per kind.
2. **No answer changed.** Every kind's digest after the change equals its baseline digest.
3. **Faster.** After the change, the bench's filtered kinds (`dense+filter`,
   `text+filter`) are each at most 0.8 of their baseline p50, and no kind's p50 is above
   1.05 of its baseline, on the same host, baseline and change run back to back.
4. **The replaced kernels agree with what they replace.** `top_k_matches_a_full_sort`,
   `the_branchless_kernel_is_the_branchy_one`, `the_dense_accumulator_is_the_map`,
   `a_projection_keeps_exactly_what_it_names`,
   `a_projection_refuses_what_the_full_decode_refuses`,
   `the_hand_written_vector_is_the_derived_one` pass.
5. **One fresh segment an index.** `each_index_keeps_its_own_fresh_segment`: queries
   alternating between two indexes keep both cached and answer each with its own rows.
6. `./scripts/gates.sh` passes.

## Test plan

| # | Test | Fails first because | Mutation it catches |
|---|---|---|---|
| 1 | the bench | it does not exist | — |
| 2 | digests compared by hand | — | any fix that reorders a tie or changes a sum |
| 3 | p50 compared by hand | — | — (a number, not a predicate) |
| 4 | `top_k_matches_a_full_sort` | the helper does not exist | a missing prefix sort |
| 4 | `the_branchless_kernel_is_the_branchy_one` | the kernel does not exist | a dropped tail element |
| 4 | `the_dense_accumulator_is_the_map` | it does not exist | a row touched only by a zero dropped |
| 4 | `a_projection_refuses_what_the_full_decode_refuses` | it does not exist | a skipped bool left unchecked |
| 4 | `the_hand_written_vector_is_the_derived_one` | it does not exist | a refusal's message changed |
| 5 | `each_index_keeps_its_own_fresh_segment` | one slot: the other index's is dropped | the map cleared on a miss |
| 6 | gates | — | — |

## RA budget

Unchanged: no blob request is added, removed or reordered, and none changes class.

## Risks

- **A tie reordered.** Caught by the digest only where the bench has ties; the suite's
  brute-force comparisons cover the rest.
- **The bench's numbers are one host's.** Criterion 3 compares runs back to back on it.
- **A fix shifts cost rather than removing it** (a copy moved from read to admit). Named
  per finding in the ledger.

## Tasks

- **M119.1** — the bench, this spec.
- **M119.2** — the fixes and the ledger. ⚠️ Amended from five commits, one a crate: the
  crates' fixes depend on each other (`pstore-query` uses `pstore-index`'s `top` and
  `pstore-format`'s projection), and one gate run costs about forty minutes on this host.
