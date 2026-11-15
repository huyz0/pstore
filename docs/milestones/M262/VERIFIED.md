# M262 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
Two changes from the spec while building, both stated here: a vector type is accepted on
`vector` alone, of the create's width (a named field is refused, as no width is recorded
for it before a write); and the refusals' messages are the write parsers' own, so "unknown
key" reads as the type parser's refusal of an object it does not know.

How to run the tests: `cargo test -p pstore-server --test it create::`.

Red runs. Not run against the parent, which refuses `schema` as an unknown field (`deny_unknown_fields`).
Each test was seen red by the mutations below, applied by a script, the tests run, and the
mutation reverted. No `cargo mutants` run is claimed for them. All were killed.

1. `created_schema_is_recorded`: green; the summary from the creating server and from a
   second one over the same store, and a stemmed query after an undeclared write. Killed:
   further text fields not copied (C1); types not copied (C1b).
2. `created_schema_refuses_other_declarations`: green for another `k1`, another type and
   another regex set, and the same map accepted. Killed: the created analyzer left at its
   default (C2).
3. `created_text_only_index`: green. Killed: width 0 refused (C3); the metric refusal
   dropped (C3b).
4. `create_without_schema_is_m147s`, and M147's tests in `create.rs` unchanged: green.
   Killed: a schema-less create recording a regex attribute (C4).
5. `created_schema_refusals_match_writes`: green for six refusals, none committing. Killed:
   the type parser's errors swallowed (C5).
6. Gates: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features --
   -D warnings` pass; `scripts/gen-clients.py --check` passes after regenerating the clients
   for the create body's `schema`. The workspace suite: NOT-RUN at this commit; it is run
   once over the PR's last commit and recorded in that milestone's ledger. `cargo deny
   check`, coverage and the mutation sweep are NOT-RUN here; they are CI's on the PR.

## After code review (PR #63)

- **Fixed: a malformed `vector` declaration was silently dropped.** `{"vector": "f16"}`,
  `"uint64"`, `{"type": "[2]f16"}` and `"[2]f64"` created an index with the declaration
  ignored. Each is now refused `400`, as a write refuses it, and a width spelled `[02]` is
  accepted as a write accepts it. Four refusal cases and the padded width were added to
  `created_schema_refusals_match_writes`. Killed: the old filter (only `[..]`-shaped values
  read on `vector`) restored.
- **Strengthened: `create_without_schema_is_m147s`** now compares the whole recorded schema
  to a golden. The golden was taken from the parent build (4104569, before M262) by a probe
  test in a separate worktree, and it matched byte for byte. So a schema-less create that
  records any analyzer, declaration or further text field now fails it.
- `cargo test -p pstore-server --test it -- create:: tokenizers:: group_expanded::
  analyzer::`: 38 passed, 0 failed, after the fixes.

## After CI's mutation shard (PR #63)

- Three mutants in `created_schema` were MISSED: `&&` to `||` in the named-vector check, the
  `"f32"` arm deleted, and the no-dims guard made `false`. The named-vector check now reads
  a width between the brackets (`[2]f16`, not `[]string`, which the write's parser refuses
  with its own message). `created_schema_refusals_match_writes` adds a `[2]f32` create
  accepted with no `vector_types`, a `"[]string"` string refused by the write's parser, and
  a no-dims refusal. Red against d718c28 (the `[]string` case); each of the three mutants,
  and the new check made always true, applied by a script: killed.
