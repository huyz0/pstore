# M6a — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" is a **mutation applied to the shipped code**, the named test run, and
the failure read — not a stub not yet written. Three fixtures survived their own mutation on
the first attempt and are recorded as such, which is the whole reason to do it.

1. **A bucket is derived, and hashing defeats structured ids** —
   `bucket_is_derived_and_hashing_defeats_structured_ids` (`cargo test -p pstore-catalog --test
   buckets`): 100,000 ids in three families at width 256, busiest bucket under 1.5× the mean.
   Red under `(tenant.0 as u64) % width` at **256.000×** on the high-bits family — while the
   *sequential* family passed the same mutation, which is why there are three.
   `every_bucket_is_inside_the_width` pins the range at five widths, 1 and 65,536 included.
2. **One read, one write, no LIST anywhere** — `an_observe_costs_one_read_and_one_write` (red
   at **2 reads against 1** when `get_with_tag` is split into `get_tag` + `get`) and
   `the_whole_lifecycle_issues_no_list` (red at 2 against 0 with one `list_unrestricted`).
3. **Depth 2 given the width, 3 from a cold root** — `enumeration_is_two_rounds_deep` on
   `DepthCounting`. Red at **9 against 2** with the pointer reads awaited in a loop rather than
   joined: 16 requests either way, and the answer identical.
4. **Requests are a function of width, not tenants** —
   `enumeration_requests_do_not_scale_with_tenants`: 32 reads at 100 tenants and at 2,000, width
   16, with `marks.len() == 16` asserted in both so the counts are not equal by luck. Red at
   **132 against 2032** with one extra read per record.
5. **Incremental reads only what moved** — `incremental_enumeration_skips_unchanged_buckets`:
   after a fold touching 1 of 16 buckets, exactly that one bucket's run is read. Red as all 16 with the
   `run_epoch`/digest comparison dropped.
6. **Pending is visible; folded appears once** — `a_pending_record_is_visible` (red as `[]`
   against `[7]` when only the run is read) and `a_folded_record_appears_once`, whose fixture
   puts the tenant in the run **and** back in pending: red at **2 against 1** when run and
   pending are concatenated rather than merged.
7. **An unchanged index set writes nothing** — `an_unchanged_index_set_writes_nothing`: 99
   further `observe` calls over an advancing epoch cost zero requests. Red with the epoch folded
   into `TenantRecord::identity` — the change that makes an append commit-rate.
8. **Racing appenders both land** — `racing_appenders_both_land` on `Gated`, `store.arm()` and
   `store.raced()` both asserted. ⚠️ **The first version was a lottery**: unarmed, the barrier
   never engages, a single-threaded runtime runs the appenders in sequence and nothing rebases.
   Armed, it is red when the retry reuses its original head instead of re-reading.
9. **Racing folders lose no record** — `racing_folders_lose_no_record`, whose fixture
   interposes an append between the two folders' head reads so they produce *different* runs at
   one `run_epoch`. ⚠️ **Neither half of the protection is individually killable**: the digest
   in the run key alone survives (the `NotExists` precondition still refuses the overwrite) and
   the precondition alone survives (the key differs). Red at `[1]` against `[1, 2]` only under
   **both** mutations. Both stay — the precondition makes immutability the store's rule rather
   than an argument, the digest makes a pointer name the exact bytes it committed.
10. **`pending` is bounded** — `pending_is_bounded_by_an_inline_fold` checks
    `pending.len() <= MAX_PENDING` after **every** one of 25 appends and that the inline folds
    lost nothing. Red with the cap checked after the push instead of before.
11. **Newest epoch wins wherever records meet; a tombstone hides without being dropped** —
    `the_newest_epoch_wins` (red under `>` for `>=`: `Live` against `Deleted`),
    `merge_keeps_tombstones`, `an_older_epoch_does_not_overwrite_a_newer_one` and
    `a_tombstone_hides_a_tenant_without_being_dropped` (wrong first: filtering tombstones at
    fold time survived it until a **stale writer arriving after** the fold was added; then red
    at `[7, 8]` against `[8]`). ⚠️ **Code review found the real hole, which none of those
    reach.** Two records for one tenant also meet in the *pointer*, and the appender applied a
    second precedence rule there — `retain` by tenant id — so a stale writer overwrote a newer
    pending record (`Epoch(2)` against `Epoch(9)`) and resurrected a pending tombstone (`[7]`
    against `[]`). Both observed red before the fix;
    `an_older_epoch_does_not_overwrite_a_newer_pending_one` and
    `a_stale_live_record_does_not_resurrect_a_pending_tombstone` now pin them, and `merge` is
    the single rule both paths call. ⚠️ **Mutation could not have found this**: the missing
    epoch comparison was absent code, not a wrong operator.
12. **Nothing depends on `pstore-catalog`** — `grep -l pstore-catalog crates/*/Cargo.toml` names
    only its own manifest, so no serving path can reach the catalog. ⚠️ Structural, not
    behavioural, and that is the honest form: M6a adds no edge a behavioural version could fail
    on. The RA row "open / write / query a tenant — unchanged" rests on this alone.
13. **A refused read is an error, not a shorter answer** —
    `a_refused_read_is_an_error_not_a_shorter_answer` and `a_refused_run_read_is_an_error` on
    `Flaky::refusing_reads_at`, `failures() == 1` asserted so the injection is known to have
    fired. Red when `read_head`'s `NotFound` arm widens to `Err(_)`.
14. **C-12's cheap side, measured** — `an_observe_moves_fewer_bytes_than_a_fold`, at the cap
    rather than on an empty pointer: under a quarter of a `fold`'s bytes over a 64-record
    bucket. Red at `MAX_PENDING = 128`. ⚠️ **The first fixture measured an empty pending list
    and survived that mutation**, reporting the trade as free at any cap.
15. **Gates** — `./scripts/gates.sh` green, `cargo deny check` green. `pstore-catalog`
    **95.30% regions** / 96.82% lines (`cargo llvm-cov --package pstore-catalog --lib --tests`);
    workspace **95.30% regions** (`./scripts/coverage.sh --fail-under-regions 95`). Per file:
    `keys.rs` 100%, `enumerate.rs` 97.94%, `record.rs` 96.20%, `bucket.rs` 93.96%, `append.rs`
    89.47% — the crate clears the floor and two files do not, stated rather than averaged away.

## Mutation

`scripts/mutants.sh --file crates/pstore-catalog/src/*.rs` **in the `dev` container**, where
`dev/README.md` says heavy work belongs: **99 mutants, 85 caught, 14 unviable, 0 missed, 0
timeouts.** Every viable mutant dies, and the rounds it took are the finding:

| Round | Result | What the survivors were, and what changed |
|---|---|---|
| 1 | 106 mutants, 82 caught, **10 missed** | Six inside `hash`'s murmur3 finalizer — a weakened mixer still passes an imbalance bound, so a *statistical* test cannot see it. Four in `decode_records`' `> MAX_RECORDS` guard. |
| 2 | 116 mutants, 93 caught, **7 missed** | The hash is now pinned by golden values (`the_hash_is_pinned_so_a_deployment_never_re_buckets`) and all six die. The remaining seven were the ceiling arithmetic again, in its new form. |
| 3 | 98 mutants, **84 caught, 0 missed** | The guard is **gone**, not tightened. |
| 4 | 99 mutants, **85 caught, 0 missed** | After the review fix in criterion 11 — which mutation could not have found, and did not. |

⚠️ **The ceiling is the more useful of the two.** `MAX_RECORDS = 1 << 20` stopped
`Vec::with_capacity(n)` turning a corrupt length into an OOM abort — a real hazard, guarded by
a number nobody could justify and no test could observe, since a *loose* bound is only wrong
for inputs the cursor refuses anyway. Deriving it from the buffer length made it exact and
still left five survivors one operator down. Growing on demand removes the hazard instead: no
attacker-controlled capacity, no constant, nothing to mutate. **The gate did not say the test
was weak; it said the code was.**

⚠️ **The golden hash values are not a snapshot to refresh.** Bucket assignment decides which
object a tenant's record lives in, so changing the mixer re-buckets every tenant in a live
deployment and enumeration reports whatever is left. If that test fails, restore the function
or ship a re-bucketing pass — never update the constants.

## What is not built, and named rather than omitted

- **Bucket splitting (OQ-8).** The root carries `width` and every reader takes it as a
  parameter — the mechanism a split needs; the split needs a protocol keeping an old-width
  reader stale rather than wrong, and `{bucket:04x}` stops at 65,536 buckets, so it is a
  key-format change too.
- **Run reaping.** A superseded run is garbage and stays;
  `a_refused_pointer_cas_leaves_an_orphan_run_and_no_loss` pins the half-done fold that makes
  one. Reaping needs a retention window, or a reader mid-enumeration loses the run it is on.
- **Quotas, metering, billing aggregation, and any wiring into the commit path** — M6b. Nothing
  calls `Appender::observe` yet, which is criterion 12 restated: this is a mechanism, and its
  caller is M6b's. The per-tenant counters `Accounted` already keeps are what metering builds on.
  ⚠️ **Corrected by [M6b](../M6b/SPEC.md): the caller is not M6b's.** M6b is a `BlobStore`
  decorator and wires nothing into any stack either — the caller is whoever composes a serving
  stack, and nothing does. Recorded here rather than left as two ledgers disagreeing.
- **The 1M-tenant measurement.** Criterion 4 measures the invariant at 2,000 tenants over 16
  buckets; what holds at 1M is arithmetic on it, and a cost that turned superlinear only above
  2,000 would not be caught here.
