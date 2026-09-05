# M3 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it. This **enumerates**; it does not certify the evidence is true beyond what
is recorded here.

Gate: `scripts/check-verified.py`.

⚠️ **Every number here is `provisional`**: WSL2, synthetic corpora, and three orders of
magnitude below the 100M vectors the roadmap's exit condition names. See "What this
milestone does not show".

1. `two_quantizers_encode_identically`, `encoding_is_stable_across_dimensions`.
   `cargo test -p pstore-index --test rabitq`. Two independently constructed quantizers
   produce identical bytes: any per-index state — a sampled codebook, a fitted rotation —
   would show up as a difference, and would be an operational cost multiplied by the
   tenant count.
2. `the_measured_failure_rate_is_below_delta`,
   `a_deliberately_wrong_estimator_breaches_the_bound`,
   `the_rotation_is_randomised_not_merely_orthogonal`,
   `the_alignment_is_measured_per_vector_not_assumed`.
   `cargo test -p pstore-index --test rabitq`. δ = 1e-3 at 384d, fixed seed, asserted on
   the empirical failure rate over 5,000 pairs swept across query angles.
   ⚠️ `BOUND_EPSILON` began at 2.25 from a half-remembered constant and measured a 2.6%
   failure rate — exactly what a 2.25σ two-sided bound does. The test did not catch a bug
   in the estimator; it caught a bound that had never been derived. It is now
   `sqrt(2·ln 2000) ≈ 3.90`, with the arithmetic beside the number.
   ⚠️ Two of these tests were **verified unable to fail** before being kept: deleting the
   de-biasing division left everything green, because the failure-rate test used only
   orthogonal pairs where a 20% *multiplicative* bias is invisible. The "wrong estimator"
   test could never have caught it — it compares against a parallel implementation.
3. `int8_rerank_beats_one_bit_on_the_same_pairs`,
   `int8_reconstructs_within_one_quantisation_step`.
   `cargo test -p pstore-index --test ladder`. int8's mean error is ~25× lower.
   ⚠️ The reconstruction bound is asserted at **half** a step, not a whole one: the loose
   bound is also satisfied by truncation, which biases every coordinate down by half a step
   and mostly cancels in an inner product against a zero-mean query.
4. `a_rung_zero_query_reads_no_int8_or_float_bytes`
   (`cargo test -p pstore-index --test persisted`),
   `a_rung_zero_scan_reads_no_full_precision_or_int8_bytes`
   (`cargo test -p pstore-index --test rung0`). Asserted with the per-section byte counter
   added in M3.2, through the real search path.
5. `scripts/recall.sh` → **clustered, n=20,000, dim=384, 100 lists, p=16, oversample=32,
   `rerank: fast`: recall@10 = 0.9810**, 3,440 candidates, **0.25 MB** of rung-0 bytes
   against the 8 MB ceiling. Smoke test in the suite:
   `the_clustered_index_meets_the_recall_floor` (`cargo test -p pstore-index --test recall`).
   ⚠️ The **uniform** corpus is reported without a floor: **0.7380**. Clustering cannot help
   where there is no structure to find; that is the D-10 argument, not a defect.
6. `a_cold_query_from_head_costs_three_round_trips`,
   `a_clustered_query_costs_two_round_trips_beyond_head`,
   `exact_rerank_costs_exactly_one_more_round`.
   `cargo test -p pstore-index --test persisted`. HEAD, then {footer ∥ centroids}, then the
   probed lists with **both** code sections in one call. Verified sensitive: serialising the
   footer and centroid fetches takes it to 4.
   ⚠️ **Not measured on the gate dataset**, and not end-to-end through a query layer,
   because the layer that joins the engine to the index (`pstore-query`, layer 4) does not
   exist in this milestone. The test composes them itself, which `pstore-index` may do
   because the dependency runs downward. The depth is structural — it does not vary with
   row count — but that is an argument, not a measurement.
7. `probing_more_lists_costs_bytes_not_depth`.
   `cargo test -p pstore-index --test persisted`. p=64 against p=4: more bytes, identical
   depth. ⚠️ Uses `MemoryStore::with_coalesce_gap(256)`, because at fixture size the default
   64 KiB gap merges every posting list into one fetch and the test would measure the
   coalescer. At production sizing a list is tens of kilobytes and the gap does not reach
   across one.
8. `the_rerank_knob_buys_recall_and_not_only_bytes`
   (`cargo test -p pstore-index --test persisted`), `the_ladder_only_helps`,
   `a_higher_rung_never_returns_a_worse_top_k`
   (`cargo test -p pstore-index --test recall`).
   ⚠️ This criterion existed and was **unguarded** until mutation testing: deleting the sq8
   fetch outright left every other test green, because the depth tests measure what a rung
   *costs* and the rung-0 byte test uses `none`. That gap is how the persisted int8 scale
   bug (below) survived.
9. `augmentation_lifts_recall_by_five_points_at_p_two`.
   `cargo test -p pstore-index --test recall`. Parameters chosen from a sweep, not guessed:
   1 replica at boundary 0.05 gives **+9.2 points at p=2** for 1.72× index size, and
   **nothing at p=8**. Worth keeping because at equal recall it moves *fewer* bytes per
   query — ~344 candidates against ~800 — and query bytes are the scarce resource.
10. `an_easy_query_prunes_more_than_a_hard_one`.
    `cargo test -p pstore-index --test recall`. A query on a centroid keeps fewer lists than
    one equidistant between two, and pruning never returns an empty set.
11. `a_small_index_answers_exactly_and_builds_no_index`,
    `the_threshold_is_the_only_thing_that_switches_paths`,
    `the_default_threshold_is_the_stated_number`,
    `a_corrupt_centroid_table_is_refused_not_guessed`.
    `cargo test -p pstore-index --test persisted`. Below 25,000 rows no centroid object is
    written **at all** — not an empty one — so "is this clustered?" is answered by whether
    the object exists. The exact path reproduces brute force exactly.
    ⚠️ The switch is exercised at a lowered threshold so the suite stays fast (322s → 1.7s);
    the stated 25,000 is pinned by its own test so that convenience cannot become the value.
12. `clustering_stays_balanced_on_skewed_data`,
    `every_vector_is_reachable_from_at_least_one_list`,
    `balance_is_bought_cheaply_not_at_any_price`, `a_centroid_is_the_mean_of_what_it_holds`,
    `the_same_corpus_clusters_the_same_way`.
    `cargo test -p pstore-index --test clustering`. Without the cap the largest list is 6.0×
    the mean on 10:1 skewed data.
    ⚠️ `balance_is_bought_cheaply` was **vacuous when written** — on an interleaved fixture
    the cap never binds and the ratio was exactly 1.0000. It now uses grouped arrival.
13. `./scripts/coverage.sh --fail-under-regions 95` → **95.03% region**, 96.95% line on
    shipped crates, exit 0. `cargo llvm-cov --workspace --fail-under-lines 95` → passes.
    `cargo mutants --workspace` — figure in the notes below. `./scripts/gates.sh` → green.

## Corrections this milestone made to the corpus

- **[C-3](../../research/06-indexing/quantization.md)** — D-11 claims rung 0 alone reaches
  90–95% recall@10. Measured at 384d it reaches **0.30**, flat from p=8 to p=32, and no
  oversample can help because with `rerank: none` the answer *is* rung 0's top-k. int8
  rerank reaches 0.981 **in the same three round trips**. The default rerank mode is now
  `fast`; the budget still closes.

## What this milestone does not show

- **The exit condition.** The roadmap asks for 90–95% recall@10 at ≤3 round trips **on 100M
  vectors**. Recall and depth are met at **20,000 × 384d**. 100M × 768d is ~300 GB and the
  environment is WSL2 with no cloud account; brute-force ground truth alone would exceed the
  gate's budget by orders of magnitude. **This part is NOT-RUN**, and it is blocked on M0b
  rather than on effort.
- **That the failure modes of scale are absent.** Centroid table size, posting-list skew and
  boundary effects at 1M centroids may not appear at 100 lists at all.
- **Anything on real embeddings.** Both corpora are synthetic. The clustered one is a
  Gaussian mixture, which is what clustering is *best* at; its tightness was itself corrected
  after an earlier version produced within-group cosine 0.97 — 500 near-copies of one vector,
  which no quantizer can rank and which made recall look like an algorithm failure.
- **Hand-written SIMD.** Deferred with a stated reason, contradicting `quantization.md`'s
  "SIMD is mandatory" — which is a claim about production throughput, not about a
  correctness gate.
