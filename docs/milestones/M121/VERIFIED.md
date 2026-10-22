# M121 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, 4 cores, the debug profile with
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0`; the bench in
release. `provisional` (AGENTS.md). NOT-RUN on macOS and Windows.

1. `a_multi_field_scan_reads_each_fields_vectors_once` passes: every field's `Vectors` span
   is read exactly once. **Red first** at 480 bytes for 240. `a_multi_field_scan_reads_its_fields_together`
   passes at 1 round, **red first** at 2.
2. `a_stat_is_one_head` passes: one HEAD, the answers `head` and `get_tag` give, the same
   refusal for a missing object. **Red first** at 2 HEADs, the default.
   `a_column_check_is_one_request_an_object` passes: 3 checks for 3 objects, **red first** at
   6 with the check as two calls.
3. `a_scan_is_served_from_bulk_and_admits_nothing` and `a_cached_range_owns_its_bytes` pass.
   The copy skipped for a scan or an over-quota range is visible to no test: checked by
   reading `admit_all`.
4. `concurrent_batched_misses_issue_one_request_a_range` passes: eight racing `get_ranges`
   reach the store as one call alone does. **Red first** at 24 requests for 3.
5. `rows_at_builds_only_the_rows_asked` passes, with `a_row_asked_twice_comes_back_twice`
   and the rest of the suite: `cargo test --workspace --all-features`, 255 test binaries, all
   ok. **Red** with a skipped row's attributes not read through.
6. `./scripts/query-bench.sh`: every digest equals the baseline's -- `47539a4b450befe3`,
   `64d349ed03f0a2f6`, `65559801f84a1412`, `b06b396d0aa759fb`, `60cf6bfe1f9fc517`,
   `d559aff4b3101b72`, `dd1bf0796e2ba34f`. One run, p50 dense 2.42 ms, text 1.51, hybrid
   3.51: no time is claimed from it, since none of the deltas is on the bench's query path
   beyond the cache's batched read.
7. `./scripts/gates.sh` passed, run by the pre-commit hook on this commit.

## Every finding

[FINDINGS.md](FINDINGS.md) lists all twelve findings: five fixed, four deferred with a reason,
two rejected, and one kept for its test's sake (an external column checks every object).
