# M5a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it. Gate: `scripts/check-verified.py`.

⚠️ **Landed as two commits, not the four the task table lists**, and the spec says why:
M5a.1/M5a.2 are one property (a format that can hold a sparse field while the fold cannot
write one exists only to satisfy a task boundary), and M5a.3/M5a.4 are one property (the
measurement selects the default the retriever ships with).

1. **A sparse field round-trips**, dimensions exactly and impacts within `max_impact / 254` —
   `a_sparse_field_round_trips`, `a_posting_list_holds_every_row_carrying_its_dimension_and_no_other`,
   `rows_in_a_list_are_ascending` (`cargo test -p pstore-format --test sparse`). ⚠️ The bound is
   against the **term's** largest magnitude, not the posting's own: a bound written the other
   way passes for the largest weight in every list and fails for the smallest.
2. **A sparse document is stored with its postings and refused without them** —
   `a_sparse_field_without_its_postings_is_refused`, and
   `a_writer_refuses_only_what_it_still_cannot_store` (`--test vector_fields`), which was
   observed failing against M3b's blanket refusal before the arm was narrowed.
3. **A sparse field has a layout row** — `a_sparse_field_has_a_layout_row`, which also asserts
   the row addresses the postings section and that the span it names is the postings' length.
4. **A dense query is unaffected by a sparse field beside it** —
   `a_sparse_field_does_not_displace_the_dense_one` (`--test sparse`) and
   `a_dense_query_is_unaffected_by_the_sparse_field_beside_it`
   (`cargo test -p pstore-query --test hybrid`), whose fixture names the sparse field so it
   **sorts first**. Without the dense-only field index the dense leg returns zero rows and
   nothing reports an error.
5. **The dictionary scales with distinct dimensions** —
   `the_dictionary_scales_with_terms_not_documents`: 10× the rows over a vocabulary the
   fixture *covers by construction* leaves it byte-identical, and the postings grow 5×.
6. **A sparse field survives write → fold → compaction** —
   `a_sparse_field_survives_write_fold_and_compaction` (`cargo test -p pstore-engine --test
   sparse`), asserted after the fold *and* after the merge so a failure names the stage.
   Observed red against the pre-M5a.2 engine. Supporting:
   `a_folded_segment_has_its_dictionary_beside_it`,
   `a_reaped_sparse_segment_takes_its_dictionary_with_it` (observed red with the GC change
   reverted), `a_dense_only_index_writes_no_sidecar_and_costs_nothing_extra`.
7. **Candidates are exact, as a set** — `the_candidate_set_is_exact`
   (`cargo test -p pstore-index --test sparse`), 200 queries over 2,000 rows against a
   brute-force oracle.
8. **f32 impacts rank exactly like brute force** — `f32_impacts_rank_exactly_like_brute_force`,
   same 200 queries, order and scores.
9. **Depth is exactly 2 beyond the open, 3 from `HEAD`** — at gate scale (20,000 rows,
   30,000 dimensions) by `./scripts/depth.sh`, and as a shape by
   `a_sparse_query_costs_two_round_trips_beyond_head` in the suite. ⚠️ **Split after M5**:
   `cargo mutants` reruns the suite once per mutant, so a gate-scale fixture inside it costs
   hours across a sweep — the rule `recall.sh` already stated and this milestone ignored. ⚠️ **Renamed from the spec's `a_sparse_query_costs_three_rounds_from_head`**:
   HEAD is the engine's round and is not in this crate, so the test measures the two rounds
   that are, exactly as M3's equivalent does. Drift recorded rather than hidden.
10. **One round beyond open whatever the term count** —
    `the_postings_fetch_is_one_round_whatever_the_term_count`, a query of every dimension the
    fixture's 200 sample queries mention (>100 terms), at `coalesce_gap = 256`. `List == 0`
    asserted by `a_query_fetches_only_its_own_lists`.
11. **Bytes are ≤1.2× the query's own lists** — `./scripts/depth.sh` at gate scale
    (**6,358 bytes moved against 6,358 in the query's own lists**) and
    `a_query_fetches_only_its_own_lists` as a shape, scoped to the `SparsePostings` span via
    `Accounted::bytes_in`. ⚠️ Observed red: replacing the
    per-entry ranges with the whole span fails this and two others.
12. **An unknown dimension costs nothing** — `an_unknown_dimension_costs_nothing`: an absent
    term does not change the answer, and a query of only absent terms returns empty with the
    read counter unchanged.
13. **OQ-126 answered, both halves** — `impact_encodings_are_measured`. Measured on the gate
    corpus and the pinned 200 queries:

    | encoding | postings bytes | top-10 overlap vs f32 | top-1 |
    |---|---|---|---|
    | u8 | 1,780,379 | 0.9910 | 1.0000 |
    | f16 | 2,420,379 | 1.0000 | 1.0000 |
    | f32 | 3,700,379 | — | — |

    u8 is **2.08× smaller** than f32 and clears the floor pinned in advance (0.95 / 0.90), so
    `DEFAULT_ENCODING` stays u8 — selected by the measurement, not assumed by it. The test
    asserts the floor **and** that the constant still matches what cleared it.
14. **Gates** — `./scripts/gates.sh` green (8/8); `./scripts/coverage.sh --fail-under-regions 95`
    at **95.28% regions**, 97.09% lines, 95.45% functions; `cargo deny check` clean; mutation
    recorded below.

## What was measured and is not a claim about production

⚠️ The dictionary sidecar is **720,015 bytes** for 30,000 terms — **88×** `INDEX_BUDGET`, not
the 44× the spec estimated at 12 bytes an entry. The shipped entry is 24 bytes, because a list
needs its byte length *and* its posting count *and* its scale. The C-10 argument survives its
own arithmetic being off by two, which is why it is recorded as measured.

⚠️ Everything here runs against `MemoryStore` on WSL2. Byte counts and request counts are
exact; **no latency number is claimed**, and none is measured.

## Named, not built

- **Block-max pruning (OQ-45).** The postings are doc-ordered and sparse search is exact, so
  there is no top-k bound to prune against yet. Bytes, not depth, is the sparse failure mode:
  a 1,000-term query fetches 1,000 lists in one round, and criterion 11 bounds the over-read,
  not the query's appetite.
- **A second sparse field per segment.** Refused at the door rather than silently written as
  nothing; it needs a `FieldSparse*` id pair mirroring `FieldVectors`.
- **Cross-segment fusion.** Legs fuse by row within one segment. A stable cross-segment id is
  full-text's two-pass IDF problem and is deliberately not invented twice.
