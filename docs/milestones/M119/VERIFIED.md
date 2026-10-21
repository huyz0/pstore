# M119 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores. Bench numbers are release
builds, `provisional` (AGENTS.md): relative, this host only. Tests ran in the debug profile
with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`: with debug
info and incremental caches the workspace's test binaries did not fit the session's disk.

1. `./scripts/query-bench.sh` passed and printed a digest per kind, before and after.
2. Every digest after equals its baseline (`47539a4b450befe3` dense, `64d349ed03f0a2f6` text,
   `65559801f84a1412` hybrid, `b06b396d0aa759fb` dense+filter, `60cf6bfe1f9fc517`
   text+filter, `d559aff4b3101b72` dense k=100, `dd1bf0796e2ba34f` text k=100), on each of
   the change's three runs. `./scripts/query-bench.sh` is the command; NOT-RUN on macOS and Windows.
3. `./scripts/query-bench.sh` back to back with the baseline's binary (built from `f250ecf`),
   the last run of each, p50 in ms:

   | kind | before | after | ratio |
   |---|---|---|---|
   | dense | 11.35 | 3.89 | 0.34 |
   | text | 3.84 | 2.28 | 0.59 |
   | hybrid | 12.41 | 4.05 | 0.33 |
   | dense+filter | 29.01 | 13.34 | **0.46** |
   | text+filter | 18.24 | 7.47 | **0.41** |
   | dense k=100 | 19.48 | 12.11 | 0.62 |
   | text k=100 | 11.63 | 7.96 | 0.68 |

   Ingest 44,304 → 49,678 docs/s; fold 98.4 → 91.1 ms a segment. Run to run on this host,
   `dense k=100` moved from 10.3 to 13.9 ms on the same code. Before the projected mask,
   `text+filter` was 0.90 of its baseline and missed the bound: that is what found Delta 3.
4. `top::tests::top_k_matches_a_full_sort`, `top::tests::the_dense_accumulator_is_the_map`,
   `rabitq::tests::the_branchless_kernel_is_the_branchy_one`,
   `docs::tests::a_projection_keeps_exactly_what_it_names`,
   `docs::tests::a_projection_refuses_what_the_full_decode_refuses`,
   `types::tests::the_hand_written_vector_is_the_derived_one` pass. **Red first**, each by
   its mutation, restored after: the prefix sort removed; a zero-only row not recorded; the
   last partial chunk's first element skipped; a skipped bool left unchecked; the refusal's
   message changed. `a_projection_keeps_exactly_what_it_names` was not run against one here;
   it was afterwards, at M120.3: red with an unwanted id kept, and again with every
   attribute kept. The vector test also failed once on the real code, on the test's own
   enum name inside serde's message; the derive now keeps the name.
5. `each_index_keeps_its_own_fresh_segment` passes. **Red first** with the map cleared on
   every miss (the single slot): `a query of one index dropped the other's segment`.
6. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.

## The review's findings, and what became of them

Five read-only reviewers, one a crate group, then one independent reviewer of the diff.

**Fixed** (by crate): `pstore-index` — the 1-bit kernel branchless; codes scored in their
record, not copied out; no centroid-table clone a query; top-k by selection at every ranked
site; the index-row map replaced by in-place reads and a bitmap; dense BM25 and sparse
accumulators; clustering's argmin before its sort, one buffer, means row by row; a segment
build moving its documents. `pstore-format` — the term dictionary as spans; FDE signs once a
repetition; trigram dedup by set, hashing on the stack; the block-size fit measuring zones
only; zone keys cloned once a block; postings keys once a word; the sparse dictionary kept,
not copied; `rows_at` by bisection; external rows as `Bytes` slices, walked not removed; a
scan's field vectors moved. `pstore-query` — statistics for the query's terms only, each
dictionary decoded once; `fuse` by selection over a hash map; the exact scans by selection;
filter values borrowed; the segment shared by `Arc`; order-by and aggregate built only for
what they keep; the mask a bitmap, deletes a sorted vector, and the mask decoding only what
it reads. `pstore-engine` — a fresh segment an index; a hit copies no row; the HEAD cache an
`Arc`; schemas compared before copied; width checks only for a named field; `scoped` in one
buffer. `pstore-cache` — `std` mutexes; misses placed by index; admitted bytes copied out of
a coalesced fetch. `pstore-server` — `VectorIn` by hand; a multi-query's sub-queries moved; a
patch built, not parsed; documents moved into the format; one lock an eviction pass; the
external list read only to build. `pstore-meter` — ranges sliced, not copied.

**Not landed in M119, and why.** ⚠️ **Every one has since landed**, in
[M120](../M120/VERIFIED.md) `17e96bb`, under a spec naming the depth it changes and an
equivalence test for the parse; [FINDINGS.md](../M120/FINDINGS.md) is the per-finding record.
- An external column's pin checks and a multi-field scan's reads run together: each cuts
  round-trip depth, which this spec does not change. **Landed in M120** (Delta 1).
- A write body and a peer part parsed once into their type: refuses a duplicate key the tree
  accepted, and accepts a malformed number or invalid UTF-8 in an unknown field that it
  refused. Kept as a tree here, as was a single query's. **Landed in M120** behind a
  validation pass and a tree fallback (Delta 2).
- The centroid table decoded once a dense leg a segment a query. **Landed in M120**: read in
  place (Delta 3).
- `scope` (order-by, aggregate) owning its HEAD, so a cache hit copies it. **Landed in M120**
  (Delta 4).
- A fuzzy filter's characters and trigrams gathered per row and per block; `exq` and `lire`
  untouched. **Landed in M120** (Deltas 5 and 6).

The independent review's minors, fixed: the multi-vector accumulator and the dedupe bitmap
are capped, as the dense accumulator is; overlapping list mappings resolve to the last list,
as the map did. Its test gaps are criteria 4 and 5.
