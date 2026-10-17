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

1. **`pstore_index::exq`** — extended-RaBitQ codes of `B ∈ {2, 4}` bits a dimension (Gao et al.,
   *Practical and Asymptotically Optimal Quantization of High-Dimensional Vectors in Euclidean
   Space for ANN Search*, SIGMOD 2025). It encodes the **residual from the same centroid** as the
   1-bit code, after the **same rotation** (`Quantizer`'s, so nothing new is trained). For each
   vector it searches the rescale factor over its critical values and keeps the grid code with the
   best cosine to the residual. A record is `B·D/8` bytes of code plus two `f32` (norm and the
   unbiasing `<ȳ, o>`). It has an estimator with `estimate_residual`'s signature. A `B` of 1, 3, 5
   or more is refused, as is a dimension mismatch.
2. **`Ladder::rung0_bounded(scored, cap)`** — `scored` carries each candidate's estimate and its
   bound `b = residual_norm · error_bound(code)`. It keeps every candidate whose `est + b` reaches
   the `k`-th largest `est − b`, best first, never more than `cap`, never fewer than `k` (when
   there are `k`). `rung0` is unchanged.
3. **`search::probe_sq8(table, query, p)`** — the same probe order computed from centroids stored
   as `sq8` records (`sq8::encode`, which already exists), with `<c, q>` for the residual
   estimate taken from the same records. `probe` is unchanged.
4. **`recall` example, `--prism`** — one mode printing all three tables below on **both** corpora,
   each row with its scale, dimension, corpus and parameters. It is not part of the gate run;
   the gate's output and floor are unchanged.

**Not changed:** `vec_index.rs`, `pstore-format`, the engine, the server, `Query::default()`.

## Acceptance criteria

All numbers are **provisional** (container or WSL2, synthetic corpora); they are relative.

1. **A narrower tier, measured.** At p=8, over=32, `--prism` prints recall@10 and MB/query for
   rung 1 = int8 (control), `exq` B=4, `exq` B=2, and **sq4**: per-vector uniform 4-bit with no
   rotation, harness-only, as the control that shows whether the rotation is earning anything.
   **Decision rule:** OQ-170 is answered **yes** if `exq` B=4 is within **0.005** of int8 on
   *both* corpora at **≤ 0.62×** int8's MB/query. If sq4 also passes, the simpler sq4 is the
   recommendation. Otherwise it is answered **no**, with the numbers.
2. **The bound holds where we will lean on it.** `the_residual_bound_holds_at_its_confidence`:
   over every (probed row, query) pair of a 2,000 × 384 clustered fixture, `|true − est| > b` in
   at most **2 × 10⁻³** of pairs. That is twice the δ = 10⁻³ `BOUND_EPSILON` is derived for.
3. **Bound-sized survivors, measured.** At p=8, `--prism` prints, per corpus, recall@10 under
   `exact` and `fast` and the mean survivor count, for `rung0_bounded(cap = 1,024)` against
   `rung0` at over=32. **Decision rule:** OQ-40 is answered **"no constant: the bound"** if bounded
   recall is ≥ over=32's − 0.002 on both corpora *and* mean survivors < 320. Otherwise it records
   which half failed. C-3 predicts the survivor half may fail, and that outcome is a finding.
4. **An int8 centroid table, measured.** At p ∈ {4, 8, 16}, `--prism` prints the mean overlap of
   `probe_sq8`'s probe set with `probe`'s, recall@10 under `fast`, and both table sizes.
   **Decision rule:** OQ-171 is answered **yes** if overlap ≥ **0.99** and recall is within
   **0.003** of `f32` on both corpora at every `p`. The size ratio is `(D + 8) / 4D` (0.255 at 384)
   and is printed as computed, not asserted.
5. **The primitives are tested, red first.** Named in the test plan.
6. **The corpus says what was found.** `prism-comparison.md` gains a dated results table, and each
   of OQ-170, OQ-40 and OQ-171 is struck through and answered in `open-questions.md`, or carries
   the number that kept it open.
7. **Gates:** `./scripts/gates.sh` passes; the coverage floors hold; `./scripts/mutants.sh` over
   the changed lines misses 0, or each miss is argued equivalent in the ledger.

## Test plan

| AC | Test, seen red first | Mutation it catches |
|---|---|---|
| 1 | `exq_error_falls_with_bits`: on 500 rotated vectors, mean \|true − est\| for B=4 < B=2 < the 1-bit code's | the rescale search returning its first candidate |
| 1 | `exq_record_round_trips_and_refuses_bad_shapes`: record length is `B·D/8 + 8`; B of 1, 3 or 5 and a short slice are refused | `B·D/8` off by a byte; a missing refusal |
| 1 | `exq_residual_estimate_is_unbiased`: mean signed error over 2,000 pairs is within 3 standard errors of 0 | the `<ȳ, o>` division removed |
| 2 | `the_residual_bound_holds_at_its_confidence` (above) | the bound not scaled by `residual_norm` |
| 3 | `bounded_rung_keeps_every_overlapping_interval`: hand-built intervals, one touching the `k`-th lower bound and kept, one a hair below and dropped | `≥` → `>`; the `k`-th taken as the `(k−1)`-th |
| 3 | `bounded_rung_respects_its_cap_and_its_floor` | the cap ignored; fewer than `k` returned |
| 4 | `sq8_probe_matches_f32_on_separated_centroids`: 64 well-separated centroids, identical order for 50 queries | `<c, q>` from the wrong record |

The harness rows (criteria 1, 3, 4) are measurements, not tests. Their evidence is the command
and its output, pasted into the ledger.

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
  and the close (criteria 5–7).
