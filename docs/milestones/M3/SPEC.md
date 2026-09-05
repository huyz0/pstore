# M3 — Vector index

**Serves:** D-8 (SPANN-family clustered index), D-9 (centroids are the hottest object),
D-10 (small indexes scan exactly), D-11 (RaBitQ + int8 rerank ladder), D-12 (recall is a
client knob), D-34 (depth is a tested invariant), D-35 (recall is a CI gate).
Opens evidence on OQ-36, OQ-38, OQ-39, OQ-40.

**Exit condition** (roadmap): *90–95% recall@10 at ≤3 round trips cold, measured, on 100M
vectors.*

⚠️ **The 100M part cannot be met here and this spec does not pretend otherwise.** The
environment is WSL2 with no cloud account, by standing constraint. Every number below is
`provisional`, carries its scale and dimension, and the gap is carried as work for the
milestone that has real storage — not as a task, because it is blocked on M0b.

## Delta

**Adds**
- `pstore-index` — quantization and the clustered index. The Layer-3 slot the workspace
  root already reserves. **One crate, not the roadmap's `pstore-quant` +
  `pstore-index-vec`**: the standing constraint is "modularized but not too many", and a
  quantizer with no index to serve is not independently useful. Split it when something
  else needs the codes; sparse impact scoring is the likely trigger.
- Segment **sections** for `rabitq` (1-bit) and `sq8` (int8) codes, plus a **centroid**
  section, all addressed from the footer.
- **Byte accounting per section** in `pstore-blob`, alongside the existing request counter.
  Criteria 4 and 7 are unassertable without it: today nothing records how many bytes a
  query moved, or which section they came from.
- `scripts/recall.sh` — the D-35 gate. Runs **outside `cargo test`**, because a 250k-vector
  build inside the suite would be re-run once per mutant by `cargo mutants`. Budget: ≤5
  minutes wall clock.

**Does not add**
- **Hand-written SIMD.** ⚠️ Correcting the reason, which was wrong in the first draft of
  this spec: `unsafe_code = "forbid"` is a lint over *our* crates, so a third-party SIMD
  dependency needs no `pstore-kernel`. The real reason is that `simsimd` is a new
  dependency whose value is unmeasured here — nothing in this milestone is throughput-bound
  at 250k vectors. ⚠️ This **contradicts `quantization.md`'s "SIMD is mandatory"**, which
  is a statement about production, not about a correctness gate. Deferred to the milestone
  that has a throughput budget.
- **Incremental maintenance** (LIRE/SPFresh). That is the roadmap's *milestone* "M3.5",
  referred to here as **M3-LIRE** so it cannot be confused with a task id, and OQ-51.
- **PQ or any per-index training.** D-11 rejects it; the reason has not changed.
- Sparse vectors and BM25. `modalities-and-sequencing.md` puts sparse first, in M5.
- Datasets fetched over the network. `evaluation-methodology.md` already says the
  multi-tenant regime has no public dataset and must be built. The harness accepts a real
  one; the gate does not require it.

## The numbers this milestone is pinned to

Stated here so no criterion can be satisfied by choosing them afterwards.

| Name | Value | Why this value |
|---|---|---|
| Exact-scan threshold | **25,000 vectors** | Below D-10's ~50k–200k range, which is stated for 128d; higher dimensions scan more bytes per vector. Evidence for OQ-36. |
| Gate dataset | **250,000 × 384d** | 10× the threshold, so the gate cannot silently measure brute force. 384d is a real embedding width (bge-small, Matryoshka-truncated) inside D-11's target family — **not** 128d, which `vector-index-survey.md` names as SPANN's weak spot. |
| Default `p` | **16** posting lists | Mid-range of the corpus's 8–64. |
| Default oversample | **8×** | Mid-range of the corpus's 4–32. |
| Posting list target | **4,000** vectors | `vector-index-survey.md` sizing; ≈192 KB of 1-bit codes at 384d. |
| Recall floor | **recall@10 ≥ 0.90** | The low end of the roadmap's 90–95%. |
| Byte ceiling | **≤8 MB fetched per query** at the defaults | Probing all 63 lists costs ~12 MB, so the ceiling binds against "probe everything". |
| Balance bound | no list > **4× the mean** | On a dataset with 10:1 density skew. |
| Bound confidence | **δ = 1e-3**, 384d, fixed seed | RaBitQ's bound is probabilistic; asserted on the empirical failure rate over ≥10,000 pairs, not per-pair. |

## Acceptance criteria

1. **Quantization needs no training**: encoding depends only on the vector, the dimension
   and a fixed global seed — two independently constructed quantizers produce identical
   bytes for the same input.
2. **RaBitQ's error bound holds** at the δ, dimension and seed above: the measured failure
   rate over ≥10,000 pairs is below δ, and a deliberately wrong estimator breaches it.
3. **int8 rerank is strictly more accurate than 1-bit** on the same pairs: lower mean
   relative error at rung 1 than rung 0, over ≥1,000 pairs.
4. **A `rerank: none` query fetches zero bytes from the `sq8` and full-precision sections**,
   asserted by the per-section byte counter.
5. **Recall@10 ≥ 0.90 *and* ≤8 MB fetched**, at the stated defaults, on the stated dataset,
   against exact brute force. **One criterion, not two** — recall bought with unbounded
   bytes is not recall (`evaluation-methodology.md`).
6. **Depth, measured from `HEAD` and not from an already-open index:** `rerank: none` and
   `rerank: fast` cost **≤3 sequential** blob rounds; `rerank: exact` costs **≤4**, and that
   fourth round is what the client is buying. ⚠️ This is the one place the milestone spends
   more than the three-round budget, and it does so only on explicit request.
7. **`p` is free in depth**: probing 64 lists costs the same sequential depth as 8, and more
   bytes — both asserted.
8. **The rerank knob is real** (D-12): `none | fast | exact` give non-decreasing recall and
   non-decreasing bytes on the same query set.
9. **Boundary augmentation earns its size**: at a deliberately small `p` (2), recall with
   augmentation exceeds recall without it by a stated margin on the same index.
10. **Query-aware pruning earns its complexity**: an easy query (near a centroid) fetches
    strictly fewer bytes than a hard one (equidistant between centroids), same `p`.
11. **D-10 holds and is the only switch**: below 25,000 vectors an index answers by brute
    force with recall exactly 1.0 and writes no centroid or posting-list object; above it,
    the clustered path is used with no silent fallback.
12. **Clustering is balanced and lossless**: no posting list exceeds 4× the mean on 10:1
    skewed data, and every vector is reachable from at least one list.
13. Region coverage ≥95% on shipped crates, mutation ≥80%, full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `two_quantizers_encode_identically` | per-index state smuggled into the encoder |
| 2 | `the_measured_failure_rate_is_below_delta` | a bound asserted but never exercised |
| 2 | `a_deliberately_wrong_estimator_breaches_the_bound` | a bound so loose it holds for anything |
| 3 | `int8_rerank_beats_one_bit_on_the_same_pairs` | a rerank rung that does not rerank |
| 4 | `rerank_none_reads_no_sq8_or_float_bytes` | sections merged, so rung 0 pays for precision it discards |
| 5 | `recall_at_ten_meets_the_floor_inside_the_byte_ceiling` | recall regression, and recall bought with unbounded bytes |
| 6 | `a_cold_ann_query_from_head_costs_three_rounds` | a data-dependent chain creeping into search |
| 6 | `exact_rerank_costs_exactly_one_more_round` | an unadvertised fourth round on the default path |
| 7 | `probing_more_lists_costs_bytes_not_depth` | posting lists fetched in a loop |
| 8 | `the_rerank_knob_trades_bytes_for_recall_monotonically` | a knob wired to nothing |
| 9 | `augmentation_lifts_recall_at_a_small_probe_count` | augmentation written but never consulted |
| 10 | `an_easy_query_fetches_less_than_a_hard_one` | pruning that prunes nothing |
| 11 | `a_small_index_answers_exactly_and_builds_no_index` | building an ANN index nobody needs |
| 11 | `the_threshold_is_the_only_thing_that_switches_paths` | a silent fallback hiding a broken ANN path |
| 12 | `clustering_stays_balanced_on_skewed_data` | k-means collapsing onto dense regions |
| 12 | `every_vector_is_reachable_when_all_lists_are_probed` | vectors dropped at assignment, visible only as recall |

## RA budget

Measured **from `HEAD`**, because that is what a client experiences. M1 already spends
≤2 rounds opening an index and a segment, so the query path has one round left; the
centroid section rides with the footer-addressed index section (D-9), and rungs 0 and 1
read different byte ranges of the **same** posting-list objects in the **same** round.

| Operation | Budget |
|---|---|
| Cold, `rerank: none` or `fast` | HEAD+manifest, open+centroids, `p` lists = **3 depth** |
| Cold, `rerank: exact` | + 1 Rpar for float32 = **4 depth**, client-requested only |
| Warm (centroids and index section cached) | **1 depth** |
| Small index (exact) | **1 Rpar** = 1 depth beyond open |
| Build of *n* vectors | *n* Rpar in, **1 W** per segment, 0 LIST |

## Risks

- **Synthetic data flatters clustering.** A Gaussian mixture is what clustering is best at,
  so the gate would measure the generator. Mitigated by also running a uniform cloud (no
  structure to find) and a 10:1 skewed mixture, and by reporting every number with its
  dataset.
- **The scale gap is the main threat to the exit condition.** 250k is ~400× below 100M, and
  the failure modes that appear at scale — centroid table size, posting-list skew, boundary
  effects — may not appear here at all. Nothing in this milestone closes that.
- **The floor may be tuned instead of met.** It moves only in the strengthening direction,
  and the dataset seed is fixed so a better number cannot come from a luckier draw.
- **The recall floor does not transfer across dimensions.** It is measured at 384d. A 768d
  point is reported *without* a floor, as evidence for OQ-39/OQ-41.
- **The error bound may not hold as published.** That is a finding about RaBitQ worth more
  than the milestone, and criterion 2 is written to detect it rather than confirm it.

## Tasks

| ID | Task |
|---|---|
| M3.1 | `pstore-index`: RaBitQ 1-bit encode + asymmetric int4 query, bound tested |
| M3.2 | Per-section byte accounting in `pstore-blob` |
| M3.3 | int8 scalar quantization and the rerank ladder |
| M3.4 | Segment sections: `rabitq`, `sq8`, `centroids`, footer-addressed |
| M3.5 | Balanced clustering: centroid selection, assignment, balance bound |
| M3.6 | Boundary augmentation |
| M3.7 | Posting-list layout; centroids ride with the index section |
| M3.8 | ANN search: probe, `p` lists in parallel, rerank ladder, query-aware pruning |
| M3.9 | The D-10 threshold as the single switch |
| M3.10 | Recall harness and `scripts/recall.sh` as a gate |
| M3.11 | Depth and byte assertions for cold, warm and each rerank rung |

**Carried, not a task:** measuring at ≥10M vectors needs real storage and is blocked on
M0b. It belongs to that milestone, and listing it here as a commit would be a task nobody
can start.
