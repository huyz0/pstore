# M10 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

1. **A read-only process** — `a_read_only_process_reports_the_epoch_it_served`
   (`cargo test -p pstore-server --test served_epoch`). B reports `e` for a relevance query,
   a rank_by order, a strong query, and each multi-query entry. **Observed red** on the
   unfixed server: it reported 0.
2. **A stale process** — `a_process_behind_another_reports_the_newer_epoch_it_served`.
   **Observed red**: it reported `e`, not `e + 1`.
3. **Round trip** — `the_served_epoch_repeats_the_read_as_of`. The test uses distinct vectors
   and distinct `n`, so neither order rests on a tie. **Observed red**: an as_of of 0 answered
   404.
4. **The engine** — `a_live_answer_carries_the_heads_epoch` and
   `a_past_answer_carries_the_epoch_it_asked_for`
   (`cargo test -p pstore-engine --test served_epoch`). They cover an existing index and one
   never written, live and as_of. **Observed red** as a compile error: the fields did not
   exist.
5. **No cost** — `a_read_only_process_reports_the_epoch_it_served` pins its three queries'
   cost as measured on the unfixed server:
   - reads 5, 3 and 7;
   - bytes 545, 537 and 553;
   - no LIST.

   They are unchanged after the fix. Every existing count and depth test passes untouched.
6. **Gates**
   - `./scripts/mutants.sh --check . --in-diff <the M10 source diff>`: **4 tested, 4
     unviable, 0 missed.** Each mutant replaces a function with `Ok(Default::default())`,
     which cannot compile, because `Answer` has no `Default`.
   - ⚠️ So the sweep says nothing about the stamps, and the stamps were mutated by hand
     instead. Each of these made `cargo test -p pstore-engine --test served_epoch` fail:
     - the live early return and the live full path, each stamped with 0;
     - the as_of early return and the as_of full path, each stamped with HEAD's epoch;
     - the rank_by as_of path, stamped with HEAD's epoch.
   - A sixth hand mutant, stamping 0 at line 1318, landed on a pre-existing IndexStats site
     this change does not touch. The engine tests passed with it applied. It is not evidence
     for this change, and it is recorded rather than dropped.
   - Spec review: one round, pass. It raised two minors, both taken:
     - the empty-index as_of case;
     - BACKLOG row 42, for the index-metadata fallback.
   - Code review: one round, pass. Its one major was that this ledger was not yet written.
   - `./scripts/gates.sh` on this tree: all seventeen PASS.
