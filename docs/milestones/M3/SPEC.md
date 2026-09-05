# M3 — Vector index

**Serves:** D-8 (SPANN-family clustered index), D-10 (small indexes scan exactly), D-11
(RaBitQ 1-bit + int8 rerank ladder), D-12 (recall is a client knob), D-35 (recall is a CI
gate). Opens evidence on OQ-39/OQ-40 and OQ-2.

**Exit condition** (roadmap): *90–95% recall@10 at ≤3 round trips cold, measured, on 100M
vectors.*

⚠️ **The 100M part cannot be met here and this spec does not pretend otherwise.** 100M ×
768d is ~300 GB of float32; the environment is WSL2 with no cloud account, by standing
constraint. Recall and depth are measured at a scale that runs in CI, the scale is stated
with every number, and the gap is carried as a task for the milestone that has real
storage. A number measured on WSL2 is `provisional` — non-negotiable, and it applies to
every figure in this milestone.

## Delta

**Adds**
- `pstore-vec` — quantization and the clustered index. **One crate, not the roadmap's two**
  (`pstore-quant` + `pstore-index-vec`): the standing constraint is "modularized but not too
  many", the two would share every type across a boundary neither side defends, and a
  quantizer with no index to serve is not independently useful. Split it when something
  else needs the codes — sparse impact scoring is the likely trigger.
- Segment **sections** for quantized codes: `rabitq` (1-bit) and `sq8` (int8), addressed
  from the footer, so a rung-0 scan never fetches a full-precision vector.
- A **centroid section** and posting lists: hierarchical balanced clustering with boundary
  augmentation, and query-aware pruning at search time.
- `scripts/recall.sh` — the recall gate (D-35), reporting recall@k *with* its scale, its
  dataset, and its cache state (D-30: never a number without those).

**Does not add**
- **Hand-written SIMD.** It needs `unsafe`, which by non-negotiable lives only in
  `pstore-kernel`, a crate that does not exist. Creating it to hold one dot product is a
  worse trade than letting the autovectorizer work. Deferred with the kernel crate.
- **Incremental maintenance** (LIRE/SPFresh). That is M3.5 and OQ-51; segments here are
  built once and replaced by compaction.
- **PQ, or any per-index training.** D-11 rejects it, and the reason (millions of tenants)
  has not changed.
- Sparse vectors and BM25. M5/M6, and `modalities-and-sequencing.md` says sparse first.
- Real datasets fetched over the network. The harness accepts one; the gate does not
  require one. See Risks.

## Acceptance criteria

1. **Quantization needs no training.** Encoding a vector depends on nothing but the vector,
   the dimension, and a fixed global seed — asserted by encoding the same vector through
   two independently constructed quantizers and comparing bytes.
2. **RaBitQ's error bound holds.** Over ≥10,000 random query/vector pairs, the estimated
   inner product is within the construction's stated bound of the true one, and the bound
   is asserted as a number, not as "close enough".
3. **int8 rerank is strictly more accurate than 1-bit** on the same data: mean relative
   error at rung 1 is lower than at rung 0, over ≥1,000 pairs.
4. **A rung-0 scan fetches no full-precision bytes.** Asserted by the request-class byte
   counter, not by inspection.
5. **Recall@10 ≥ 0.90** on ≥200,000 clustered vectors at 128d, at the default `p` and
   oversample, against exact brute force as ground truth. Asserted by `scripts/recall.sh`
   as a gate that fails below the floor.
6. **A cold ANN query costs ≤3 sequential round trips**, and **≤2 with centroids cached**,
   asserted by the depth counter.
7. **`p` is free in depth.** Probing 64 posting lists costs the same sequential depth as
   probing 8, and more bytes — both asserted.
8. **D-10 holds automatically.** Below the exact-scan threshold an index answers by brute
   force with recall exactly 1.0, and no centroid or posting-list object is written at all.
9. **Clustering is balanced.** No posting list exceeds a stated multiple of the mean, over
   a dataset with deliberately uneven density.
10. Region coverage ≥95% on shipped crates, mutation ≥80%, and the full gate set green.

## Test plan

| # | Test that must fail first | Mutation it catches |
|---|---|---|
| 1 | `two_quantizers_encode_identically` | smuggling per-index state into the encoder |
| 1 | `encoding_is_stable_across_dimensions` | a rotation seeded from data rather than fixed |
| 2 | `rabitq_estimates_stay_inside_the_error_bound` | a bound that is asserted but never exercised |
| 2 | `a_deliberately_wrong_estimator_breaks_the_bound` | a bound so loose it holds for anything |
| 3 | `int8_rerank_beats_one_bit_on_the_same_pairs` | a rerank rung that does not rerank |
| 4 | `a_rung_zero_scan_reads_no_full_precision_bytes` | sections merged, so rung 0 pays for float32 |
| 5 | `recall_at_ten_meets_the_floor` | any recall regression, which is otherwise silent |
| 5 | `recall_is_one_when_every_list_is_probed` | a search that drops candidates independently of `p` |
| 6 | `a_cold_ann_query_costs_three_round_trips` | a data-dependent chain creeping into search |
| 6 | `a_warm_centroid_table_saves_a_round_trip` | centroids re-fetched per query |
| 7 | `probing_more_lists_costs_bytes_not_depth` | posting lists fetched in a loop |
| 8 | `a_small_index_answers_exactly_and_builds_no_index` | building an ANN index nobody needs |
| 8 | `the_exact_threshold_is_the_only_thing_that_switches_paths` | a silent fallback that hides a broken ANN path |
| 9 | `clustering_stays_balanced_on_skewed_data` | k-means that collapses onto dense regions |
| 9 | `every_vector_lands_in_at_least_one_list` | vectors dropped by assignment, invisible except as recall |

## RA budget

| Operation | Budget |
|---|---|
| Cold ANN query | 1 Rseq (centroids) + 1 Rpar (`p` lists) + 1 Rpar (rerank) = **3 depth** |
| ANN query, centroids cached | **2 depth** |
| Small index (exact) | 1 Rpar = **1 depth** |
| Build of *n* vectors | *n* Rpar in + **1 W** per segment, 0 LIST |

## Risks

- **Synthetic data can flatter recall.** A Gaussian mixture is exactly what clustering is
  best at, so a gate built only on it measures the implementation less than the generator.
  Mitigated by generating deliberately adversarial cases too — uneven density, and a
  uniform cloud where clustering has no structure to find — and by making the harness
  accept a real dataset. **The number is reported with its dataset, always.**
- **The scale gap is the main threat to the exit condition.** 200k vectors is three orders
  of magnitude below 100M, and the failure modes that appear at scale (centroid table size,
  posting-list skew, boundary effects) may not appear at all here. Nothing in this
  milestone can close that; it is stated, not managed.
- **Recall as a gate can be gamed by tuning the gate.** The floor moves only in the
  strengthening direction — non-negotiable — and the dataset seed is fixed so a "better"
  number cannot come from a luckier draw.
- **The error bound may not hold as published.** That would be a finding about RaBitQ worth
  more than the milestone, and the test is written to detect it rather than to confirm it.

## Tasks

| ID | Task |
|---|---|
| M3.1 | `pstore-vec`: RaBitQ 1-bit encode + asymmetric query, with the error bound tested |
| M3.2 | int8 scalar quantization and the rerank ladder (rungs 0/1/2) |
| M3.3 | Segment sections for `rabitq` and `sq8`, addressed from the footer |
| M3.4 | Balanced clustering: centroid selection, assignment, boundary augmentation |
| M3.5 | Posting-list layout and the centroid section |
| M3.6 | ANN search: centroid probe, `p` lists in parallel, rerank ladder |
| M3.7 | D-10 threshold: exact below it, clustered above, one switch |
| M3.8 | Recall harness + `scripts/recall.sh` as a gate |
| M3.9 | Depth and byte-budget assertions for the cold and warm paths |
| M3.10 | Carried: measure at ≥10M vectors where real storage exists (blocked on M0b) |
