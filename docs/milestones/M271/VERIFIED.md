# M271 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

The SDK suite (clients/turbopuffer/test_sdk.py: test_copy_branch_and_the_async_copy and the
five from M268 to M270) is Python, so it is named without code marks: `check-verified.py`
resolves only Rust tests.

The spec changed while it was built:
- **The wire forms.** The first spec keyed branch and copy on a top-level
  `source_namespace` under the SDK's `?stainless_overload=` query, as the SDK's generated
  resource methods read. The pinned SDK's own requests, captured with an HTTP hook, showed
  something else. Its namespace wrapper sends `branch_from_namespace` or
  `copy_from_namespace` holding `{"source_namespace", …}` for `branch_from`, `copy_from`
  and `start_copy_from` alike. The facade, its tests and the spec now key on those, and
  ignore the query.
- **A failed run.** The native copy refuses a missing source with a 404 before it starts,
  so no failed run can be reached in process. The in-process test checks that refusal
  against the native one, and a unit test covers every native state.
- **The finished result.** The SDK's `WriteResult` requires `billing`, which the facade does
  not invent. So the SDK hands back the finished copy's `result` as a dict, and the SDK test
  reads it that way. This is listed in the spec's risks.

Red runs.
- With the new tests written and no facade change, `facade_branch_and_copy_are_the_native_branch`
  failed (`branch_from_namespace` refused as untranslated), and
  `facade_async_copy_is_the_native_copy` and `facade_adds_no_request` failed with a 404 on
  the async route.
- `an_operation_is_the_native_copys_state` was written with the code. Every assertion in
  it was seen red by the mutations below.
- The new SDK test was not run against M270's facade, which refuses every form it sends;
  the in-process red run covers the same refusals.

1. The SDK runs against pstore (`./scripts/facade.sh`): green, 6 tests OK. The new test
   checked:
   - branch_from answers rows_affected 0 and makes a namespace that reads the source's rows;
   - copy_from does the same, and a patch to the copy leaves the source's rows unchanged;
   - write(copy_from_namespace=...) does the same;
   - start_copy_from answers a token, and poll_copy_from reached finished with a success
     result on the real server's own replication worker, well within its 60 s bound, after
     which the copy reads the source's rows;
   - poll_copy_from with another token is refused NotFoundError;
   - copy_from with source_region is refused BadRequestError, naming it.
2. Translation, in process: `facade_branch_and_copy_are_the_native_branch`,
   `facade_async_copy_is_the_native_copy` and the extended `facade_adds_no_request`, with
   the seven earlier facade tests, by `cargo test -p pstore-server --test it facade`: green,
   10 passed. `an_operation_is_the_native_copys_state` and the six other unit tests in
   `src/facade.rs`: green, 7 passed. M268's refusal case and the SDK's refusal test now
   refuse `sharding`.

   The mutations and the tests that killed each:
   - `durability` kept beside the branch: `facade_branch_and_copy_are_the_native_branch`
     and `facade_adds_no_request` (the native write refuses a branch that is not alone).
   - The source's `source_region` check removed: `facade_branch_and_copy_are_the_native_branch`
     and `facade_async_copy_is_the_native_copy`. Both match the facade's "is not served"
     wording, because the source's unknown-key check behind it also names the key.
   - The token taken from `epoch` instead of `run`: `facade_async_copy_is_the_native_copy`.
   - A token mismatch answered as the copy: `an_operation_is_the_native_copys_state`.
     The token check was rewritten for clippy (the run compared as a number, not a string)
     after the first mutation run. The same mutation, `if false`, was applied to the new
     check and run again with `--no-fail-fast`: `an_operation_is_the_native_copys_state`
     and `facade_async_copy_is_the_native_copy` both failed.
   - `failed` answered `running`: `an_operation_is_the_native_copys_state`.
   - `status_code` 409 for `source_not_found`: the same test.
3. Gates:
   - `cargo fmt --check` passes.
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings` first failed on a
     collapsible `if` and on the token compared by building a string. Both are fixed: the
     token is parsed and compared with the run as a number. Clippy then passes.
   - The workspace suite, `cargo test --workspace --no-fail-fast -j 3`, after the clippy
     fixes: 56 test binaries, 2,934 passed, 0 failed, 15 ignored. `./scripts/facade.sh` ran
     after it: 6 OK.
   - The two IPv6 tests were NOT-RUN, as in M267 to M270, because this container has no IPv6
     loopback.
   - `scripts/check-verified.py`, `scripts/check-links.sh`, `scripts/build-index.py --check`
     and `scripts/check-portable.sh` pass.
   - `cargo deny check`, coverage and the mutation sweep were NOT-RUN here; CI runs them on
     the PR.
