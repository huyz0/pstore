# M1 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it. This **enumerates**; it does not certify the evidence is true beyond what
is recorded here, and a line naming something not actually run would violate the "never
claim a gate ran without running it" non-negotiable.

Gate: `scripts/check-verified.py`.

1. `a_segment_round_trips_through_the_blob_store`, `a_segment_is_readable_from_its_key_alone`,
   `an_empty_segment_is_legal`, `a_corrupt_footer_is_an_error_not_a_wrong_answer`.
   `cargo test -p pstore-format --test segment`. Identity is asserted, not just row count:
   the vector and both attribute types of a specific row are compared, because dropping a
   block or losing a field still yields a plausible answer.
2. `opening_a_cold_segment_costs_at_most_two_reads`, `a_small_segment_opens_in_one_read`,
   `a_segment_whose_index_section_overflows_the_suffix_takes_two_reads`.
   `cargo test -p pstore-format --test segment`. Asserted through the request counter.
   The third exists because until it was written the two-read branch was never taken —
   earlier "big segment" cases had few enough blocks that the index section still arrived
   with the footer.
3. `zone_maps_skip_blocks_that_cannot_match`, `pruning_does_not_change_the_answer`,
   `a_string_filter_cannot_prune_but_still_filters`,
   `a_filter_on_an_unknown_column_matches_nothing_and_prunes_nothing`,
   `filters_prune_only_what_they_provably_can`.
   `cargo test -p pstore-format --test search`. The saving is asserted through the counter
   and the *answer* is asserted against an unpruned reference, because the dangerous
   mutation is not pruning too little — that is slow — but pruning a block that could
   match, which silently drops rows.
4. `exact_search_matches_a_brute_force_reference`,
   `search_returns_k_results_when_k_exceeds_the_segment`, `search_respects_a_filter`,
   `search_on_a_dimension_mismatch_is_an_error`.
   `cargo test -p pstore-format --test search`. The reference is computed in the test from
   the inputs, never from the segment: a self-consistent wrong answer is exactly what a
   weaker test would miss.
5. `a_write_batch_costs_exactly_one_put`, `a_write_batch_is_one_round_trip_and_one_request`.
   `cargo test -p pstore-engine`. Batches of 1, 10 and 1000 each cost one PUT.
6. `many_indexes_share_one_bundle` (fifty indexes, one PUT),
   `a_bundle_reader_gets_only_its_own_index`.
   `cargo test -p pstore-engine --test engine`.
7. `concurrent_committers_produce_a_dense_epoch_sequence` (8 writers × 4 commits → 32
   unique, gapless epochs, and all 32 documents survive), `a_stale_committer_is_fenced`,
   `folding_twice_does_not_duplicate_rows`.
   `cargo test -p pstore-engine --test engine`. Mutation verified killed by construction:
   the non-atomic `get` + `get_tag` pair produced 28 unique epochs of 32, and the four
   duplicates were lost updates.
8. `a_write_is_visible_before_it_is_folded`, `a_folded_write_is_still_visible_exactly_once`,
   `writes_after_a_fold_are_visible_alongside_folded_ones`,
   `a_fresh_read_costs_no_blob_requests_beyond_head`,
   `a_fresh_engine_recovers_acknowledged_writes_from_the_wal`.
   `cargo test -p pstore-engine`.
9. `a_cold_read_has_a_sequential_depth_of_at_most_three`,
   `a_cold_filtered_read_has_a_sequential_depth_of_at_most_three`,
   `a_cold_search_has_a_sequential_depth_of_at_most_three`,
   `depth_does_not_grow_with_the_number_of_segments`,
   `the_whole_flow_stays_inside_its_request_budget`, `nothing_on_the_read_path_ever_lists`.
   `cargo test -p pstore-engine`. The counter is itself tested
   (`a_loop_that_awaits_each_result_counts_every_iteration`, `concurrent_fan_out_counts_once`)
   because a naive request counter would pass while measuring the wrong thing.

   ⚠️ **CORRECTED in M3.** This criterion was met only at fixture scale. The test used 500
   rows, which is 8 blocks, whose index section fits the 8 KiB suffix read — so the segment
   opened in **one** round and the invariant held for a reason the test never stated. At
   40,000 rows the index section no longer fits, the open costs a second round, and a cold
   scan measures **4**. The claim above was true of what ran and false of the system.
   Fixed by bounding the index section by construction (`SegmentWriter` grows its block
   size until the index fits the budget), and the invariant is now asserted directly by
   `the_block_index_stays_inside_the_suffix_read_at_any_size` as well as by
   `a_cold_read_of_a_large_segment_still_costs_three_rounds`, both in
   `cargo test -p pstore-engine --test depth_at_scale`.

10. `./scripts/gates.sh` — all eight green. **150 tests.**
    `cargo llvm-cov --workspace --all-features` → **96.10% lines** (the CI gate,
    `--fail-under-lines 95`, passes) and **94.19% regions**.
    ⚠️ **Region coverage is 0.8 points under the 95% floor.** Shipped crates alone are
    94.87% regions / 97.62% lines; the residue is defensive error-formatting arms in
    `reader.rs` and `conformance.rs`, reachable only from backends failing in specific
    ways. Writing tests purely to reach them would be the coverage-driven test writing the
    standards forbid, so it is recorded as a shortfall rather than papered over.
    `cargo mutants --workspace --all-features` → **364 caught of 439 viable = 82.9%**
    (544 mutants, 3 min). **Above the 80% target**, and up from 76.8% at M0a — the
    conformance suite's `Broken` backend closed most of the gap M0a.11 recorded.
    ⚠️ Same precision point as M0a: `cargo mutants` injects *its own* mutations, so
    "mutation verified killed" above means the named mutation was **reasoned about**
    except where a test demonstrably produced it — criterion 7's lost update did, because
    the bug was real and the assertion caught it.

## What was found rather than built

M1's value was the bugs, all four of them invisible functionally and each caught by a test
written before the code:

- **A lost update in the commit protocol.** `head::read` did `get()` then `get_tag()` — two
  calls — so another committer could land between them and leave us holding the *old* bytes
  with the *new* tag. The CAS then succeeded and silently overwrote a commit we never saw.
  Found by asserting a **dense** epoch sequence rather than merely a successful one: 28
  unique epochs of 32. Fixed by adding `get_with_tag` to the trait, atomic by construction.
- **A data leak in the fold.** Every index was folded into one segment object, so each
  index's `SegmentRef` pointed at the whole thing and scanning one index returned its
  neighbours' rows. One segment per index now.
- **The WAL was write-only.** `fold()` replayed the in-memory copy, so bundles were paid for
  and never read — and a restarted process could not recover a single acknowledged write.
  **Coverage found it**: `bundle.rs` sat at 33% because `read_index` was never called.
- **Optimistic concurrency livelocked.** It surfaced as a *flaky* test, which is the more
  expensive way to find out. Every loser retried immediately, collided with the same peers,
  and lost again. Jittered backoff derived from the lane id; verified over ten runs.

And two latency bugs the depth counter caught the moment it existed, both functionally
invisible because the answer is identical either way:

- `get_ranges` awaited each coalesced fetch in turn.
- `Engine::scan` opened segments one at a time. **Ten segments cost 21 sequential hops** —
  measured, not estimated — which is six times the whole latency budget.

Plus a **panic** in the bundle decoder: a tampered index offset of `u64::MAX` overflowed an
addition rather than returning an error. A crash vector reachable from a malformed object,
which is the one input a storage layer must assume is hostile.

The trait also gained `get_suffix` (`Range: bytes=-N`), because `Segment::open` was calling
`head()` first — billed as a read, doubling every cold open, and putting a HEAD on the hot
path the design forbids.

## Exit condition

> *Write, read, filter, exact-search a single index. Measured `RA(write) = 1 W` and
> `RA(cold read) ≤ 3 Rseq`, asserted in tests.*

**Met**, with the adaptation stated in the spec: "on real S3" became "through the same
`BlobStore` trait against the in-process store", because there are no cloud accounts.
Request counts and round-trip depth are backend-independent and are genuinely measured;
latency is not, and is not claimed.

## Carried forward

| ID | Task |
|---|---|
| M1.12 | Region coverage 94.19% vs the 95% floor; decide whether `pstore-testkit` belongs inside the gate at all, since it never ships |
| M1.13 | `foyer` cache tier, deferred from this milestone — it buys latency, which M1 does not claim |
| M1.14 | Lane tail discovery: a successor's flush watermark is injected (`replay_for_test`) rather than discovered by forward probing and the lane bitmap. M2. |
