# M97 — Three ideas from PRISM, measured before any is shipped

**Serves:** OQ-170 (a narrower rerank tier), OQ-40 (oversample per rung), OQ-171 (an int8
centroid table); the finding is [`prism-comparison.md`](../../research/06-indexing/prism-comparison.md).
D-11, D-12 and D-34 bound what may change.

⚠️ **A measurement milestone.** It adds library primitives and a harness mode. It changes **no
segment format, no store path, no default and no API**. Each idea ends in a recorded number and a
decision rule written here *before* the number exists, so that a disappointing result closes the
milestone as surely as a good one. Wiring a winner in (a section, an index option, an engine path)
is the next milestone, specified once these numbers are in.

## What is true today (measured for this spec, `./scripts/recall.sh`, provisional: container)

| Corpus, k=10, over=32 | p | `none` | `fast` (int8) | `exact` (f32) | MB/query fast / exact |
|---|---|---|---|---|---|
| clustered 20,000 × 384, 100 lists | 8 | 0.3090 | 0.9810 | 0.9990 | 0.84 / 0.62 |
| uniform 10,000 × 384, 50 lists | 8 | 0.2440 | 0.5150 | 0.5150 | 1.38 / 0.71 |

Two facts this spec rests on. The int8 records are **84%** of the bytes a `fast` query moves
(p=16: 1.60 MB against 0.25 MB for the 1-bit codes alone). And on the uniform corpus `exact` adds
nothing over `fast`, so the loss there is at rung 0 or in probing, not in rerank precision.

## Delta

1. **`pstore_index::exq`** — extended RaBitQ (Gao et al., *Practical and Asymptotically Optimal
   Quantization of High-Dimensional Vectors in Euclidean Space for ANN Search*, SIGMOD 2025) at
   `B ∈ {2, 4}` bits a dimension, stored as the **`B − 1` extra bits beside the existing 1-bit
   code**: the sign bit *is* the 1-bit code's bit. The residual is the same one, from the same
   centroid and under the same rotation, so nothing new is trained. For each vector it searches
   the rescale factor over its critical values and keeps the code with the best cosine to the
   residual.
   - **Record:** `(B − 1) · padded / 8 + 8` bytes, two `f32` (norm and the unbiasing `<ȳ, o>`).
     ⚠️ `padded`, not `D`: the rotation spreads energy into the padding (`rabitq.rs` module
     docs), so 384d codes 512 coordinates. At 384d a B=4 record is 200 B and B=2 is 72 B.
   - **Estimator:** the same shape as `estimate_residual`, taking the 1-bit code and the extra bits.
   - **Refused:** `B` outside {2, 4}, and a slice of the wrong length.
   - **Touches `rabitq.rs`:** `rotate`, `padded` and `Query`'s vector become `pub(crate)`, and
     `lib.rs` gains `pub mod exq`. Nothing else in `rabitq.rs` changes.
2. **`Quantizer::residual_error_bound(code, query_norm)`** = `residual_norm · query_norm ·
   error_bound(code)`, the bound on `estimate_residual`. ⚠️ `prepare` does not normalize, and the
   engine passes raw queries, so the query norm is an argument rather than an assumption.
3. **`Ladder::rung0_bounded(scored, cap)`** — each candidate carries its estimate and its bound `b`.
   It keeps every candidate whose `est + b` reaches the `k`-th largest `est − b`, best first, at most
   `cap`. When the bounds hold, that set contains the true top `k` and at least `k` rows. `rung0`
   is unchanged.
4. **`search::probe_sq8(table: &[sq8::Code], query, p) -> Vec<(usize, f32)>`** — the probe order
   and `<c, q>` for each probed list, both from centroids stored as `sq8` records. It ranks by
   `‖c‖² − 2<c, q>` with `‖c‖²` from the decoded record, which is `probe`'s L2 order. `probe` is
   unchanged.
5. **`recall` example, `--prism`** — prints the three tables below on **both** corpora, each row
   with its scale, dimension, corpus and parameters. MB/query uses `padded` for every code-shaped
   tier, as the harness's 1-bit formula already does. The gate run, its output and its floor are
   unchanged.

**Not changed:** `vec_index.rs`, `pstore-format`, the engine, the server, `Query::default()`.

## Acceptance criteria

All **provisional**; p=8, over=32, k=10; hit counts beside recall; "both" = clustered **and** uniform.

1. **A narrower tier.** `--prism` prints recall@10 under rung 1 and MB/query for int8 (control),
   `exq` B=4, `exq` B=2 and sq4 (unrotated per-vector 4-bit, harness-only). **Rule:** a tier
   *passes* if its recall is ≥ int8's − 0.005 on both, at ≤ 0.62× int8's MB/query. OQ-170 is
   answered by the passing tier with the fewest bytes, with sq4 preferred to `exq` at equal bytes
   because it is simpler. If none passes, the answer is no, with the numbers.
2. **The bound holds and is tight.** `the_residual_bound_holds_and_is_tight`: on a 2,000 × 384
   clustered fixture, with 50 unit queries at p=8, breaches of `residual_error_bound` are ≤ 2 × 10⁻³
   of the (probed row, query) pairs. Breaches of half the bound are ≥ 10× that rate.
3. **Bound-sized survivors.** `--prism` prints recall@10 under `exact` and the mean survivor count
   of `rung0_bounded(cap = 1,024)` and of `rung0` at over=32. **Rule:** OQ-40 is answered "no
   constant: the bound" if, on both, `exact` recall is ≥ over=32's − 0.002 *and* the mean survivor
   count is < 320. Otherwise the ledger records which half failed, on which corpus.
4. **An int8 centroid table.** At p ∈ {4, 8, 16}, `--prism` prints the mean overlap of
   `probe_sq8`'s set with `probe`'s, recall@10 under `fast`, and both table sizes. **Rule:** OQ-171
   is answered yes if, on both and at every `p`, the overlap is ≥ 0.99 and recall is ≥ f32's − 0.003.
5. **Recorded.** `prism-comparison.md` gains a dated results table; OQ-170, OQ-40 and OQ-171 are
   each answered or carry the number that kept them open.
6. **Gates.** The test plan's tests seen red first; `./scripts/gates.sh` and the coverage floors
   pass; `./scripts/mutants.sh` misses 0 on the changed lines, or each miss is argued equivalent.

## Test plan

| AC | Test, seen red first | Mutation it catches |
|---|---|---|
| 1 | `exq_search_beats_a_fixed_scale`: on 500 rotated residuals, the chosen code's cosine is ≥ that of every other critical value tried, and its mean beats a fixed-scale code | the rescale search returning its first candidate |
| 1 | `exq_error_falls_with_bits`: mean \|true − est\| for B=4 < B=2 < the 1-bit code's, on correlated queries (`near`, as `tests/rabitq.rs`) | extra bits ignored by the estimator |
| 1 | `exq_record_round_trips_and_refuses_bad_shapes`: the length is `(B − 1)·padded/8 + 8` at D = 384 and 100; B of 1, 3 and 5 and a short slice are refused | `D` for `padded`; a missing refusal |
| 1 | `exq_residual_estimate_is_unbiased`: B=4, 2,000 correlated pairs, mean signed error within 3 standard errors of 0 | the `<ȳ, o>` division removed |
| 2 | `the_residual_bound_holds_and_is_tight` (above) | `residual_norm` or `query_norm` dropped |
| 3 | `bounded_rung_keeps_every_overlapping_interval`: hand-built intervals, one touching the `k`-th lower bound and kept, one a hair below and dropped | `≥` → `>`; the `(k−1)`-th taken for the `k`-th |
| 3 | `bounded_rung_respects_its_cap` | the cap ignored |
| 4 | `sq8_probe_orders_and_scores_as_f32`: 64 centroids with 8 near-tied pairs, the order matches `probe` except within a tie, and each `<c, q>` is within the sq8 step bound of f32 | `<c, q>` from the wrong record; `‖c‖²` dropped |

The harness rows (criteria 1, 3, 4) are measurements, not tests. Their evidence is the command and
its output, pasted into the ledger.

## RA budget

Unchanged: W, Rseq, Rpar, List, bytes and depth. Nothing here is on a store path. What the
numbers *predict* for a later milestone is stated in each decision rule: rung-1 bytes for
criterion 1, fourth-round bytes for criterion 3 and centroid-object bytes for criterion 4.

## Risks

- **Synthetic corpora flatter or punish the method.** The Gaussian mixture is the easy case, and
  uniform is the hard one, which is why every rule needs both. Real embedding families are OQ-39's
  and stay open; a "yes" here is a yes on synthetic data, said so in the ledger.
- **An `exq` bug that reads as "the method loses".** `exq_error_falls_with_bits` and the
  unbiasedness test must pass before any harness number is believed.
- **The bound is two-sided at δ per pair, and a query has ~1,800 pairs.** A few violations a query
  are expected; criterion 3's recall comparison, not the bound's derivation, is what decides.
- **Fitting the rule to the result.** The bounds above are written before the harness exists; the
  ledger may not move them (the non-negotiable on thresholds).

## Not covered

- A segment section, a centroid-object version, an index option (`rerank_codes`), an engine path
  or any API surface. That is the next milestone, for whichever rule says yes.
- fastscan (H-1; OQ-173), a centroid tree (OQ-37, OQ-53), SOAR (OQ-172), auto `nprobe`: see
  `prism-comparison.md` for why not now.

## Tasks

- **M97.1** `exq`, its tests, and the `--prism` tier table (criterion 1).
- **M97.2** `rung0_bounded`, the bound test, and the survivor table (criteria 2, 3).
- **M97.3** `probe_sq8`, its test, and the centroid table (criterion 4).
- **M97.4** Results into `prism-comparison.md` and `open-questions.md`, the ledger, the gates,
  and the close (criteria 5, 6).
