# M5f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Commands: `cargo test -p pstore-query --test fusion_identity` and
`--test multi_segment`.

⚠️ **Two of the four load-bearing mutations survived their first fixture**, and both are
recorded here rather than quietly fixed. A fixture that cannot fail is the failure this
project's spec preamble warns about, and it caught this milestone twice.

1. **Rows from different segments never merge** — `two_segments_with_the_same_row_are_two_hits`
   and `ties_break_on_the_pair_and_the_segment_comes_first`. Observed red by keying the map on
   `(0, row)` (both fail) and by leaving the tie-break on `row` alone (the second fails).
   ⚠️ This is not a hypothetical: `fuse` shipped keyed on `row`, and `Hit`'s own doc stated the
   limit — *"a stated limit rather than an oversight"* — while nothing enforced it. Two
   unrelated documents summed into one hit, ranked above either, every leg correct.
2. **Global statistics, computed once** — `global_statistics_change_the_ranking_across_segments`:
   two segments' ranked document ids equal the same documents' ids in one segment, compared by
   **id** because the layouts number rows differently.
   ⚠️ **Observed red only after the fixture was rebuilt.** Scoring each segment against
   `idx.summary()` instead of the merged statistics **passed** the first fixture, whose halves
   disagreed about `df` alone: RRF over a single leg reproduces that leg's order, and
   per-segment idf changed the scores without reordering the union enough to notice. The
   rebuilt fixture disagrees about `avgdl` as well — 40 two-term documents against 40
   twenty-term ones — so the global order puts a short segment-A document first and segment B's
   single hit **last**, and the per-segment order puts B's **first**. Both positions asserted.
3. **Fusion happens once, over the union** — `fusion_happens_once_over_the_union`: two
   retrievers over two segments, and the fused scores are not all equal. Fusing per segment and
   merging gives every segment's top hit the same `1/(k+1)`, because RRF is blind to score
   magnitude by design — so a ten-segment index would surface ten equal "winners".
4. **Each retriever's union is re-ranked before it is fused** —
   `each_retrievers_union_is_ranked_before_it_is_fused`. Observed red by removing the re-sort.
   ⚠️ **This test exists because the mutation survived everything else.** `fuse` reads a leg's
   position as its rank, and with one retriever the fused order *is* the leg's input order — so
   an unsorted concatenation ranks segment 0's hits above segment 1's and nothing downstream
   repairs it. Invisible on the other three fixtures, all of which put the good hits in segment
   A. The fixture that can tell puts the **best** hit in segment B: one single-term document
   against twenty two-term ones, so length normalisation ranks it first.
5. **Depth is 2 over N segments** —
   `four_segments_cost_one_open_round_and_one_leg_round`: `DepthCounting` at N = 4 gives 2,
   **and equal to the same query's depth over one segment**, which is the form that does not
   depend on the fixture. Observed red by replacing the `try_join_all` over `open` with a
   sequential loop — the identical answer at four times the depth, which is the failure
   `Engine::scan`'s comment already measured as "a twenty-one-hop query".
6. **A missing term dictionary fails the query** —
   `a_missing_term_dictionary_is_an_error_not_a_smaller_corpus`.
   ⚠️ **OBSERVED-NOT, and the finding is that no new code is needed.** Spec review round two
   argued that `open`'s `maybe` shape — which correctly swallows a missing *centroid* table,
   because D-10 reads that as "scan me exactly" — would drop a segment out of `doc_count` and
   every `df` if carried to term dictionaries. A guard was written for it. **No mutation of
   that guard could be caught**: the segment's own text leg already refuses on the same missing
   sidecar and `try_join_all` fails the query with it, so no wrong answer is reachable. The
   guard was removed rather than kept as code nothing constrains, and the test pins the
   property wherever it is enforced.
7. **Zero LIST** — `a_multi_segment_query_does_not_list`, on `Accounted`.
8. **One segment is unchanged** — `a_one_segment_query_is_unchanged` (every hit carries
   `segment == 0`), plus the existing suites: `hybrid` 11, `fusion` 5, `text_field` 3, all
   green with no change beyond `Hit`'s new field and the paired `Target`. `scripts/ndcg.sh`
   **NDCG@10 1.0000, MRR 1.0000** against floors of 0.80 / 0.75 with the control at 0.5441;
   `scripts/depth.sh` **depth 1 + 1**, 6,358 and 2,024 bytes exact at 20,000 rows.
9. **Gates** — `./scripts/gates.sh` green. Coverage: `pstore-query/src/fuse.rs` **100%**
   regions, lines and functions; `run.rs` **96.47%** regions, **100%** functions; workspace
   **95.42%** via `./scripts/coverage.sh --fail-under-regions 95`.

## What is not built, and named rather than omitted

- ⚠️ **`Engine::query`**, and the blocker is not identity. `Engine::seal` writes no centroid
  table, so a folded segment carries no ANN index at all and `Engine::search` is exact brute
  force over a full `scan`. Building the index at fold time is its own milestone. Until then
  the multi-segment path is tested and has no production caller — the same position `Metered`
  landed in, stated for the same reason.
- **A durable identity.** `(segment, row)` is stable for the query's HEAD snapshot, which is
  all fusion needs; a compaction moves rows. Nothing needs a durable one: there are no deletes
  or updates by id, and `Engine::search` already returns `Document.id`.
- **Resolving hits to documents.** A block fetch per segment, already outside `query`'s budget
  for one segment and unchanged for N.
- **A bound on N.** Ten segments is ~30 requests in the open round. This milestone makes many
  segments **shallow**, not cheap; bounding the count is compaction's existing job.
