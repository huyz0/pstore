# M28 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0), with
`MemoryStore` behind stores that count or hold requests. Criterion 5's time is a bound, and
its run time below is `provisional`.

Command: `cargo test -p pstore-cache`.

**Observed red** on the spec's commit (`ade1fa6`): 6 of the 8 new tests failed, as the
criteria below say. The other 2 guard behaviour the parent has, and were seen red by mutation.

1. **A scan's miss costs no bulk reader a request.** `a_scan_miss_costs_no_bulk_reader_a_request`:
   2 requests, against 9 on the parent. Killed: a scan that claims.
2. **A claim-free fetcher leaves another's gate.** `a_claim_free_fetch_leaves_the_claimants_gate`:
   2 requests.
   - On the parent its claimant never fetched (it waited on the scan's claim), so it failed
     at that wait, not at the count (code review).
   - Killed: the guard's close dropped.
3. **A failed or cancelled claim releases its waiters and its gate.**
   - `a_failed_claim_releases_its_waiters_and_its_gate` and
     `a_cancelled_claim_releases_its_waiters`, each ending with a later pair of reads at 1
     request.
   - On the parent the cancelled claim's waiter hung.
   - Killed: the guard's removal dropped (also by code review) and its close dropped.
4. **Recency is unchanged.** `eviction_follows_a_model_lru` and `eviction_is_least_recently_used`.
   - Killed: eviction from the newest end.
   - Also killed: a touch that keeps its entry's tick. That one first survived, and the
     model's sequence was extended to touch one entry twice before evictions pass it.
5. **Recency is O(log entries).** `recency_is_logarithmic`: 0.75 s here (`provisional`). On the
   parent it failed at its 20 s bound.
6. **The disk hash is pinned.** `the_disk_hash_is_the_encodings`.
   - Its three literals came from a Python `XxHash64` written from the published algorithm.
     It was checked against the published vectors for `""` and `"a"`, over the 40-byte
     encodings written out by hand.
   - The code review checked them again with its own implementation.
   - On the parent the derived `Hash` gave other values. Killed: a field left out.
7. **The format moved.** `a_tier_of_the_previous_format_is_emptied`: served from disk on the
   parent, fetched again here. The existing restart tests, such as
   `a_reopened_tier_serves_without_a_request`, pass.
8. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `acad494`, and on this ledger's
     commit.
   - The sweep over M28's source diff (`ade1fa6..acad494`), against the whole crate: 34
     mutants, 31 caught, 2 unviable, 1 timeout, **0 missed**.
   - The timeout is the guard's `Drop` emptied. Waiters then hang, and an older unbounded
     test hangs with them. The bounded tests of criteria 1–3 kill the same defect outright,
     with only the close dropped.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly.

Spec review took two rounds, each approving with majors folded into the text:
- a cancelled claimant left its gate forever, so the claim became a guard;
- the guard's `Drop` cannot await, so the gates moved to a `std` mutex.

Code review took one round, which passed with one minor, recorded in `acad494`'s message. The
review counter also records a packet no agent saw, built on a clippy failure.
