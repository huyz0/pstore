# M126: reject unknown public request fields

## Serves
D-34 explicit API tradeoffs and cost accounting; D-73 query semantics.
First slice of [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
Close the native write/query request roots and each document envelope to unknown
fields, using their existing serde definitions as the allowed-field source.
Keep existing strict multi-vector, external, replication, schema and query-option
parsers. Attribute names, named vector fields, patch columns and aggregate labels
remain user data, not closed option objects. Report a typed `bad_request` (400)
that identifies the unknown field, before engine access or mutation.
Apply the same policy through typed fast parsing, JSON-tree fallback, and multi
children. Duplicate-key last-value behavior stays as M120 specifies it.
Native batched/eventual defaults, valid results, score meanings, stored formats,
and the separate `unsupported_durability` error remain unchanged. Authentication,
admission limits and broader compatibility remain subsequent delivery work.
Migrate the replication test fixture's previously ignored root `n` into attributes
and assert that replication preserves its value; do not relax the new validation.

## Acceptance criteria
1. Unknown write-root, document-envelope, query-root and multi-child keys are refused with 400 `bad_request` naming the field, including duplicate-key fallback bodies.
2. A refused write performs zero blob requests/bytes and changes no buffered rows, session token, or index listing; valid rows preceding a malformed document are not accepted.
3. Unknown nested option keys remain refused for schema, patch/filter-patch, multi-vector, fusion, boost, diversify, highlight and facets. Arbitrary attribute/vector names and aggregate labels stay valid.
4. Existing valid request and duplicate-key fixtures, native defaults, cost/depth tests and server suite pass. Unsupported vendor safety/output keys cannot be silently accepted.
5. Record valid and refused request CPU/allocation measurements against the preceding revision; all timing numbers are provisional and no speedup is claimed from one run.

## Test plan
1. `unknown_request_fields_are_refused_before_engine_access`: add valid requests carrying one extra field at each newly closed envelope and in typed/fallback/multi paths; observe successful acceptance on M125/M126's parent. Mutation: drop the unknown key.
2. `a_refused_batch_leaves_rows_session_and_registry_unchanged`: seed state, insert a valid row before an invalid document, compare counters then query/list state and session. Mutation: validate after buffering the first row.
3. `nested_options_are_closed_but_data_names_are_open`: inject extra keys into existing nested options and exercise user-defined data names. Mutation: skip an unknown nested option or close a user-data map.
4. Existing `the_fast_parse_is_the_tree_parse`, server integration tests, query cost/depth fixtures and the workspace gates cover valid behavior. Add explicit valid duplicate/default controls beside negative fixtures.
5. Use a bounded ignored parser benchmark with a fixed input corpus, a counting allocator in an external measurement harness if needed, repeated iterations and output digests. Test-first: observe the invalid corpus accepted before the change. The measurement harness is not a shipping crate or a new CI threshold.
Closed serde envelopes are gate-design rung 1; behavioral rejection/state tests are rung 3.

## RA budget
Validation adds 0 W / 0 Rseq / 0 Rpar / 0 LIST and zero transferred bytes.
Existing write and cold-query budgets are unchanged; no stored-data migration.

## Risks
Clients that relied on ignored keys now receive an explicit error; publish the migration
note. An additional full JSON materialization on valid vector requests would regress
M119/M120 parsing work; preserve the typed fast path and measure it. Diagnostic precedence
can differ for multiply-invalid input; the named durability refusal remains protected.

## Tasks
- M126.1 Close public envelopes, add rejection/state controls, and record verification and parsing measurements.
