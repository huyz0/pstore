# M143 verification

Implemented in `crates/pstore-server/src/ingress/shape.rs` (the scan), `ingress.rs` (four
version-1 fields of `RequestLimits`, their configuration, and the refusal in `admit`
after body collection and before the handler) and `lib.rs` (`checked_query`'s `top_k`
bound). All runs below are in a 4-core cloud container and every number is
`provisional`.

How to run the tests: `cargo test -p pstore-server --lib shape::` and `--lib ingress::`
for the scan and the configuration, and `cargo test -p pstore-server --test it shape::`.

The scan is one pass over the collected bytes with a fixed per-level array of 129 counts:
no allocation. It tracks string and escape state, counts the elements of each open array
and the members (keys) of each open object, and stops at the first breach. It does not
validate JSON; a body it passes goes to the parsers unchanged.

⚠️ `json_depth` defaults to 128, not the reviewed 32: at 32 the workspace suite failed
`patch::what_a_patch_cannot_mean_is_refused` and `ref_new::malformed_templates_are_refused`,
which nest a condition 80 deep to reach the condition parser's 64-level cap. The spec was
amended on main (8fedb74) before this commit; no test was changed.

Red run (.harness/m143-red.log, and .harness/m143-red-scan.log for the scan): the scan's
unit tests failed against a scan that passed everything, and with the configuration in
place but no scan in `admit` and no `top_k` check, `shape_limits_are_exact`,
`a_breach_is_refused_before_parsing_on_every_route` and `query_top_k_is_bounded` failed.
`existing_caps_keep_their_refusals` passed, as it should before the change; its red is the
"top_k overrides the existing caps" mutation below. The configuration tests were written
before the fields and observed failing to compile; their red is the mutations below.

Mutations, one at a time by script and reverted (.harness/m143-mutations.log), each running
the `pstore-server` lib and integration tests. All ten were killed: the scan never run, an
escape ignored, a count allowed one past its bound, depth admitting one more level, objects
counted as arrays, `top_k` unbounded, `top_k` overriding the existing caps, the `top_k` bound applied to a facet sub-request, `json_depth`
not validated, and `string_bytes` not read. No `cargo mutants` run is claimed.

1. PASS: `shape_limits_are_exact`. For each field, on a write and on a query, with small
   configured limits: the body one past the limit, sent first on a fresh `Api`, is `413
   request_too_large`, not retryable, its message names the field, `engines()` is 0 and the
   store's request counters are 0; the body exactly at the limit is then admitted (200 for
   the write, 404 for a query of an absent index).
2. PASS: `a_breach_is_refused_before_parsing_on_every_route`,
   `depth_counts_open_arrays_and_objects`, `elements_and_members_count_one_container_each`,
   `strings_count_their_raw_bytes`, `brackets_and_quotes_inside_strings_are_text`,
   `a_breach_is_reported_whatever_follows_it`. A too-deep body that is not JSON after the
   breach is refused for its depth on the write, query, warm and external query routes;
   brackets, commas and escaped quotes inside strings are text, an escaped backslash ends
   its escape, `{}` is depth 1 and a scalar body 0.
3. PASS: `query_top_k_is_bounded`, `existing_caps_keep_their_refusals`. A vector, text or
   multi-query leg with `top_k` 10,001 is `400 bad_request`; 10,000 is answered. A
   `rank_by` and a facet `top_k` of 10,001, 17 queries and 9 named fields keep their
   existing 400 messages.
4. PASS: `unsafe_or_malformed_configuration_is_refused`, `configuration_lowers_each_limit`,
   `version_one_defaults_are_the_documented_contract`,
   `request_limit_configuration_precedes_store_and_bind`. Each field is refused when zero,
   above its default, misspelled or malformed; the binary refuses `json_depth` 129 before
   the store configuration and before bind.
5. PASS, with the IPv6 exception M127 records. `cargo clippy --all-targets --all-features -- -D warnings`
   passed. The workspace suite (all features, both `[::1]` tests skipped: the
   container has no IPv6; they run in CI) passed 2,326 tests with none failing.
   `./scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed at 97.20%
   lines / 95.78% regions, `shape.rs` 100% lines (.harness/m143-coverage.log).
   `scripts/recall.sh`, `scripts/ndcg.sh` and `scripts/depth.sh` passed
   (.harness/m143-quality.log).
6. PASS: `refused_shapes_allocate_little` (release, rustc 1.90.0,
   [measurements/shape-allocation.csv](measurements/shape-allocation.csv)). Each 2 MiB
   body sent as one chunk, measured from the request's dispatch to the end of the
   response: a wide array, a wide object, a long string and deep nesting are each refused
   having allocated about 10.5 KB, under the 64 KiB bound the test asserts (the window
   includes collection, which keeps one chunk without copying). The same bodies before
   this change were not measured. An admitted 2 MiB write of 64-component rows
   allocated 11.1 MB, 5.3 times its body.

Code review: see the commit message.
