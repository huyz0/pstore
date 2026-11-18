# M274 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.
The SDK test is Python, so it is named without code marks: `check-verified.py` resolves only
Rust tests.

Tests: `cargo test -p pstore-server --test it facade` (18 passed: the six new and M268 to
M272's twelve, `facade_adds_no_request` unedited), `cargo test -p pstore-server --lib
attempt_memory`, and `FACADE_PYTHON=python3.13 ./scripts/facade.sh` (8 OK).

Red runs. Before the facade change, `a_resent_conditional_write_applies_once`,
`a_resent_by_filter_delete_applies_once`, `an_unmatched_resend_is_refused` and
`a_resend_without_keys_is_refused` failed. Mutations, each applied by a script and
reverted, all killed:
- (a) no key on a first attempt: the two resend tests failed.
- (b) a fresh key on a resend: the two resend tests failed.
- (c) an unmatched resend sent unkeyed: both refusal tests failed.
- (d) every applied write keyed (`affected_ids` alone included): `a_plain_write_is_not_keyed`.
- (e) no eviction, (f) the newest evicted, (g) `>` as `>=`: `the_attempt_memory_is_bounded`.
  ⚠️ Mutation (e) first hung forever against a `while` eviction loop, as CI's mutation gate
  would have reported it (a timeout). One body is added at a time, so the loop became an `if`.
- No keying at all, the M273 behaviour: the SDK test failed with id 2's `n` at 7, applied
  twice, not 1.

1. `a_resent_conditional_write_applies_once`: green. Row 2 patched to 7 once; another writer
   put it back to 1; the resend (count 1) answered `rows_patched: 1` and row 2 stayed 1.
2. `a_resent_by_filter_delete_applies_once`: green. Rows 1 and 2 deleted; row 9 of kind `a`
   written after; two resends (counts 1 and 2) answered `rows_deleted: 2`; row 9 kept.
3. `an_unmatched_resend_is_refused` (also: another tenant's first attempt of the same bytes
   matches nothing of this tenant's) and `a_resend_without_keys_is_refused` (keys off: the
   first attempt applies as before, the resend is refused): green, rows unchanged.
4. `a_plain_write_is_not_keyed`: green. An upsert, an upsert with `return_affected_ids` and a
   delete, each a resend never seen, answered 200.
5. `the_attempt_memory_is_bounded`: green. 4,096 bodies kept, the 4,097th evicts the first, a
   body remembered again keeps its place; digests differ by tenant and namespace.
6. `a_keyed_facade_write_costs_the_native_pair`: green; the facade's first attempt costs the
   native key issue and keyed write in reads, writes, LISTs and depth.
   `facade_adds_no_request`, unedited, green with keys on.
7. `./scripts/facade.sh`, its test_a_resend_after_a_lost_answer_applies_once: green. Its hook saw
   attempts `0` then `1`, and the row was patched once.

Gates: `cargo fmt --check` and `cargo clippy -p pstore-server --all-targets -- -D warnings`
pass. `openapi::error_codes_are_the_servers` found the new code and passes with it in
`api/openapi.json`; `scripts/gen-clients.py` regenerated both clients. Coverage, mutants and
`cargo deny` are CI's, NOT-RUN here.
