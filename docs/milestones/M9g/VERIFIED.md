# M9g — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).
M9g.1 is delivered here; criterion 2 (`sum` and `max`) is M9g.2's and **NOT-RUN**.

1. **Weighted RRF and `k`** — `weights_and_k_shape_reciprocal_rank_fusion`
   (`cargo test -p pstore-server --test composition`): weights `[1, 0]` reproduce the vector
   leg's order; at `k = 1` a 5:1 weight puts the heavier leg's own first hit first, either way
   round; one leg's first place scores exactly `w / (k + 1)` at `k = 60` and `k = 10`. **Observed
   red** on the M9f.2 server (`fusion` ignored), as was every test in the file; seen red with
   the weight ignored and with `k` ignored. `several_text_queries_are_legs_of_their_own`: one
   string and a one-string array answer alike, two queries rank every row either matches, and
   a weight on the second leg alone moves its best hit first.
2. NOT-RUN — M9g.2.
3. **Multi-query** — `a_multi_query_answers_each_as_it_would_alone`: a vector query, a weighted
   two-text query and a `rank_by` order, each result equal to the query run alone; zero LISTs.
4. **Refusals** — `composition_refuses_what_it_cannot_mean` (an unknown kind, `sum` and `max`
   until M9g.2, a wrong weight count, a negative weight, `k = 0`, a non-object `fusion`, 16
   texts, an empty text array, zero, 17 and nested `queries`, `queries` beside another field,
   and a sub-query's own refusal); 15 texts and a vector -- 16 legs -- answer. ⚠️ Code review
   found `fusion` beside `rank_by` accepted unchecked and a mistyped parameter (`kk`) read as
   the default: both are refused now, each seen red with its check removed; it also made the
   empty-text and nested-`queries` cases reach the check they name (each had been refused by
   an earlier one). `weights_fill_to_one_and_refuse_past_the_bound`
   (`cargo test -p pstore-query --lib fuse`). Every existing test passes untouched.
5. **Gates** (M9g.1) — `./scripts/mutants.sh --check . --in-diff <the M9g.1 diff>` after code
   review's fixes: **55 tested in 35m, 47 caught, 8 unviable, 0 missed**. Code review: two rounds
   (block on `fusion` unchecked beside `rank_by`, then pass). `./scripts/gates.sh` on the
   committed tree: all fifteen PASS.
