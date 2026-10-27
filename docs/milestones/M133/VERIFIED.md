# M133 verification

The spec was reviewed in two rounds, recorded in the [spec](SPEC.md).

How to run the tests:
- engine: `cargo test -p pstore-engine --test it ref_new::`;
- server: `cargo test -p pstore-server --test it ref_new::`, and the M13 suite with
  `cargo test -p pstore-server --test it patch::`.
- The runs below were made before the tests moved into `tests/it/` (one binary per
  crate), with `--test ref_new` and `--test patch`. After the move, both commands above
  passed again.

Red runs, against the parent:
- The engine tests did not compile (no `write_conditioned` or `patch_conditioned`).
- Every server test failed. Criteria 1 to 5 and 6 failed at their first `$ref_new` write,
  which the parent refuses as a malformed value. Criterion 7 failed at the new refusal
  text.
- ⚠️ Criterion 7's other refusals are guards, green on the parent, which refuses every
  object value. The mutations below are what turn them red.

The mutations were applied one at a time by a script and then reverted. No `cargo mutants`
run is claimed for them.

1. `newer_wins_for_an_upsert` (server): green after. Killed by "a reference always
   resolves as absent". `references_nest_and_compare` (server) checks every operator
   against a present value, equal and unequal, and references under `And`, `Or` and
   `Not`. Killed by "`Or` resolved as `And`", "`Not` dropped", "`Lte` as `Lt`", "`Gt` as
   `Lt`" and "`Gte` as `Lte`" (the last two added after code review found them surviving).
   - ⚠️ Drift: the spec names "resolve against the current row instead of the new one" for
     criteria 1, 2 and 6. It was not run as such, because the door has no current row to
     read; "a reference always resolves as absent" was run in its place.
2. `a_patch_references_what_it_sets` (server): green after. Killed by:
   - "a reference always resolves as absent";
   - "a patch's unset attributes read as present".
3. `absent_references` (server): green after. Killed by "an absent reference means `Eq
   null` under every operator".
4. `an_id_reference_is_the_operations_id` (server): green after. Killed by:
   - "`id` resolves to the attribute named `id`";
   - "the first operation's condition for every operation".
5. `repeated_ids_resolve_in_order` (server): green after. Killed by:
   - "the first operation's condition for every operation";
   - "the fold judges each operation against the folded row, ignoring this fold's
     earlier operations". This was observed directly: the 2-then-1 case answered
     `upserted 2`. M13's `operations_apply_in_fold_order` also kills it.
6. `adversarial_writers_never_both_win` (server; 100 writers over 4 servers, then a fifth
   server reads): green after, and green in three further runs. Killed by "a reference
   always resolves as absent".
7. `ref_new_refusals` (server): green after. Killed by:
   - "references accepted under every operator";
   - "`delete_condition` parsed as a template";
   - "no refusal beside a pattern clause";
   - "no refusal of an array";
   - "no refusal of a reserved name";
   - "a reference object may carry other keys".
   - `malformed_templates_are_refused` (server) checks a non-array `Or`, a non-string or
     reserved attribute, pattern, fuzzy and token clauses under `Not` and `Or` beside a
     reference, and a condition nested too deeply. Killed by "no refusal beside a pattern
     clause", "`Not` dropped" and "the pattern check does not look under `Not`".
8. `lengths_must_match` (engine): green after. Killed by "no length check". Also
   `each_operation_carries_its_own_condition` (engine), which checks upserts and patches
   that each carry their own condition.
9. The M13 suite, `operations_apply_in_fold_order` among it, passed unchanged.

Gates, in a cloud container: every workspace test ran under
`scripts/coverage.sh --all-features --no-fail-fast --fail-under-regions 95 --fail-under-lines 95`,
skipping M127's two `[::1]` tests (no IPv6 here): every test that ran passed; regions
95.06%, lines 96.63%, both floors held.
- A first measurement read 94.95% regions, with `ref_new.rs` at 70.90%. The two tests
  for nesting and malformed templates were added, and an unreachable arm of the pattern
  check was removed (a fixed part is one leaf clause). No floor or test changed.
- Every mutation above was re-run after that, by the same script, and killed.
- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and
  `cargo deny check` passed.
- `scripts/check-poison.sh`, `check-links.sh`, `build-index.py --check`, `check-slos.py`,
  `check-milestone-ids.py` and `check-portable.sh` passed.
- `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed.
- NOT-RUN here: `scripts/mutants.sh` (`cargo-mutants` is not installed in this container).
