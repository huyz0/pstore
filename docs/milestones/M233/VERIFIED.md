# M233 verification

How to run the tests: `cargo test -p pstore-engine --test it late` and
`cargo test -p pstore-format --lib columnar`. The benchmark: `./scripts/columns.sh` and
`./scripts/columns.sh 100000`.

Red runs: `an_unadmitted_row_is_never_built` names `decode_block_late`, which the parent does
not have, so it was red by not compiling. `late_materialization_answers_as_whole_rows_do`
passes on the parent too (it is an equivalence), so what it tests is shown by mutation. Its
first form compared the columnar engine with the row engine only, and two mutations in code
both share survived; it now also checks every answer against a model built from the
documents in the test (`Aggregator` and `Selector` fed the visible rows), and both are killed.

Mutations were applied one at a time by a script and reverted. 8 of 8 were killed: the
first pass not decoding the id in a shadowed segment; the second pass replacing the first
pass's attributes; the second pass never building the id; the second pass ignoring which rows
were admitted; `Selector::would_keep` inverted and refusing a selector not yet full (killed by
the `order` unit tests, which check the selector against a sort); an aggregation building no
attribute; an order-by's first pass dropping the filter. No `cargo mutants` run is claimed.

CI's `cargo mutants` on #58 found `visit_opened_at`'s `t.shadowed && !shadow.is_empty()` as `||`
missed: no engine path pairs an unshadowed target with a non-empty shadow, so no engine test
could tell. `the_shadow_hides_rows_only_in_a_shadowed_target` (`pstore-query` `tests/it/shadow.rs`)
holds the visit's contract: red under that mutant (`d1` hidden in an unshadowed target), green
without it.

1. `late_materialization_answers_as_whole_rows_do`: green. 201 filters (none, and 200 random
   over `q`, `p`, `s`, `sparse` and the id, every op, under `And`, `Or`, `Not`), each through
   an ungrouped and a grouped aggregation, an order-by of two keys at two offsets, and a
   scoring expression; three segments with two deletes and 70 unfolded rows, 50 of them
   shadowing folded ones; columnar against row blocks, and both against the model.
2. `an_unadmitted_row_is_never_built`: green.
3. `./scripts/columns.sh`, `provisional` (in-memory store, 1 MiB coalescing gap, one 4-core
   container, medians of 31 runs). M232 against M233, built from the two commits and run one
   after the other. GETs and bytes were identical in every case. Times in ms:

   | | 20,000 rows: M232 / M233 | 100,000 rows: M232 / M233 |
   |---|---|---|
   | columnar, `sum(price)` (statistics off) | 12.39 / 2.80 | 65.9 / 14.7 |
   | columnar, `avg(rating) where price >= 1` (every row) | 12.88 / 4.32 | 66.1 / 24.7 |
   | columnar, `max(price) where price >= 5001` | 12.37 / 2.92 | 65.1 / 13.7 |
   | columnar, `count where category =` | 12.26 / 2.69 | 70.6 / 12.8 |
   | columnar, `sum(price) where customer =` (2 blocks) | 0.327 / 0.091 | 10.1 / 1.81 |
   | columnar, `sum(price)` by category | 13.70 / 6.38 | 79.6 / 33.2 |
   | columnar, top 10 by price | 13.15 / 3.51 | 67.4 / 16.8 |
   | columnar, top 10 by price `where category =` | 13.16 / 4.68 | 75.9 / 22.6 |
   | columnar, top 10 by rating `where price >= 1` (every row) | 13.41 / 4.44 | 76.9 / 21.5 |
   | row blocks (M230), `sum(price)` | 12.95 / 4.87 | 67.6 / 24.6 |
   | row blocks (M230), `sum(price)` by category | 14.66 / 7.96 | 74.5 / 39.1 |
   | row blocks (M230), top 10 by price | 13.33 / 13.41 | 68.0 / 67.2 |
   | row blocks (M230), top 10 by rating `where price >= 1` | 13.68 / 13.71 | 69.4 / 69.1 |

   The first M233 run had row blocks' order-bys 7% to 16% slower: the joined projection asked
   the first pass's names (a binary search) before `ALL` for every attribute of every row.
   Asking `then` first brought them back within 1%; the table is that run. Masks and the
   whole-segment decode do not use the new path and moved within run-to-run noise. Adopted,
   with no setting, by criterion 3's rule.

Downsides, measured: (a) **row blocks' order-bys gain nothing**: a row block is decoded once
with both passes' names, and an order-by needs every attribute; the cost is within 1%.
(b) **A filter admitting every row** gains least (`avg(rating) where price >= 1`: 3x, against
4.4x unfiltered), since its column is decoded in the first pass and the rest for every row.
(c) **Order-bys build every admitted row whole** before the selector is full: the early
refusal starts once it holds `offset + limit` rows. Nothing changes on disk, so there is
nothing to roll back.

Gates, in a cloud container: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` (the two IPv6 peer tests this container cannot run skipped; nothing else failed), `scripts/depth.sh`, `check-links.sh`, `check-verified.py`, `gen-clients.py --check`, `check-slos.py`, `build-index.py --check` and `check-poison.sh` passed. The coverage floor is CI's full run.
