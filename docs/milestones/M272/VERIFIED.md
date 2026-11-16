# M272 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

The SDK test (clients/turbopuffer/test_sdk.py: test_the_manifest_names_everything_the_sdk_sends)
is Python, so it is named without code marks: `check-verified.py` resolves only Rust tests.

The manifest, `api/compat/turbopuffer.json`, holds 17 operations (16 SDK namespace methods
and `namespaces`), 21 write keys and 14 query keys. One operation is refused
(`update_schema`). Of the write keys, `create_namespace`, `encryption` and `sharding` are
refused and `disable_backpressure` is a hint. Of the query keys, `compute_attributes`,
`distance_metric`, `offset` and `rerank_by` are refused.

⚠️ Writing the evidence found that no test called `schema()` directly. Adding the call found
that it answers `{}` for a namespace with a `title` text attribute. That is the native
summary's `schema.attributes`, which records a type only for datetime, uuid, bytes and
uint64 attributes (M173). The facade passes it on faithfully, but text and numeric
attributes are not listed. The SDK test now checks `schema()` equals the native attributes,
and the manifest's `schema` note says what is missing. Listing them would be a translation
change, not part of a manifest.

Red runs. Each was an edit to the manifest, run and reverted by a script.

1. The manifest is complete against the SDK. test_the_manifest_names_everything_the_sdk_sends,
   run alone under the pinned SDK, failed with the hint_cache_warm operation removed, and
   failed with the offset query key removed. Unedited, it passed. `./scripts/facade.sh` ran
   it with the rest of the suite: 7 tests OK.
2. The manifest is the code:
   - `the_manifest_is_the_facades_routes` failed with `recall`'s entry removed (its route is
     then unlisted), and failed with an extra entry for a route nothing registers.
   - `the_manifest_keys_are_the_translation` failed with `deletes` marked refused, and failed
     with `rerank_by` marked served. It was restructured for clippy (a `panic!` arm became
     assertions), and both edits were run again on the new version: both failed.
   - `the_manifest_evidence_resolves` failed with an SDK test name misspelt.
   - Unedited, all three pass, by `cargo test -p pstore-server manifest`.
3. Gates:
   - `cargo fmt --check` passes.
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings` first failed on
     the `panic!` above, then passed.
   - The workspace suite, `cargo test --workspace --no-fail-fast -j 3`, after the clippy fix:
     56 test binaries, 2,937 passed, 0 failed, 15 ignored. The schema assertion and the
     manifest's two notes changed after it, and `cargo test -p pstore-server manifest` and
     `./scripts/facade.sh` (7 OK) ran again after them.
   - The two IPv6 tests were NOT-RUN, as in M267 to M271, because this container has no IPv6
     loopback.
   - `scripts/check-verified.py`, `scripts/check-links.sh`, `scripts/build-index.py --check`
     and `scripts/check-portable.sh` pass.
   - `cargo deny check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on
     the PR.
