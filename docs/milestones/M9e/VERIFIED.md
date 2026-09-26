# M9e — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **The order** — `rank_by_orders_integers_then_strings_then_absent`
   (`cargo test -p pstore-server --test order`): integers, strings (`Zebra` before `apple`,
   bytewise), a partly absent attribute and ties, `asc` and `desc`, half folded and half not,
   then all folded, then `as_of` -- **observed red** on the M9d server, with the rest of the
   file. The comparator alone: `the_order_is_the_one_written_down`
   (`cargo test -p pstore-query --lib order`).
2. **Filter, then `offset`, across segments** —
   `filters_apply_before_offset_and_offset_spans_every_segment`: two segments and unfolded
   rows, a filter admitting half, `offset: 4`.
3. **Paging by id** — `paging_by_id_visits_every_document_once_while_the_index_changes` (an
   upsert, a delete, a new id and folds between pages, through the API) and
   `paging_by_id_survives_a_compaction_between_pages` (`cargo test -p pstore-engine --test order`;
   no route compacts, so the engine test is where a merge lands between pages, with a delete,
   an upsert whose new value is asserted, and a new id beyond the cursor).
4. **Deleted and superseded rows** — folded, in each of the three tests above (a deleted id
   absent, an upserted id once, as its new version); unfolded,
   `unfolded_deletes_and_upserts_hide_the_folded_rows` -- ⚠️ added at code review, which found
   every other test folded first, so dropping the shadow survived them all; now seen red with
   it dropped. The same test pins an index only unfolded deletes touched as `404`, as a
   relevance query answers it (it was `200 []`; code review).
5. **Depth** — `an_ordered_query_is_three_round_trips_however_many_segments`: 3 at 1 and at 8
   segments, with a delete vector, with and without an unfolded row, and with `as_of`; requests
   exactly `2 + 2 × segments` (HEAD, each suffix and the one delete vector, one block plan per
   segment -- code review tightened it from a bound with slack).
6. **The selector** — `the_selector_holds_at_most_its_cap_and_equals_a_sort_truncated` (500
   pseudo-random rows, both directions, caps 0 to 600, the bound asserted after every offer)
   and `ordering_by_id_is_bytewise`.
7. **Refusals** — `rank_by_refuses_what_it_cannot_mean` (ten shapes, the 10,000 bound at and
   past its edge; a relevance query still carries `score`); `score` and `$dist` absent from a
   `rank_by` row in criterion 1's test.
8. **Gates** — the mutation sweep over the diff after code review's fixes,
   `./scripts/mutants.sh --check . --in-diff <the M9e diff>`: **60 tested in 26m, 34 caught,
   18 unviable, 7 missed, 1 timeout**, each resolved in `order.rs`: `Ranked`'s equality pinned
   by `a_ranked_rows_equality_is_its_ranks`; the selector's strict eviction by
   `among_equal_ranks_the_first_offered_is_kept`; `len` made test-only and asserted exactly
   (`the_selector_holds_exactly_what_it_was_offered_up_to_its_cap`), `is_empty` deleted; and the
   timeout -- a filter that stopped applying, which returned the first page forever -- turned
   into a failure by bounding both paging loops. Re-swept `--check order --file
   crates/pstore-query/src/order.rs`: **28 tested, 22 caught, 6 unviable, 0 missed**.
