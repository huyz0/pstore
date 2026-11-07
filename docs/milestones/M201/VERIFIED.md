# M201 verification

Implemented in:
- `crates/pstore-engine/src/lib.rs`: `gc_inner` in three steps (announce `record_reap(horizon)`,
  delete, prune), and `reap_plan`, which plans a pass through `max(horizon, reaped_before)`
  and lists the exact graveyard pairs the prune removes.
- `crates/pstore-engine/src/replica.rs`: M196's comment on the publish-time check, updated.

The docs are in `docs/api-migrations.md` (M201) and the DELIVERY row for H. All runs were in
a 4-core cloud container, so every number is `provisional`.

How to run:
- `cargo test -p pstore-engine --test it gc_announce::`
- `cargo test -p pstore-engine --test it replicate::a_copy_inside_a_gc_pass_never_publishes`
- `cargo test -p pstore-engine --test it reap::`

**Red first.** The four new tests were run against the parent's `gc_inner` and all four failed
(.harness/m201-red.log): HEAD's `reaped_before` was still 0 at GC's first delete, a refused
delete left no announcement, the rival's burial was deleted by the parent's retried pass (the
pass's count was 5, not 4), and the same-tenant copy run inside a GC pass published. That test loops over its
three source kinds and stops at the first failure, so only the same-tenant kind was seen red; the
other two run the same parent code path, with nothing announced at the first delete.

**Suite:** 2,738 passed and 0 failed, with the two IPv6 tests skipped because the container has
no IPv6 (.harness/m201-suite.log). Those two, run alone, are the only tests the skips leave
out, and both fail with `Address family not supported by protocol` (.harness/m201-ipv6.log):
the review packet's `cargo test` FAIL is that environment, as in M200. fmt and clippy pass.

**Coverage:** `scripts/coverage.sh` over the shipping crates gave lines 97.32%, regions 96.00%
and functions 97.04% (.harness/m201-coverage.log).

**Mutations.** I applied 9 mutations one at a time by script and reverted each one
(.harness/m201-mutations.log). One survived the first round: the announcement's commit not
recorded as this engine's epoch. AC 3's test now checks the engine's epoch after the refused
delete, and kills it. All 9 are killed: the announcement dropped, made on an equal horizon, or
not recorded; the floor not raised to `reaped_before`; the prune by range, not retried, not
removing its pairs, or keeping an empty epoch; and the prune not recorded.

**Acceptance criteria.**
1. PASS: `gc_announces_its_horizon_before_it_deletes`. A second engine reading HEAD at GC's first
   delete sees `reaped_before` equal to the pass's horizon. After the pass the graveyard is
   empty, every buried object is gone, and `gc` returns the count of distinct buried keys.
2. PASS: `a_copy_inside_a_gc_pass_never_publishes`, for a same-tenant, a cross-tenant and a
   remote source. The copy's whole sync runs at GC's first delete and ends `failed` with
   `epoch_unavailable`, the sync commits nothing, and `dest`'s HEAD names no segment.
3. PASS: `an_announced_reap_is_finished_by_the_next_pass`. A refused first delete is the pass's
   error and leaves `reaped_before` raised, the graveyard as it was and every object in place.
   A second pass, with a horizon below the announcement and in a second case equal to it,
   deletes them all, prunes the graveyard, returns their count and issues exactly one
   conditional PUT. A third pass returns 0 and writes nothing.
4. PASS: `a_rivals_burial_survives_the_prune`. A rival commit at GC's first delete files a new
   object at epoch 1. The pass returns its own count with none of its own pairs left, the
   rival's object is never present and unlisted, and one more pass deletes and unlists it.
5. PASS: `a_scheduled_reap_costs_what_gc_does` now expects `(1, 1, 2)`, the owner's OK given in
   the project thread on 2026-11-07. `gc_with_nothing_due_is_free` passes unchanged.
6. PASS for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`, run here (see above).
   `scripts/mutants.sh` is pending CI's shards.

The choice that GC does not wait for a copy (no hold), and ends it `epoch_unavailable`
instead, is the owner's: Huy picked "Close the gap, keep typed error" on a decision card in
the project thread on 2026-11-07, and approved the cost test's change the same day.

Code review (packet `.harness/review/m201.r1.packet`; round 2 PASS) found no correctness defect. Its two
majors were about evidence, and both are taken: the IPv6-only run above, and the owner's
choice recorded here and in the spec. Minors: the red claim's scope is stated above, and the
docs now say the prune also removes the pairs GC declined. Left as it is: a lost prune
retried after a concurrent pass already pruned every pair still commits once, one bounded CAS
on contention only.
