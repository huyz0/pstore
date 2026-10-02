# M27 — A query asks for no centroid table it knows is not there

**Serves:** [BACKLOG](../BACKLOG.md) row 46, and D-10, which reads a missing centroid table as
"scan me exactly".

## What is true today

- A segment has a centroid table (`….seg.cen`) only when the fold or compaction that sealed it
  held at least `exact_scan_threshold` rows (25 000), with a dense vector of non-zero width
  (`try_build_all_with`).
- Every dense query asks for every segment's table in its open round, beside the footer
  (`pstore_query::run::open`). Below the threshold the answer is a 404, which no cache can
  keep. That is a request per small segment on every dense query, warm or cold, and most of a
  warm query's cost for most tenants.
- HEAD records each segment's row count (`SegmentRef.rows`), the count it was sealed with by a
  fold, compaction, branch or replica copy. A query has it before the open round.
- ⚠️ **Except `as_of`** (spec review B1): a past manifest's resurrected refs carry `rows: 0`
  (`head.rs`), and those are mostly since-compacted segments, which are the large ones.
- M21's warm already uses that count to skip tables that cannot exist.

## Delta

**A segment whose recorded row count is known, and below this engine's `exact_scan_threshold`,
is opened without asking for its centroid table.** No format change.

- `pstore_query::Target.centroids` becomes `Option<Key>`. `None` means "this segment has none",
  and the open round asks for nothing.
- The engine sets it, for each segment HEAD names, from one predicate shared with M21's warm:
  `may_have_centroids(rows, threshold)` = `rows == 0 || rows >= threshold`.
  - ⚠️ **No width term on the query side** (spec review round 2). On `as_of` over a dropped
    index, the present HEAD has no schema, so a width read from it would be 0 for segments that
    have tables. Warm keeps its own `dims > 0` skip beside the shared predicate, since it reads
    only the present HEAD.
  - **`rows == 0` is unknown**, not small: a fold or compaction never records an empty segment,
    and `as_of`'s resurrected refs carry 0. So the table is asked for, as today.
- The fresh segment of unfolded rows gets `None` when its own build made no table.
- **Why not the row's footer flag:** it changes the format, helps only segments sealed after it,
  and still costs a read to learn. HEAD's count costs nothing, and covers every segment that
  exists.

**Correctness does not depend on the threshold matching.** If a reader's threshold were higher
than the writer's, it would skip a table that exists and scan that segment exactly, which D-10
already allows and which ranks at least as well. If it were lower, it asks, and gets the 404
it gets today. `exact_scan_threshold` is a constant in production, so neither happens there.

**Not changed:**
- the sparse and text dictionaries, which are still asked for when a leg wants them, beside the
  footer that would say whether they exist (a separate cost, BACKLOG row 51 below);
- the format, HEAD, the fold, compaction, and every API field. `meta.cost.blob_reads` falls by
  the reads not made: it reports what the query cost.

**Existing tests** (spec review M1):
- `pstore-server` `read_cache.rs` pins a warm vector query at `1 + segments` reads, citing row
  46. It becomes `1`, in the strengthening direction, and is criterion 5's server-side red test.
- `pstore-query`'s tests that build a `Target` pass `Some(centroid_key(..))`.
- `warm.rs`'s loop over `.cen` reads asserts over none once warm reads no 404. It keeps its
  meaning, and M21 test 3's mutation is checked again.

## Acceptance criteria

1. **No 404 for a table that cannot exist.** A dense query over 8 small segments makes 0
   requests for a `.cen` key. On the parent commit it makes 8.
2. **A table that exists is still used.** A dense query over a segment at the threshold reads
   its table, and probes as today: its reads and bytes are equal to the parent commit's.
3. **Answers are unchanged.** For dense, hybrid and filtered queries over small and large
   segments, the ranked ids and scores equal the parent commit's.
4. **`as_of` is unchanged.** An `as_of` dense query at an epoch before a large segment was
   compacted away reads that segment's table, and so does one over an index dropped since.
   Its ids and scores equal the parent commit's.
5. **A reader with a higher threshold still answers correctly.** It answers a large segment by
   exact scan, the true top-k by brute force, with no `.cen` read.
6. **Cheaper by exactly the 404s.** A warm dense query over 8 small segments makes exactly 8
   fewer requests than on the parent commit, at the same depth. `read_cache.rs`'s warm vector
   query reports 1 read.
7. **Warm agrees.** M21's warm still asks for no table it cannot have, through the shared
   predicate. `warm.rs` is unchanged and passes.
8. **Gates.** `./scripts/gates.sh` is green, and the mutation sweep over M27's source diff misses
   0, every miss closed by a test or named as equivalent.

## Test plan

Engine tests in `crates/pstore-engine/tests/centroid_skip.rs`, over a store that counts requests
per key suffix. The "parent commit" numbers are measured on that commit, as M23 and M25 did,
and pinned.

| # | Test | Must fail first because / mutation it catches |
|---|---|---|
| 1 | `small_segments_ask_for_no_centroid_table` | today: one `.cen` read per segment |
| 2 | `a_segment_at_the_threshold_still_uses_its_table` | `<` as `<=`; always `None` |
| 3 | `answers_are_unchanged` | a table skipped where it existed and an ANN answer expected |
| 4 | `an_as_of_query_still_uses_a_resurrected_table`, `an_as_of_query_over_a_dropped_index_uses_its_tables` | `rows == 0` read as small; a width term on the query side |
| 5 | `a_higher_threshold_reader_scans_exactly` | today: it reads `.cen`; a skipped table treated as an error |
| 6 | `the_saving_is_exactly_the_404s`, server `read_cache.rs` | an extra read added; depth changed |
| 7 | `warm.rs` (existing) | `>=` as `>` in the shared predicate; warm's `dims > 0` dropped |

## RA budget

| Op | W | Rseq | Rpar | List | Depth |
|---|---|---|---|---|---|
| Any query with a dense leg (dense, hybrid, filtered), `n` small segments | 0 | unchanged | **−n** | 0 | unchanged |
| The same, a large segment | 0 | unchanged | unchanged | 0 | unchanged |
| `as_of`: a resurrected ref (its count unknown) | 0 | unchanged | unchanged | 0 | unchanged |
| `as_of`: a live ref, small | 0 | unchanged | −1 each | 0 | unchanged |

## Risks

- **A threshold that differs between processes** costs the skipping reader an exact scan of a
  large segment. That is correct, and dearer in bytes. `with_index_params` lets a deployment
  tune it, and the server sets no other value. A deployment that tunes it should tune every
  process alike, or HEAD should record the writer's threshold.

## Tasks

- **M27.1** — `Target.centroids` optional, the shared predicate, the engine's targets, and tests.
- **M27.2** — The ledger, `BACKLOG.md` row 46 closed, row 51 opened for the dictionaries' 404s,
  and the roadmap row.
