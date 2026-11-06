# M195 verification

The spec was reviewed in one round, recorded in the [spec](SPEC.md), and amended while the
tests were written (recorded there). Code review is recorded in the commit body.

How to run the tests: `cargo test -p pstore-server --test it -- vector_types::` and
`cargo test -p pstore-format --test it -- vector_types::`.

Red run: on the parent, with the tests and none of the source, a vector type in `schema` is
`400`, so the five server tests failed there (5 of 5). The format tests call what the parent
does not have (`Kind::F16`, `component`, `vector_key`) and do not compile on it. Then each
mutation below, applied one at a time by a script and reverted: 23 killed. Two survived at
first: a named field's answered width taken as stored (euclidean's extra component left on)
and `[N]f32` recording a type; the refusal test's closing cases were added and kill both. No
`cargo mutants` run is claimed.

1. `vector_types::a_declared_field_holds_and_scores_its_types_values` (f16 and i8, under dot
   product, euclidean and cosine: 450 rows of width 16 with ties, subnormals, 65,504 and
   ±65,519, folded in two segments, 50 unfolded, read from that process and a restarted one;
   20 exact queries each against an undeclared index of the oracle-rounded rows, ids and
   scores bit-equal), `vector_types::named_and_multi_vector_fields_take_a_type` (a named
   field and a multi-vector field declared f16, projected, and queried by a dense leg and a
   `multi` leg against the oracle-rounded index) and
   `vector_types::components_round_to_f16_or_must_be_i8_integers` (format): green. Killed:
   f16 truncated rather than rounded, the bound checked before rounding, the bound a step
   short, i8's negative zero kept, the field normalized before it is rounded.
2. `vector_types::what_a_type_cannot_hold_is_refused_naming_the_rule`: green. 65,520 and
   -65,520 under f16; 3.5, 128 and -129 under i8; a declared width other than the vectors';
   a vector type on an attribute, on `sparse`, on a field no document carries and on
   `vector` in a write with none; an attribute type on a vector field; a vector kind declared
   for an attribute; base64 f32 vectors refused under both types; each `400` naming the rule,
   nothing stored. A base64 f32 vector rounded under f16, `upsert_columns` under i8 admitted
   and refused, `[N]f32` neither rounding nor recording, and a named field's type answered at
   its written width under euclidean. Killed: the i8 integrality and range checks removed,
   the width unchecked, `sparse` admitted, an attribute type on a vector field admitted, a
   vector type on an attribute admitted, a vector kind admitted as an attribute's, a field
   no document carries admitted, `[N]f32` recording f16, the answered width taken as stored.
3. `vector_types::a_recorded_type_holds_for_every_later_write` and
   `vector_types::the_vector_kinds_take_the_next_codes_and_are_no_attributes` (format):
   green. After the declared write folds, vector-only undeclared writes from that process and
   a fresh one are rounded, and one past the bound refused; another type is `400
   schema_conflict` naming `vector`, f16 and i8; a process that read HEAD before the type was
   recorded writes an unrounded row, which the fold quarantines with a reason naming
   `vector` as f16 and as f32; `schema.vector_types` answers the field and
   `schema.attributes` is `{}`; two processes declaring f16 and i8 before one fold keep lane
   1's and quarantine the other's row. Killed: the types read from the declaration only, a
   missing stamp admitted, no stamp written, `$vector.` names answered in
   `schema.attributes`, the conflict named as an attribute's, `vector_types` answered without
   its width. And (code review found the path, then the cases were added) `[N]f32` declared
   on a field recorded f16, or unfolded i8, is `400 schema_conflict` naming both, after one
   HEAD re-read that lets the f32 declaration through once the typed index was dropped by
   another process. Killed: the f32 conflict missed, the re-read skipped.
4. `vector_types::a_declared_index_costs_what_an_undeclared_one_does`: green. A write, its
   fold, a fresh process's first vector-only write and a cold query cost the same depth and
   requests declared f16, declared f32 and undeclared, pinned at (5, 13), (6, 13), (5, 13)
   and (4, 4). Killed by it among others: the types read from the declaration only.

Gates on the final tree: `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
clean. The workspace suite, without the two IPv6 tests: 2,693 passed, none failed.
`scripts/coverage.sh --changed --fail-under-lines 95 --fail-under-regions 95
--ignore-run-fail` with those skips, after `cargo llvm-cov clean`: lines 97.29%, regions
95.84% (`provisional`: a 4-core cloud container). CI's coverage and mutants runs on the PR
are the gate.
