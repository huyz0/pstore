# M203 verification

The spec was reviewed once before code: no blocking finding, three majors (the oracle's
`Saturate`/`Decay` arguments could under- or overflow the vendor's formula as written; a
kept score could narrow to an `f32` 0; a `"0s"` midpoint), all applied, and six minors, of
which the parse ambiguity (Saturate, Decay and Dist read only with an array second), the
root routing, the `Product` weight order, a test of the float clamp and the no-value note
were applied.

How to run the tests: `cargo test -p pstore-query --test it expressions::`,
`cargo test -p pstore-query --lib expr::`,
`cargo test -p pstore-engine --test it order::an_expression_query_takes_three_rounds` and
`cargo test -p pstore-server --test it numeric_scoring::`.

Red runs. `numeric_expressions_over_http` was run against the parent's `pstore-server` and
`pstore-query` sources (this milestone's stashed) and failed: `rank_by` refused the
vendor's Saturate example (400). `numeric_expressions_out_of_bounds_are_refused` passed
there, as every one of its inputs was refused by the parent too; it is seen red by
mutation S4 below. The query crate's tests cannot compile against the parent (the nodes do
not exist), so they were seen red by the mutations below.

Each named mutation was applied by a script, the tests above run, and the mutation
reverted; the tree was compared with its state before the run. No `cargo mutants` run is
claimed for them. All were killed.

1. `numeric_expressions_match_the_oracle`: green, 300 seeded trees, every kind drawn at
   least 30 times. Killed: `Saturate` as `Decay` (M1); the exponent ignored (M2); `Dist`
   without `abs` (M4); no value as 1 under `Decay` (M5). `Max` folding from 0 (M3) first
   **survived** every test: under a non-negative root it changes a score only through a
   `Sum` of a signed `Max` inside another `Max`, which the generator seldom draws. A case in
   `missing_values_score_zero` was added (`Max([0, Sum([Max([n]), 5])])`) and M3 is killed
   by it.
2. `signed_roots_are_refused`: green. Killed: `Max` non-negative only when every clause is
   (M6); `Product` of a signed clause non-negative (M7); `Sum` non-negative when any clause
   is (M12).
3. `missing_values_score_zero` and `expr::tests::wire_scores_stay_positive`: green.
   Killed: `Decay` not clamping a negative to 0 (M8); `Saturate` not clamping (M13, by the
   oracle); a datetime read as a number under `Dist` (M9); the float clamp removed (M10);
   the wire narrowing unclamped (M11).
4. `numeric_expressions_over_http` and `numeric_expressions_out_of_bounds_are_refused`:
   green. Killed: a duration's `d` read as hours (S1); an offset-less origin refused (S2);
   every pair of strings read as an expression (S3); an unknown params key accepted (S4).
5. `order::an_expression_query_takes_three_rounds`, with a `Saturate` of an `Attribute`
   clause added: green, 3 rounds and `2 + 3 × segments` requests. The mutation "an attribute
   read issuing its own request" is OBSERVED-NOT: no code path could issue one, as the value
   is read from the decoded row; the request count is the check.
6. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass. `cargo test --no-fail-fast` over pstore-format, -index, -query,
   -engine and -server, skipping the two IPv6 tests (NOT-RUN in this container, no IPv6):
   2,143 passed and one failed, `wire_scores_stay_positive`, whose spec line was wrong
   (`1e-40` is a positive subnormal `f32`, not below its range); the spec and test now use
   `1e-50`, and the crate's unit tests pass (42). `cargo deny check`, coverage and the
   mutation sweep are NOT-RUN here; they are CI's on the PR.

Code review, round 1: pass, with three minors. Fixed: `["Attribute", "Eq", 3]`, a filter on
an attribute named `Attribute`, was refused; only `["Attribute", name]` is now the node, and
`numeric_expressions_over_http` asserts the filter (not seen red). Malformed and over-long
durations (`"1.h"`, `".5h"`, 16 digits) added to the refusals. The third (M204's directory
in the tree) was a separate worktree's, not this change's.

CI's mutation shards on the PR's first head missed eight mutants, each now killed or gone:
a `uint64` attribute's arm deleted (a test of one added); `origin_instant`'s guards' `&&`
as `||` twice (the guards were redundant -- appending `Z` parses only where there was no
offset -- and are removed; two malformed origins added to the refusals); `saturate`'s and
`decay`'s `x > 0` as `>=` (equivalent at 0: the branches are replaced by `max(x, 0)`, and
mutations M8, M13 re-run against the new form, killed); the `s` and `w` duration arms
deleted (a test of each added, each arm's deletion seen red by hand); `Saturate`/`Decay`'s
`depth + 1` as `* 1` or `- 1` (a depth test for them added, `* 1` seen red by hand).
