# M97 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores). Every number is
**provisional** and was measured once, on synthetic corpora (a Gaussian mixture and a uniform
cloud, 384d), so a "no" here is a "no" on synthetic data, as the spec's risks say. The decision
rules are the spec's, written before the harness existed, and none was moved.

Commands:
- `./scripts/recall.sh --prism` (output below, 46 s)
- `cargo test -p pstore-index --test exq --test ladder --test probe_sq8 --test rabitq`

```
# 20000 x 384d clustered corpus, 100 lists, k=10 (provisional: container, synthetic)
## rerank tier, p=8 over=32: recall@10 under rung 1, bytes beside the 1-bit code
  tier  recall@10      hits     B/row  MB/query    x int8
  int8     0.9810  981/1000       464     0.841     1.000
  exq4     0.8560  856/1000       272     0.493     0.586
  exq2     0.5540  554/1000       144     0.261     0.310
   sq4     0.6990  699/1000       272     0.493     0.586
## survivors, p=8: recall@10 under exact, and the fourth round's f32 bytes
          rule  recall@10      hits  survivors    MB rt4
       over=32     0.9990  999/1000      320.0     0.492
bound cap=1024     1.0000 1000/1000      499.8     0.768
## centroid table: f32 153600 B, int8 39200 B; recall@10 under fast (int8 rung 1), over=32
   p        f32      hits       int8      hits  overlap     lists
   4     0.9800  980/1000     0.9800  980/1000   0.9875   395/400
   8     0.9810  981/1000     0.9810  981/1000   0.9925   794/800
  16     0.9810  981/1000     0.9810  981/1000   0.9950 1592/1600
# 10000 x 384d uniform corpus, 50 lists, k=10 (provisional: container, synthetic)
## rerank tier, p=8 over=32: recall@10 under rung 1, bytes beside the 1-bit code
  tier  recall@10      hits     B/row  MB/query    x int8
  int8     0.5150  515/1000       464     1.378     1.000
  exq4     0.5110  511/1000       272     0.808     0.586
  exq2     0.4110  411/1000       144     0.428     0.310
   sq4     0.5040  504/1000       272     0.808     0.586
## survivors, p=8: recall@10 under exact, and the fourth round's f32 bytes
          rule  recall@10      hits  survivors    MB rt4
       over=32     0.5150  515/1000      320.0     0.492
bound cap=1024     0.5170  517/1000     1024.0     1.573
## centroid table: f32 76800 B, int8 19600 B; recall@10 under fast (int8 rung 1), over=32
   p        f32      hits       int8      hits  overlap     lists
   4     0.3020  302/1000     0.3050  305/1000   0.9900   396/400
   8     0.5150  515/1000     0.5170  517/1000   0.9938   795/800
  16     0.7380  738/1000     0.7390  739/1000   0.9950 1592/1600
```

The int8 rows are the control. They reproduce the plain gate run, `./scripts/recall.sh`, which is
unchanged: clustered `fast` is 0.9810 at p=8 and uniform is 0.5150.

1. **A narrower tier: none passes, so OQ-170 is answered no.** A tier must be within 0.005 of int8
   on both corpora, at 0.62× int8's bytes or less. exq B=4 is 0.586× and gives 0.8560 against
   0.9810 on clustered, though 0.5110 against 0.5150 on uniform is within the margin. exq B=2 gives
   0.5540 and 0.4110, and sq4 gives 0.6990 and 0.5040. The exq tests must pass before these numbers
   count, and they do: `exq_search_beats_a_fixed_scale`, `exq_error_falls_with_bits`,
   `exq_record_round_trips_and_refuses_bad_shapes`, `exq_residual_estimate_is_unbiased`, and
   `a_vector_on_its_centroid_estimates_the_centroid`.
2. **The bound holds and is tight.** `the_residual_bound_holds_and_is_tight` uses 50 queries × 800
   probed rows, 40,000 pairs. The full bound is breached 6 times (1.5 × 10⁻⁴, under the 2 × 10⁻³
   limit) and half the bound 2,048 times (5.1%, over the 2% floor). The same queries at 3× their
   length breach half the bound 2,048 times too. Dropping the residual norm leaves 17 breaches of
   half the bound and fails the test. Dropping the query norm gives 20,764 at 3× and fails it too.
3. **Bound-sized survivors: the survivor half fails on both corpora, so OQ-40 stays open.** The
   recall half passes. `exact` recall is 1.0000 against 0.9990 on clustered and 0.5170 against
   0.5150 on uniform, so neither is more than 0.002 below over=32's. But the mean survivor count is
   499.8 on clustered and 1,024.0 on uniform, where every query hit the cap. The rule needs it under
   320. The ladder's arithmetic is pinned by `bounded_rung_keeps_every_overlapping_interval` and
   `bounded_rung_respects_its_cap`.
4. **An int8 centroid table: the overlap half fails at one point, so the answer is not yes.**
   Recall is never more than 0.003 below f32's. It is equal at every p on clustered, and higher by
   0.003, 0.002 and 0.001 on uniform. Overlap is at least 0.99 in five of the six cells, but
   clustered p=4 has 395/400 = 0.9875. The table is 39,200 bytes against 153,600 for 100 lists.
   The probe order and `<c, q>` are pinned by `sq8_probe_orders_and_scores_as_f32`.
5. **Recorded.** `prism-comparison.md` gains a "Measured (M97, 2026-10-17)" section and a banner
   on each idea. OQ-170 is struck through as answered no. OQ-40 and OQ-171 carry the numbers that
   keep them open. The INDEX row is restated. `./scripts/check-links.sh` and
   `./scripts/build-index.py --check` pass (see 6).
6. **Gates.**
   - **Seen red first.** Stubs that compiled but returned nothing or a constant failed six tests:
     `exq_record_round_trips_and_refuses_bad_shapes`, `exq_error_falls_with_bits`,
     `a_vector_on_its_centroid_estimates_the_centroid`, both bounded-rung tests,
     `sq8_probe_orders_and_scores_as_f32`, and `the_residual_bound_holds_and_is_tight`. Two tests
     passed against the stub, which returned the 1-bit estimate and a constant cosine. Each was
     then seen red against the defect it targets. `exq_search_beats_a_fixed_scale` failed with
     the search returning its first candidate ("B=2: chose cosine 0.8018, but scale 19.65 gives
     0.9163"). `exq_residual_estimate_is_unbiased` failed with the cosine division removed: mean
     error −0.004836 against a standard error of 0.000060.
   - ⚠️ **One assertion changed before it ever passed.** The mean comparison in
     `exq_search_beats_a_fixed_scale` was first written with a margin of 10⁻³, which the spec does
     not state. It failed at B=4, 0.9951 against 0.9947. It now asserts the spec's words, a strict
     "beats", and the test comments say so. The per-critical-value check beside it is unchanged
     and is what catches the search mutant.
   - **Mutation:** `cargo mutants --in-diff` over the M97 diff of `pstore-index/src`, tested
     against `--lib` and the four M97 test binaries. That narrowing can only under-count
     caught mutants, never over-count them.
     - First sweep: 187 mutants, 170 caught, 3 unviable, and 14 missed. Each miss was then
       dealt with:
       - Seven were in `rotated_residual`'s **copy** of the 1-bit encoder's normalisation,
         which nothing downstream can see because the cosine and the estimate are both
         scale-invariant. The copy is gone. `Quantizer::unit_residual` is now the one place
         either code takes it.
       - Four (`cosine_at` → 0 or −1, and its `*` → `+` or `/`) left the search test
         one-sided. It now also asserts that the chosen cosine **equals** the best state the
         scan visits. It was seen red against `cosine_at` → 0: "B=2: chose cosine 0.94636166,
         but the best critical value gives 0".
       - `|| residual_norm == 0` beside `cosine == 0` was redundant, because a zero residual
         is the only way to get a zero cosine. It was removed.
       - `cosine`'s `* ||o'||²` was always 1, because the residual is unit and the rotation
         orthonormal. It was removed, so the cosine now matches what the search maximises.
       - The search's `>` → `>=` only breaks ties between prefixes with the same cosine. It
         is equivalent and is excluded in `.cargo/mutants.toml`, named by line and column.
       - `BitsError`'s `Display` was killed by a new assertion that the refusal names the
         width.
     - Re-sweep after those changes:
       - `exq.rs`: 132 mutants, 128 caught, 3 unviable, and 1 missed, the excluded tie.
       - `rabitq.rs`, against the whole `pstore-index` suite: 20 mutants, 19 caught, 1
         unviable.
     - Why the narrowing stands as the answer: every test in the narrowed set is also in
       the workspace suite, so a mutant the narrowed set catches, the workspace catches
       too. The narrowed sweep missed nothing outside the one exclusion. A workspace run
       could therefore kill more mutants, but it could not add a miss.
     - The script's own whole-workspace run was also started on M97's diff,
       `MUTANTS_BASE=63f9779 ./scripts/mutants.sh`: 178 mutants, at about six minutes
       each on this 4-core container. It was stopped at the session's two-hour limit
       after 21, all in `exq.rs`: 20 caught, 1 unviable, 0 missed. It was not
       completed, and those 21 are not a sweep of the rest.
     - `./scripts/recall.sh --prism` was rerun after the changes, and its output is identical
       to the block above.
   - `./scripts/gates.sh`: every gate PASS, on the tree as committed.
   - **`./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`: PASS**, after a
     follow-up commit that restores the region floor. As first committed, M97 failed it.
     - At `991c86a`, measured here: lines 96.47% and regions **94.52%** (2,922 of 53,275
       missed). CI's `coverage` job on the base, `63f9779`, read 94.49%, so the floor was
       already red before M97. M97's own files: `exq.rs` 97.37%, `ladder.rs` 100%,
       `rabitq.rs` 97.53%, `search.rs` 96.48%.
     - The follow-up adds tests and no shipping code. Each was seen red against an injected
       defect before it counted. The tests cover the external index's decoder (every
       truncation of a blob with every section), the HEAD decoder (every cut of a HEAD with
       every section: 14 decodable, one per optional section, seen red at 15 against a read
       that defaulted), condition, disk-cache key, Lance v2.1 compression, analyzer and
       full-text line refusals, the jobs queue's config, a lost registration, Azure offline
       build, sketch declarations, reserved names, strong reads of a past epoch, and a
       multi-vector query of another width, on both the exact and the FDE path.
     - Measured on the tree as committed, `--all-features` as CI runs it: exit 0, lines
       96.85% and regions **95.08%** (2,652 of 53,946 missed). With the default features:
       exit 0, the same 95.08% and 2,652 missed.
     - No threshold was moved.
   - **Review:**
     - Round 1 on the full staged diff found one major finding and one minor.
       - Major: this ledger was untracked while staged docs linked to it. Fixed in `991c86a`.
       - Minor: a test comment said "about 2%" where the measured bias is −0.0048. Corrected.
     - A later round, on the mutation-driven changes made after round 1, found no blocking or
       major finding, and three minors, each addressed:
       - A residual whose squares underflow has norm 0 and a nonzero cosine, so the
         `cosine == 0` guard does not catch it. It is safe only because the estimate
         multiplies by the norm first. That is now stated in `estimate_residual` and pinned
         by `a_residual_too_small_to_normalise_estimates_the_centroid`, which was seen red
         (NaN) with the product reordered.
       - The `>` → `>=` exclusion in `.cargo/mutants.toml` said the two prefixes give "the
         same code". They give the same cosine, not the same code. The comment now says
         so, and that the mutant is not strictly equivalent.
       - A comment said `y2 >= D / 4` where it is `padded / 4`. Corrected.
     - The coverage tests were reviewed as their own change, in one round on the
       full staged diff: pass, with no blocking or major finding and three minors, which land
       as they are and are listed in that commit's message.

**Not covered**, as the spec states: a segment section, an index option, an engine path or any
API. Real embedding families (OQ-39) are not covered either. A B above 4 was not measured. At 384d
a B=6 record would be 0.86× int8's bytes, which is arithmetic and not a measurement.
