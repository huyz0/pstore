# M278: request budgets that ratchet both ways

## Serves
D-34 and the request-count rows of [slos.md](../../research/11-design/slos.md), through
lesson 2 of [lessons-from-hsearch-opensearch.md](../../comparisons/lessons-from-hsearch-opensearch.md).

## Delta
1. **One table**, [request-budgets.md](../../research/11-design/request-budgets.md): a row per
   budget, `exact` or `max`, and its count.
2. **`pstore_testkit::budget`**: `spend(name, measured)` and `spend_per(name, measured, units)`
   read the row and fail above it, and for an `exact` row **below it as well**, naming the row
   to lower. A table that does not parse fails rather than skipping a row.
3. **The tests behind the SLO table's request rows read from it** instead of repeating their
   numbers: `cost.rs` (batched, durable and memtable-query costs), `depth.rs`'s API depth,
   `cold_depth.rs`, `end_to_end.rs`'s flush cost and depth, and M277's measurements. Every
   converted assertion keeps its old strength: an `assert_eq!` became an `exact` row, an
   `assert!(x <= n)` a `max` row, with the same number.
4. **`scripts/check-budgets.py`** (and `--self-test`), in `gates.sh` and CI: every row is spent
   by a test, and every name a test spends is a row.

Not changed: what any request costs, and the SLO table's objectives. **No setting**: tests,
a table and a gate.

## Acceptance criteria
1. An `exact` budget fails below and above, a `max` one only above, a per-unit one scales:
   `an_exact_budget_fails_above_and_below`, `a_max_budget_fails_only_above`,
   `a_per_unit_budget_scales_with_its_units`, `spending_less_than_an_exact_budget_fails`.
2. A malformed row or an unknown name fails: `a_misspelt_bound_fails_rather_than_vanishing`,
   `a_missing_count_fails_rather_than_vanishing`, `an_unknown_name_fails`.
3. The converted tests pass against the table, and fail when a row they spend is raised
   above what they measure (the ratchet, on real tests).
4. `scripts/check-budgets.py` passes, and its self-test refuses an unspent row and an
   unknown name.

## Test plan
Criterion 3's red run: `durable_write_puts` raised from 1 to 2 in the table; the server's
`cost.rs` and `puts_per_write.rs` tests must fail with "under its exact budget".

## RA budget
Unchanged. Its numbers move into the table.

## Risks
- A test that spends a row for the wrong measurement satisfies the gate. The table's "What"
  column is read in review.

## Tasks
- **M278.1** The spec.
- **M278.2** The table, the helper, the conversions, the gate.
