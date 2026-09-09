# M0a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md), naming the test or command that
demonstrated it. This **enumerates**; it does not certify the evidence is true beyond what
is recorded here, and a line naming something not actually run would violate the "never
claim a gate ran without running it" non-negotiable.

Gate: `scripts/check-verified.py`.

1. `create_if_absent_rejects_second_write`, `cas_rejects_stale_tag`,
   `cas_against_a_deleted_object_is_refused`, `cas_with_a_fabricated_tag_is_refused`,
   `cas_lost_and_contended_are_distinct`, `get_range_returns_the_requested_slice`,
   `delete_batch_removes_only_the_named_keys`. `cargo test -p pstore-blob`.
   Mutations verified killed: ignoring the precondition (both creates succeed); accepting
   any tag (fencing gone); mapping 409 to `Lost` (`should_rebase` would return true).
2. `accounting_counts_one_write_per_put`, `accounting_separates_read_write_and_list_classes`,
   `accounting_is_per_tenant`, `a_failed_conditional_write_is_still_billed`.
   `cargo test -p pstore-blob --test accounting`. Mutations verified killed: counting
   bytes rather than requests; folding LIST into the read class; summing all tenants into
   one bucket; billing only successful conditional writes.
3. `coalescing_merges_ranges_closer_than_the_gap`,
   `coalescing_respects_the_gap_threshold`, `coalesced_reads_equal_separate_reads`,
   `coalescing_actually_saves_requests`, `ranges_out_of_order_are_returned_in_the_order_asked`,
   plus `coalesce::tests::*` for the arithmetic traps.
   `cargo test -p pstore-blob --test coalescing`. `coalescing_actually_saves_requests`
   asserts **through the accounting counter** that eight nearby ranges cost one request,
   so "merging" cannot be claimed without the saving being real. Mutations verified
   killed: never merging; merging unconditionally; returning the merged buffer rather
   than each range's slice.
4. **Met**, latency included — ⚠️ **closed later than the rest of this ledger; see below.**
   `same_seed_reproduces_the_same_failures`, `a_zero_rate_injects_nothing`,
   `each_fault_kind_is_injected_independently`,
   `an_injected_write_fault_does_not_reach_the_backend` for 412, 409, 503 and read/write
   errors; `latency_is_injected_inside_its_bounds`, `zero_latency_sleeps_not_at_all`,
   `the_same_seed_reproduces_the_same_delays`, `an_inverted_range_delays_by_the_minimum` and
   `latency_is_independent_of_which_operations_fail` for the fourth kind.
   `cargo test -p pstore-blob --test faults`.
   ⚠️ **"Independently" is the load-bearing word, and it needed a second random stream.**
   The delay is drawn from a SplitMix64 stream of its own, so turning latency on cannot change
   *which* operations fail. Observed red by drawing it from the fault stream instead: the two
   30-operation failure sequences diverge from the third operation on, and **every other test
   in the file still passes**, because each one varies only a single fault kind at a time.
   Observed red also by removing the sleep, and by inverting the `max(lo)` clamp.
   ⚠️ Removing the `is_zero()` guard in `Faulty::delay` is a **proven equivalent mutant** and
   survives: a sleep whose deadline has passed is `Ready` on its first poll, so it is not
   observable from outside. Recorded beside the test.
5. `slowdown_reduces_concurrency_then_recovers`, `retries_are_bounded`,
   `a_transient_slowdown_is_retried_and_succeeds`, `every_method_retries_a_transient_slowdown`,
   `a_non_slowdown_error_is_not_retried`, `a_cas_failure_is_never_retried_by_the_transport`,
   `the_limit_never_reaches_zero`.
   `cargo test -p pstore-testkit --test hostile` and `-p pstore-blob --test faults`.
   Mutations verified killed: never reducing the limit; never recovering it; retrying
   forever on a persistent 503; letting the limit reach zero; retrying a CAS at the
   transport layer.
6. `the_memory_store_conforms`, `every_decorator_conforms`,
   `the_suite_detects_a_backend_that_is_not_fenced`, `a_report_names_what_diverged`,
   `the_suite_survives_a_backend_that_fails_everything`,
   `the_suite_survives_a_backend_that_only_fails_writes`,
   `the_suite_survives_a_backend_that_only_fails_conditional_writes`,
   `conformance_costs_a_bounded_number_of_requests`.
   `cargo test -p pstore-testkit`. A `ContentHash` backend is recorded
   `Divergent` on `aba_resistance` **while still declaring `cas: Supported`** — measured
   beating declared, which is the property the suite exists for. Mutation verified killed:
   deriving the observed profile from `capabilities()` rather than from the probes.
7. `sweep_reports_a_curve_not_a_point`, `a_single_writer_never_contends`,
   `a_point_with_no_commits_is_infinite_not_a_divide_by_zero`,
   `the_rendered_table_carries_its_own_caveat`. `cargo test -p pstore-testkit --test sweep`.
   The curve itself: `cargo run -p pstore-testkit --example contention` →
   1 writer 1.00 attempts/commit, 128 writers 10.95, success falling 100% → 9.1%.
   ⚠️ **PROVISIONAL**: our protocol against an in-process store, not S3. The shape is the
   deliverable; the position of the real operating point is M0b.
8. `./scripts/gates.sh` — all eight green. `cargo llvm-cov --workspace --all-features`
   → **95.43%** region coverage (floor is 95%). Unsafe audit:
   `grep -rlE '^\s*unsafe_code\s*=\s*"(allow|warn)"' --include=Cargo.toml .` returns
   nothing outside `pstore-kernel`, which does not yet exist — and `unsafe_code = "forbid"`
   at the workspace root was verified to be a hard compile error by inserting an `unsafe`
   block.
   `cargo mutants --workspace --all-features` → **152 caught of 198 viable = 76.8%**,
   263 mutants in 73 s.
   ⚠️ **Below the 80% target** for this tier (D-111). Not met.
   ⚠️ And a precision point: `cargo mutants` injects *its own* mutations, so the phrase
   "mutation verified killed" in criteria 1–7 above means **the named mutation was
   reasoned about**, not that it was individually injected. Where the two overlap the
   tests held; where they do not, the claim is weaker than the words suggest. The
   remaining 46 are enumerated in **M0a.11**.

## What was found rather than built

Three things the milestone learned, which is what its exit condition asked for:

- **The conformance suite found a real divergence on its first run against a foreign
  backend.** HTTP `Range` may answer an overrunning range with `206` and the bytes that
  exist — a **short read, not an error** — and `object_store` passes that through. Our
  trait promises the requested bytes or an error, because a silent truncation surfaces
  layers away as a recall bug. Fixed in the adapter
  (`a_short_read_is_an_error_not_a_truncation`), and it is exactly the class of finding
  that could not have come from reading documentation.
- **`object_store` has no typed variant for S3's `409 ConditionalRequestConflict`.** It is
  recovered by matching the error message (`map_cas_err`), which is a string match on a
  third-party error. Directly tested, but confirming it against a real 409 is an **M0b
  exit item**, not an assumption.
- **Mutation testing caught a test I had weakened to satisfy a lint.** Clippy's
  `float_cmp` rejected `assert_eq!(rate, 1.0)`, and I replaced the ratio assertions with
  integer ones — which silently stopped testing `attempts_per_commit` and `success_rate`
  at all. `/` swapped for `*` then went unnoticed. That is precisely the "never weaken a
  test to make a check pass" non-negotiable, violated by me, and caught by the gate that
  exists because coverage cannot see it: those lines stayed 100% covered throughout.
  Restored with epsilon comparisons.
- **The trait shape survived contact with a real backend**, which was the milestone's
  first stated risk. Two adjustments were needed: `ObjectStoreExt` carries `get`,
  `get_range` and `head` rather than the base trait, and `delete_batch` currently issues
  one request per key rather than using `DeleteObjects` — so GC would cost 1000× what it
  should. Recorded, not fixed: M0a's job was to learn whether the trait fits.

## Exit condition

> *We know what would break and at what threshold, plus a recorded capability profile per
> backend.*

**Met for the threshold half** (criterion 7's curve, and the sweep is reusable for latency
and error rate). **Partially met for the capability half**: the suite exists, runs, and
records profiles for the in-process store and for `object_store::InMemory` — but no
emulator or real cloud has been probed, so the matrix has two rows and both are local.
Completing it is M0b's first task, and it is one command.

## Carried forward

| ID | Task |
|---|---|
| ~~M0a.10~~ | ~~Latency injection in `Faults`~~ — **DONE.** Criterion 4 above is now met as written; the delay is a real `tokio::time::sleep`, free under `start_paused` because the runtime auto-advances while idle |
| M0a.11 | Mutation score 76.8% vs the 80% target. The 46 survivors are mostly (a) conformance probe *guards* — detecting a backend that returns the wrong bytes needs a deliberately-corrupting store, the same pattern as `TagStyle::ContentHash`; and (b) FNV mixing in `next_tag`, where `^=`→`&=` preserves the property under test and is arguably equivalent |
| M0a.12 | `delete_batch` via `DeleteObjects` rather than one request per key |
| M0a.13 | Run the conformance suite against MinIO, Azurite and fake-gcs-server from the compose stack |
