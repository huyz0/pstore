# M128 verification

Implemented in `crates/pstore-server/src/ingress.rs`: a route layer inside each route's
scope layer, so authentication, then scope, then content-encoding, declared length,
admission and body collection run in that order before the handler. One `Ingress` per
`Api`, shared by every router it builds. All runs below are in a 4-core cloud container
and every number is `provisional`.

How to run the tests: `cargo test -p pstore-server --test it ingress::`, and the limit
and permit unit tests with `cargo test -p pstore-server --lib ingress::`.

Red run (.harness/m128-red.log): the layer first landed as a pass-through stub with the
limits, constructors and configuration real. Against it the byte, authorization, capacity,
recovery and cohort tests failed (the 413 was axum's untyped refusal, `error.code` null),
and the encoding and deadline tests hung on bodies that never end. The configuration test
and the constructor test passed against the stub, because those parts were not stubbed;
their red is the mutation below.

Mutations, one at a time by script and reverted (.harness/m128-mutations.log), each
running the `ingress::` integration tests. All six were killed: "the body may be one byte
over" (criterion 1), "a permit's drop releases nothing" (criteria 3 and 4), "content
encoding is ignored" (criterion 2), "the ingress layer is outside the scope layer"
(criterion 2), "one counter for every identity" (criterion 3), and "limits are read after
the store configuration" (criterion 5).

CI's `cargo mutants` sweep on the PR then reported 12 TIMEOUTs. Locally the whole
workspace suite took ~300 s against the sweep's 300 s timeout (provisional), so survivors
were cut off rather than reported, and the limit mutants in `collect` could read the old
endless test body forever. Fixed by killing or removing each: every `serve` now asserts
`request_limits()` returns what the `Api` was given; a configuration of exactly 4096 bytes
is read; `key_of` reads only `x-pstore-tenant`, which authentication already rewrites to
the verified tenant, so its `Principal` arm was an equivalent mutant; `collect` is
`http_body_util::Limited` under one timeout, removing single-chunk bookkeeping whose guards
were equivalent; and the over-limit body is finite. Six of those mutants, applied one at a
time against the `pstore-server` lib and integration tests (.harness/m128-mutations-r2.log),
were all killed. The CI sweep is the full check; no local `cargo mutants` run is claimed.

1. PASS: `ingress_byte_limits_precede_engine_access`. A write of exactly the limit
   succeeds; one byte over is `413 request_too_large` as one chunk, as one-byte fragments
   without a length, and under a lying `Content-Length: 10`. A declared length over the
   limit refuses without polling the body. A finite body many times the limit is read at
   most one chunk past the limit. No refusal changes the store's request counts.
2. PASS: `authorization_precedes_body_polling_and_admission`,
   `unsupported_encoding_never_reaches_json`. Missing, invalid and wrong-scope bearers are
   401/401/403 with the body never polled and nothing admitted. The verified tenant is the
   key: tenant 9 at its one slot is refused 429 while tenant 10 is admitted.
   `gzip`, `br` and `identity, deflate` are 415 `unsupported_content_encoding` unpolled;
   `identity` is served. Existing identity and refusal semantics: the M127 suites
   (`auth_contract::`, `auth_baseline::`, `auth_sources::`) pass unchanged in the
   workspace run below.
3. PASS: `ingress_capacity_is_shared_and_tenant_scoped`. Across two routers of one Api,
   the tenant limit, the global limit and the body pool each refuse with 429 `overloaded`,
   `Retry-After: 1` and `retryable: true`; a second tenant is admitted while the first is
   at its own allowance.
4. PASS: `ingress_permits_recover_after_all_exit_paths`,
   `a_body_that_misses_its_deadline_is_refused_and_released`,
   `headerless_private_requests_share_one_cohort`, and the unit tests
   `a_permit_returns_everything_and_idle_tenants_leave_no_entry` and
   `a_poisoned_count_is_recovered`. Completion, a handler refusal, a body error part way,
   an aborted request and a missed deadline (408 `request_timeout`, paused clock) each
   return every slot and byte; 200 distinct tenants leave no entry; four held requests are
   four entries; a later request succeeds. Headerless private requests share one cohort.
5. PASS: `request_limit_configuration_precedes_store_and_bind`,
   `bare_and_authenticated_constructors_keep_the_default_limits`, and the unit tests
   `version_one_defaults_are_the_documented_contract`,
   `configuration_lowers_each_limit`, `unsafe_or_malformed_configuration_is_refused`.
   The binary with an invalid `PSTORE_REQUEST_LIMITS_CONFIG` refuses before it reports
   an invalid store configuration and before bind; valid configurations reach bind.
6. PASS, with the IPv6 exception M127 records. Clippy `--all-targets --all-features -D
   warnings` passed. The workspace suite (all features, both `[::1]` tests skipped: the
   container has no IPv6; they run in CI) first caught two fixtures that raced more of one
   tenant's writes than the new 8-per-tenant default admits:
   `adversarial_writers_never_both_win` and
   `each_of_many_concurrent_durable_writes_is_required_of_another_process` got 429.
   Both now retry `429 overloaded`, which is refused before dispatch and changed nothing;
   their assertions are unchanged. `scripts/coverage.sh --fail-under-lines 95
   --fail-under-regions 95` then ran every workspace test with none failing and passed at
   97.11% lines / 95.66% regions (`ingress.rs` 98.92% / 98.10%) (.harness/m128-coverage.log).
   `scripts/recall.sh`, `scripts/ndcg.sh`, `scripts/depth.sh` passed
   (.harness/m128-quality.log); the remaining fast gates, including `cargo deny check`,
   passed (.harness/m128-fast.log). Measurement: `ingress_cpu_allocations_reservations_and_refusal_digests`
   (release, rustc 1.90.0, [measurements/ingress-release.csv](measurements/ingress-release.csv)).
   The valid case is a well-formed 4 KiB query of an absent index (404), so it measures
   ingress and the handler without engine state: ~21.7-24.1 us, 115 allocations and
   35,405 bytes per request, the same as the same server at the default limits. Oversized
   (413) ~8.0-8.5 us / 78 allocations; saturated (429) ~7.4-7.9 us / 80; cancelled
   ~9.5-12.0 us / 69. Response digests are identical across samples. Peak reservation was
   8,192 bytes (two 4 KiB bodies), and the store's request counts did not change across
   the oversized, saturated and cancelled cases.

Code review round 1 (an independent agent, read-only) found no blocking or major code
finding. Its one major was that this ledger was still the pre-implementation one; this
file is the fix. Of its minors, two were fixed (the configuration test now also proves the
order against the store configuration, and the bearer test now shows per-tenant keying
rather than the global limit); the other two are left: the M127 row of DELIVERY.md is
marked verified because M127 is on main (647a66c), and a `Transfer-Encoding` header is
left beside the rewritten `Content-Length`, which nothing downstream reads.

Not claimed: decoded JSON or typed-value memory, response bodies, header and network
buffers, query plan reservations, buffered tenant bytes and flush reserve. Those remain
workstream A work after this slice.
