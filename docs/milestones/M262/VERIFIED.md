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
