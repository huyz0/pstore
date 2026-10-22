# M119–M120 — every review finding, and what became of it

The five read-only reviews M119 ran (one a crate group), each finding by its number in its
review, with the commit that closed it and the evidence. **Fixed** means landed and covered
by the gate run on that commit; a row naming a test names one that passes, and the ledgers
record the mutation each went red under first, where one was run. The bench digests
([M119](../M119/VERIFIED.md) criterion 2) held across every commit below.

Commits: M119.2 `7a94c49`, M120 `17e96bb`, M120.2 `4893a5e`, M120.3 `57351e3`, M120.4
`4bb3ef2` (this ledger's count), M120.5 `85c64a3` (engine 7's last sub-point, and the tests
the appendix found missing), M120.6 `69ed65f` (format 10's last sub-point), M120.7 `4b6f8d8`
(a test for engine 8, and the bench at five runs), and M120.8 (tests for query 6 and engine
6, the two rows the suite ran without constraining: hand mutation found both of engine 6's
conditions surviving the whole engine and server suites). Each commit's gate run is the whole workspace's suite at that
commit, so the last run covers every test named below at once.

**The count.** The reviews numbered **47** findings: 10 index, 10 format, 9 query, 9 engine
and cache, 9 server. **All 47 are fixed, every sub-point included; none is rejected.** Format
10's per-row key was the last: it needed `Document.vectors` keyed by a shared name, which
M120.6 made it. Three rows marked `—` are not among the 47: two
found later (the spikes, the projected mask) and fixed, and one the review itself declined.

## `pstore-index`

| # | Finding | Outcome |
|---|---|---|
| 1 | branchy, bounds-checked 1-bit kernel | Fixed, M119.2: `signed_sum`; `the_branchless_kernel_is_the_branchy_one` |
| 2 | centroid table cloned and fully sorted a query; re-decoded a query | Fixed: no clone and top-k probe in M119.2; read in place in M120 (`the_table_in_place_is_the_decoded_one`) |
| 3 | a `Vec` a candidate in `read_code` | Fixed, M119.2: `estimate_residual_raw`, `estimate_prepared_raw` |
| 4 | full sorts for top-k at seven sites | Fixed, M119.2: `top::top_k_by` at each; `top_k_matches_a_full_sort` |
| 5 | SipHash maps over every probed row | Fixed, M119.2: mappings read in place, a bitmap dedupe |
| 6 | SipHash BM25 and sparse accumulators | Fixed, M119.2: `top::Accumulator`; `the_dense_accumulator_is_the_map` |
| 7 | clustering sorts every centroid a row, 12 times | Fixed, M119.2: argmin first, one buffer, `augment` by selection |
| 8 | centroid means column by column | Fixed: `cluster` in M119.2, `lire` in M120 |
| 9 | a build copies every document twice | Fixed, M119.2: rows moved into the writer, fitted vectors reused |
| 10 | a `BTreeMap` accumulator in the multi-vector search | Fixed, M119.2: a dense vector, capped as review asked |
| — | `exq`, `lire` carry 1 and 8 | Fixed, M120 |

## `pstore-format`

| # | Finding | Outcome |
|---|---|---|
| 1 | a `String` a term in `TermDict::decode` | Fixed, M119.2: one blob and spans |
| 2 | FDE signs drawn per bucket; planes drawn per document | Fixed: signs once a repetition in M119.2; both kept a thread in M120.3 (`the_draws_kept_are_the_draws_made`) |
| 3 | quadratic trigram dedupe, a `String` a hash; trigrams per block | Fixed: set and stack bytes in M119.2; a fuzzy needle carries its own in M120 |
| 4 | the block-size fit re-encodes every row; body grown by doubling | Fixed, M119.2: zones measured alone, rows sealed once; capacity reserved |
| 5 | a zone key cloned a row | Fixed, M119.2 |
| 6 | a posting key cloned a row | Fixed, M119.2 |
| 7 | external rows as `Vec`s, removed one by one | Fixed, M119.2: `Bytes` slices, walked |
| 8 | the sparse dictionary copied a query | Fixed, M119.2: `decode_bytes` |
| 9 | `rows_at` scans blocks linearly, files every row, clones hits | Fixed: bisection and asked rows only in M119.2; moved, not cloned, in M120.3 (`a_row_asked_twice_comes_back_twice`) |
| 10 | a scan copies field vectors, reads fields one after another; names each row's field anew | Fixed: moved in M119.2; read together in M120 (`a_multi_field_scan_reads_its_fields_together`); `Document.vectors` keyed by `Field`, one shared name, in M120.6 (`a_scan_names_its_field_once`, red with a name made a row; `a_field_is_its_name`) |

## `pstore-query`

| # | Finding | Outcome |
|---|---|---|
| 1 | every segment's whole vocabulary merged a text query | Fixed, M119.2: `summary_for` the query's terms |
| 2 | each term dictionary decoded once a text leg and again for statistics | Fixed, M119.2: once at open |
| 3 | `fuse` sorts every row | Fixed, M119.2: selection over a hash map |
| 4 | exact scans sort every row | Fixed, M119.2 |
| 5 | the filter clones a value a row a leaf | Fixed, M119.2: borrowed |
| 6 | `Segment` deep-cloned a segment a query | Fixed, M119.2: `Arc` |
| 7 | the order-by selector builds a rank before rejecting | Fixed, M119.2 |
| 8 | the aggregator searches twice and clones an `Arc` a row | Fixed, M119.2 |
| 9 | SipHash mask and delete checks a candidate | Fixed, M119.2: a bitmap, a sorted vector |
| — | the mask decodes every row whole (found by the bench) | Fixed, M119.2: projected decode; both `a_projection_*` tests |

## `pstore-engine`, `pstore-blob`, `pstore-cache`

| # | Finding | Outcome |
|---|---|---|
| 1 | one fresh segment a tenant | Fixed, M119.2: one an index; `each_index_keeps_its_own_fresh_segment` |
| 2 | every unfolded row cloned a query, hit or miss | Fixed, M119.2: copied on a miss only; shadow shared |
| 3 | a bounded hit clones HEAD under the mutex | Fixed: `Arc` in M119.2; `scope` borrows in M120 |
| 4 | a coalesced slice pins its whole fetch in the cache | Fixed, M119.2: admitted as a copy |
| 5 | every write rescans every held row | Fixed, M119.2: only for a named field |
| 6 | schemas cloned a query | Fixed, M119.2: compared first |
| 7 | misses placed in O(misses × ranges); a lock a range; a key `String` a range | Fixed: by index in M119.2; one lock a batch in M120.3; the key an `Arc<str>` shared by every range of a call in M120.5 (`the_disk_hash_is_the_encodings` pins the encoding unchanged) |
| 8 | a tokio mutex for a synchronous section | Fixed, M119.2: `std`, poison recovered |
| 9 | `scoped` allocates a `String` a byte | Fixed, M119.2 |

## `pstore-server`, `pstore-node`, `pstore-catalog`, `pstore-gossip`, `pstore-meter`

| # | Finding | Outcome |
|---|---|---|
| 1 | a write body parsed into a tree, then the type | Fixed, M120: once where the tree agrees; `the_fast_parse_is_the_tree_parse` |
| 2 | a peer part the same | Fixed, M120 and M120.2: `the_fast_part_is_the_tree_part` |
| 3 | a query the same; a multi-query clones each sub-query | Fixed: moved in M119.2; once in M120; a multi-query told apart first in M120.2 |
| 4 | an untagged `VectorIn` buffers every vector | Fixed, M119.2: by hand; `the_hand_written_vector_is_the_derived_one` |
| 5 | a patch through two JSON copies; rows and cells cloned | Fixed: built directly in M119.2; moved out of the request in M120.3 |
| 6 | `to_document` copies every vector | Fixed, M119.2: by value |
| 7 | an eviction pass locks once an engine | Fixed, M119.2 |
| 8 | the external list copied a request | Fixed, M119.2 |
| 9 | the meter copies every range | Fixed, M119.2: sliced |
| — | `swim::dest_for` scans members per datagram | Not a finding: the review checked it and judged it fine, at about 50 µs for 10,000 members, a few times a tick |

The two independent reviews' findings are in [M119's](../M119/VERIFIED.md) and
[this milestone's](VERIFIED.md) ledgers: the first's two majors are M120's Deltas 1 and 2,
its minors fixed in M119.2; the second's minors are M120.2.

## Appendix — the tests over each row's path

Each row's path is executed by these, among others; `=` marks a test that compares against
brute force, a reference implementation or the code it replaced, so a wrong value on that
path fails it. Found by a read-only sweep of the suite; each name resolves.

| Row | Tests |
|---|---|
| index 1, 3 | `the_branchless_kernel_is_the_branchy_one` =, `every_rung_zero_score_is_the_rows_code_against_its_own_lists_centroid` =, `probing_every_list_is_the_flat_search` = |
| index 2 | `the_table_in_place_is_the_decoded_one` =, `probing_every_list_reaches_every_candidate`, `a_corrupt_centroid_table_is_refused_not_guessed` |
| index 4 | `top_k_matches_a_full_sort` =, `the_exact_rung_reproduces_brute_force_on_what_it_is_given` =, `sq8_probe_orders_and_scores_as_f32` = |
| index 5 | `a_replicated_document_appears_once_in_a_top_k` =, `augmentation_lifts_recall_by_five_points_at_p_two` |
| index 6 | `the_dense_accumulator_is_the_map` =, `bm25_matches_an_independent_implementation` =, `f32_impacts_rank_exactly_like_brute_force` = |
| index 7 | `the_assignment_cost_is_the_squared_distance_to_the_assigned_centroid` =, `a_full_list_takes_no_more_rows`, `two_lists_are_enough_to_replicate_between` |
| index 8, spikes | `a_centroid_is_the_mean_of_what_it_holds` =, `maintenance_is_deterministic`, `exq_residual_estimate_is_unbiased` |
| index 9 | `every_int8_score_is_the_rows_own_int8_estimate` =, `a_ragged_dimension_does_not_shift_every_later_row` |
| index 10 | `a_vector_query_scores_the_probed_lists_exactly` =, `clustered_search_finds_what_the_flat_scan_finds` = |
| format 1 | `a_dictionary_of_terms_is_searchable_and_refuses_nonsense` =, `a_dictionary_from_another_format_or_an_unsorted_one_is_refused` |
| format 2 | `the_draws_kept_are_the_draws_made` =, `the_encoding_is_frozen` = |
| format 3 | `the_needle_is_the_strings` =, `a_trigram_sets_the_bits_fnv_1a_names` =, `a_fuzzy_match_at_distance_k_is_never_pruned` |
| format 4, 5 | `pruning_does_not_change_the_answer` =, `a_segment_that_fits_keeps_its_zone_maps`, `mixed_ints_and_floats_prune_soundly_with_zone_maps` =, `a_segment_with_datetime_zones_prunes_soundly` = |
| format 6 | `the_dictionary_carries_document_frequency` =, `bm25_matches_an_independent_implementation` = |
| format 7 | `read_rows_returns_the_files_floats` =, `a_filter_on_the_files_own_column_equals_the_exact_filtered_cosine` = |
| format 8 | `f32_impacts_rank_exactly_like_brute_force` =, `a_corrupt_dictionary_is_refused_not_guessed` |
| format 9 | `a_row_asked_twice_comes_back_twice` =, `rows_at_returns_each_rows_id_and_attributes_in_one_round` = |
| format 10 | `a_multi_field_scan_reads_its_fields_together` =, `a_scan_still_returns_whole_documents` =, `a_scan_names_its_field_once`, `a_field_is_its_name` = |
| query 1, 2 | `global_statistics_change_the_ranking_across_segments` =, `a_missing_term_dictionary_is_an_error_not_a_smaller_corpus` |
| query 3 | `rrf_is_the_reciprocal_rank_sum` =, `ties_break_on_the_pair_and_the_segment_comes_first` = |
| query 4 | `multi_scores_by_maxsim` =, `fde_scores_are_the_encodings_dot_and_shadowed_rows_are_replaced` = |
| query 5, mask | `mixed_ints_and_floats_prune_soundly_with_zone_maps` =, `a_projection_keeps_exactly_what_it_names` =, `a_projection_refuses_what_the_full_decode_refuses` = |
| query 6 | `an_index_shares_the_segment_it_is_given` (M120.8, red with the segment copied), `four_segments_cost_one_open_round_and_one_leg_round`, `a_hybrid_query_opens_the_segment_once` |
| query 7 | `the_selector_holds_at_most_its_cap_and_equals_a_sort_truncated` =, `rank_by_orders_integers_then_strings_then_absent` = |
| query 8 | `it_holds_at_most_top_k_groups_and_keeps_the_smallest_exactly` =, `counts_and_sums_equal_brute_force` =, `groups_equal_brute_force` = |
| query 9 | `a_multi_leg_is_widened_past_deleted_rows`, `text_and_sparse_legs_are_widened_past_deleted_rows_too`, `unfolded_deletes_and_upserts_hide_the_folded_rows` |
| engine 1, 2 | `each_index_keeps_its_own_fresh_segment`, `a_query_on_one_index_is_not_served_another_indexs_fresh_rows` =, `a_cached_fresh_segment_is_rebuilt_when_the_text_field_is_filled` |
| engine 3 | `a_hit_skips_head_and_says_how_stale_it_is` =, `a_hit_this_engine_has_pruned_past_is_read_fresh` |
| engine 4 | `a_cached_range_owns_its_bytes` (M120.5, red with the slice admitted), `a_cached_range_equals_what_the_store_holds` = |
| engine 5 | `the_schema_outranks_a_held_width_at_the_door`, `two_writers_widths_meet_in_the_fold` |
| engine 6 | `the_schema_cache_keeps_the_newest_heads` (M120.8, red with an unchanged HEAD's epoch not recorded), `a_cancelled_replica_is_forgotten_by_the_door` (M120.8, red with replica names not refreshed), `the_write_door_uses_the_schema_the_process_has_read` |
| engine 7 | `a_repeated_range_within_one_call_fills_both_positions` =, `eviction_follows_a_model_lru` =, `the_disk_hash_is_the_encodings` = |
| engine 8 | `a_poisoned_arena_still_serves` = (M120.7, red with the poison not recovered -- the one value the change could alter: a `std` mutex can be poisoned and a tokio one cannot), `concurrent_misses_issue_one_request`, `eviction_follows_a_model_lru` = |
| engine 9 | `only_a_scoped_segment_key_unscopes` =, `a_branch_equals_its_source_and_then_diverges` |
| server 1, 3 | `the_fast_parse_is_the_tree_parse` =, `a_multi_query_answers_each_as_it_would_alone` = |
| server 2 | `the_fast_part_is_the_tree_part` = (red with the phase dropped, and with no fallback), `a_split_query_answers_exactly_as_one_server_does` = |
| server 4 | `the_hand_written_vector_is_the_derived_one` =, `multi_vectors_are_refused_at_the_door` |
| server 5 | `a_patch_merges_into_the_folded_row` =, `a_patch_by_filter_merges_into_exactly_what_it_admits` = |
| server 6 | `the_api_searches_a_named_field`, `text_is_written_queried_and_fused_with_the_vector`, `multi_vector_documents_are_searched_by_maxsim` |
| server 7 | `a_requested_or_backing_off_tenant_is_kept`, `the_least_recently_used_goes_first` |
| server 8 | `the_deployment_opts_in_by_name_and_names_its_prefixes`, `an_allowed_source_is_registered_queried_and_dropped_by_name` |
| server 9 | `the_metered_slices_are_the_stores_bytes` = (M120.5, red with a slice at the wrong offset), `the_meter_and_the_accountant_agree_on_a_coalesced_fan_out` |

## The bench, at five runs

`./scripts/query-bench.sh`'s binary built from `f250ecf` (the code of `e7ea269`, before M119, with the bench) and from `69ed65f`, five
runs alternating on one host, p50 in ms, median and range. The digests are equal in every run.
**The slowest run after is faster than the fastest run before on every kind**, so the gain is
not this host's ±15% run-to-run noise.

| kind | before | after | median ratio | slowest after / fastest before |
|---|---|---|---|---|
| dense | 9.58 (9.23–9.96) | 3.28 (3.00–3.54) | 0.34 | 0.38 |
| text | 3.72 (3.33–4.07) | 1.79 (1.70–1.96) | 0.48 | 0.59 |
| hybrid | 10.86 (10.57–11.29) | 3.89 (3.43–4.41) | 0.36 | 0.42 |
| dense+filter | 22.29 (20.47–23.42) | 11.96 (11.67–12.66) | 0.54 | 0.62 |
| text+filter | 15.47 (14.90–17.53) | 6.43 (6.28–7.55) | 0.42 | 0.51 |
| dense k=100 | 17.40 (16.82–18.38) | 10.05 (9.47–10.32) | 0.58 | 0.61 |
| text k=100 | 9.27 (8.75–9.72) | 6.75 (6.33–7.26) | 0.73 | 0.83 |

`provisional` (AGENTS.md): one 4-core Linux container. NOT-RUN on other hosts.
