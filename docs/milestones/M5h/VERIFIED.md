# M5h — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read. Command: `cargo test -p pstore-engine --test fresh_query`.

1. **An unfolded row reaches the indexed query** — `an_unfolded_row_reaches_the_indexed_query`,
   and `an_unfolded_row_reaches_both_scan_and_query` in `engine_query.rs`, which is
   [M5g](../M5g/VERIFIED.md)'s test **amended rather than deleted**: it asserted the opposite
   limit, and now asserts the agreement it used to deny. A behaviour change recorded by a test
   quietly disappearing is one nobody can find later. Observed red by not pushing the fresh
   target — four of the seven tests here fail.
2. **Ranked against the corpus, not beside it** —
   `a_fresh_row_is_ranked_against_the_corpus_not_beside_it`: a fresh row whose score puts it
   **between** two folded rows comes back between them.
   ⚠️ The first fixture could not fail — it reused `doc(250)`, a duplicate id that also fell
   outside the top 20 of the query asked. The row is now a distinct id at a component value
   that lands mid-pack by construction.
3. ⚠️ **The statistics include the fresh rows** — `the_statistics_include_the_unfolded_rows`.
   **Added after a mutation survived**, and this is the finding: every other fixture here
   drives a **dense** leg, which never reads `Stats` — so dropping the fresh segment's term
   dictionary, which removes its rows from the corpus statistics entirely, changed nothing and
   no test noticed. M5c measured that global IDF changes the top-1; this is that defect one
   corpus-slice down. The fixture's unfolded slice shifts **both** corpus-wide inputs at once —
   `gamma`'s document frequency and `avgdl` — and the ranking must match the same documents
   wholly folded. Observed red.
4. **Folding changes nothing, below the threshold** —
   `a_fold_does_not_change_the_answer_below_the_threshold`: the same query before and after a
   fold of the same rows, identical order. The criterion that cannot be faked, because it
   compares the fresh path against the folded path over the same rows.
   ⚠️ **Scoped below the exact-scan threshold, and the scoping is a spec-review finding rather
   than a convenience.** Above it the folded half is searched by ANN probe and the fresh one,
   being small, is scanned exactly — so a row the probe would miss is present before the fold
   and absent after. The first draft's unscoped version contradicted this spec's own risk.
5. **Depth unchanged, zero LIST** — `an_indexed_query_is_still_three_rounds_and_no_list`.
   Sealing and searching the fresh segment happens in a **private** `MemoryStore`, so it costs
   the tenant's store nothing; the test asserts the tenant saw no LIST and no extra write.
6. **The fresh ordinal indexes the returned documents** —
   `the_unfolded_ordinal_indexes_the_returned_documents`, asserted in both directions plus a
   guard that the fixture actually produced fresh hits. A caller indexing the wrong list gets a
   plausible wrong document.
7. **Nothing unfolded behaves as before** — `an_index_with_nothing_unfolded_is_unchanged`.
   Observed red by building an empty fresh segment anyway, which occupies an ordinal for
   nothing and shifts every other one.
8. **A fresh segment that cannot be sealed fails the query** — `OBSERVED-NOT`. The refusal is
   wired (`try_build_all`'s error is returned, not swallowed) and reviewed, and no fixture
   reaches it: `check_storable` guards the write door, so the shapes that reach the memtable
   are storable, and the remaining refusal is the index-budget one, which needs a memtable
   whose own zone maps exceed the budget. ⚠️ The behaviour that matters is stated rather than
   claimed — it must **not** fall back to the folded-only answer, which is the stale result
   this milestone removes, returned silently.
9. **The cache rebuilds only when the memtable changes** — observed red by making the
   generation comparison always true, which fails
   `a_fold_does_not_change_the_answer_below_the_threshold`: a cache that never invalidates
   serves a fresh answer that has stopped being fresh.
10. **Gates** — `./scripts/gates.sh` green, `./scripts/ndcg.sh` above its floor with the
    control below it, `./scripts/recall.sh` PASS.

## What is not built, and named rather than omitted

- ⚠️ **Resolving hits to document ids.** That is a block fetch per segment and would make
  `Engine::query` **four** sequential round trips, over the budget of three. The handle stays
  `(segment, row)`, and `Answer` exists because the fresh ordinal indexes nothing in HEAD.
- ⚠️ **A bound on the memtable.** `query` rebuilds a segment when the memtable changes; the
  window is bounded by the flush and fold intervals in normal operation and by **nothing** if a
  fold is failing. The generation cache moves the cost to writes; it does not bound it, and
  nothing reports a memtable that has stopped draining.
- ⚠️ **Agreement above the exact-scan threshold.** The fresh half is exact and the folded half
  is an ANN probe, so a fresh row competes *advantaged* against a folded one the probe would
  have missed. Bounded by the memtable's share of the corpus — small by construction, and
  unbounded by anything if folds stop.
- **`Engine::search`** stays fresh and exact, and is still the right answer for a caller who
  wants no approximation at all.
