# M1 — Single-node storage engine

**Serves:** D-34 (round-trip depth is a tested invariant), the write path (lanes, bundles,
`RA(write) = 1 W`), the commit protocol and Invariant I1, the segment footer contract
(Pattern 6), and D-10 (exact scan serves most indexes, so no ANN yet).

**Exit condition** (roadmap): *write, read, filter, exact-search a single index. Measured
`RA(write) = 1 W` and `RA(cold read) ≤ 3 Rseq`, asserted in tests.*

⚠️ **Adapted.** The roadmap says "on real S3". There are no cloud accounts, so the exit is
demonstrated against the in-process store and `object_store::InMemory` through the same
`BlobStore` trait. Request *counts* and round-trip *depth* are backend-independent and are
therefore genuinely measured; latency is not, and is not claimed. Real S3 is M0b.

## Delta

**Adds**
- `pstore-format` — an immutable segment: data blocks, an index section, and a footer at a
  known suffix offset so one `Range: -N` GET bootstraps the whole object.
- `pstore-engine` — HEAD and the CAS commit protocol, WAL lanes carrying cross-index
  bundles, the in-memory memtable that makes a write visible before it is folded, and the
  fold that turns bundles into segments.

**Does not add** — each with a reason, not an omission:
- **The `foyer` cache tier.** It buys latency, and this milestone's exit is about request
  counts and correctness. A disk cache measured on WSL2 would produce provisional numbers
  and no correctness signal. Deferred to M4, where warm-path latency first matters.
- **ANN.** D-10: exact scan serves the majority of real indexes, and it is the honest
  baseline the approximate path is later measured against.
- **Quantization, compaction, delete vectors, multi-node anything.** M3, M2, M4.

## Acceptance criteria

1. A segment round-trips: documents written into it come back identical, and a segment is
   readable knowing only its key — no side table, no listing.
2. Opening a cold segment costs **at most 2 sequential** blob reads: one suffix range for
   the footer, one for the index section. Reading any block after that adds one.
3. Zone maps prune: a filtered scan reads **strictly fewer** blocks than the same scan with
   the filter removed, and returns exactly the rows the filter selects.
4. Exact vector search returns the true top-k, verified against an independent brute-force
   reference over the same data, for random queries.
5. A write batch costs **exactly 1 W** whatever it contains, asserted through the request
   counter.
6. Writes for many indexes inside one window are carried by **one** bundle object, so the
   PUT count scales with windows rather than with indexes.
7. The commit protocol is linearizable: with concurrent committers every commit is either
   applied or rebased, epochs are strictly increasing with no gaps, and a committer holding
   a stale tag is fenced out.
8. A `durable`-acked write is visible to a query **before** it is folded into a segment.
9. An end-to-end cold read — open the index, plan, fetch — has a **sequential blob depth of
   at most 3**, asserted by a depth counter rather than by inspection.
10. Region coverage ≥95%, mutation score ≥80% on the new crates, and the full gate set
    green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `a_segment_round_trips_through_the_blob_store` | dropping the last block, or losing a field |
| 1 | `a_segment_is_readable_from_its_key_alone` | needing a side table, which reintroduces a lookup |
| 2 | `opening_a_cold_segment_costs_two_reads` | reading the whole object, or a third round trip |
| 2 | `a_small_segment_opens_in_one_read` | never using the suffix's spare bytes |
| 3 | `zone_maps_skip_blocks_that_cannot_match` | never skipping (no saving) |
| 3 | `pruning_does_not_change_the_answer` | skipping a block that *could* match |
| 4 | `exact_search_matches_a_brute_force_reference` | a wrong distance, or an off-by-one top-k |
| 4 | `search_returns_k_results_when_k_exceeds_the_segment` | panicking or truncating silently |
| 5 | `a_write_batch_costs_exactly_one_put` | one PUT per document |
| 6 | `many_indexes_share_one_bundle` | one bundle per index, restoring the per-index floor |
| 6 | `a_bundle_reader_gets_only_its_own_index` | returning another index's rows |
| 7 | `concurrent_committers_produce_a_dense_epoch_sequence` | a lost update, or an epoch gap |
| 7 | `a_stale_committer_is_fenced` | accepting a commit built on a superseded epoch |
| 8 | `a_write_is_visible_before_it_is_folded` | visibility waiting on the fold |
| 8 | `a_folded_write_is_still_visible_exactly_once` | double-counting across memtable and segment |
| 9 | `a_cold_read_has_a_sequential_depth_of_at_most_three` | any added sequential fetch |

## RA budget

| Operation | Budget |
|---|---|
| Write batch (any size, any index count in the window) | **1 W** |
| Open index, cold | ≤2 Rseq (HEAD, then manifest) |
| Open segment, cold | ≤2 Rseq (footer, then index section) |
| Cold read, end to end | **≤3 Rseq**, fan-out unbounded within a round |
| Warm read | 0 |
| LIST, anywhere | **0** |

## Risks

- **The footer contract may not survive a real segment.** Revealed by making criterion 2 a
  counter assertion rather than a design claim.
- **The memtable and the segment may double-count a folded write.** The hazard is a
  document visible twice, which reads as a duplicate rather than as an error. Criterion 8's
  second test exists for this.
- **Depth counting may measure the wrong thing.** A concurrent fan-out issued as a
  sequential loop would pass a naive counter. The counter must record *depth*, not count.

## Tasks

| ID | Task |
|---|---|
| M1.1 | `pstore-format`: footer, block directory, encode/decode round trip |
| M1.2 | Zone maps and block pruning |
| M1.3 | Exact vector search over a segment |
| M1.4 | Segment reader: open-from-key, ≤2 Rseq |
| M1.5 | `pstore-engine`: HEAD, epochs, the CAS commit protocol |
| M1.6 | WAL lanes and cross-index bundles |
| M1.7 | Memtable: visible before fold |
| M1.8 | Fold: bundles → segment, exactly-once visibility |
| M1.9 | Depth-counting store, and the ≤3 Rseq invariant test |
| M1.10 | End-to-end: write, read, filter, exact-search one index |
