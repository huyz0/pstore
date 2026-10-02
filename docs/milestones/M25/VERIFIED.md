# M25 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0). Every
count is a request count or an exact byte string, exact on any machine.

Commands: `cargo test -p pstore-engine --test quarantine`, `cargo test -p pstore-engine --lib`
(the HEAD tests), and `cargo test -p pstore-server --test quarantine`.

**Observed red.**
- All 9 engine tests failed on stubs that answered `None` and wrote nothing.
- The 3 server tests failed with the routes removed and `quarantined_rows` forced to 0.
- Each check that code review added fails on the mutation named with it, applied by hand.

1. **A rejected row is kept.** `a_rejected_row_is_quarantined_intact`: every rejected row is
   exported with its vector, attributes and a reason, and `schema_rejects` counts as before.
   `a_euclidean_row_is_exported_in_client_space`: the vector comes back exactly as written, and
   `$metric` is in `reserved`. The cosine export is untested, as the spec allows.
2. **It is never buried while named.** `a_quarantine_is_never_buried_while_named`, through later
   folds, a compaction and a branch. `gc_spares_a_named_quarantine`: a key buried by hand
   survives `gc(0)`.
3. **It survives GC.** `a_quarantine_survives_gc`.
4. **Discard is exact.** `discard_buries_exactly_what_was_exported`:
   - a later fold's object is kept;
   - burial happens at the discard's epoch, kept by `gc(1)` and reaped by `gc(0)`;
   - an empty discard commits nothing.

   From the sweep, a discard that loses its CAS:
   - `a_contended_discard_retries_and_lands`: one `Contended` answer is retried;
   - `a_discard_that_keeps_contending_reports_contention_after_every_attempt`: exactly 24
     refusals, then `Contended`.
5. **A drop buries it.** `dropping_an_index_buries_its_quarantine`, at the drop's epoch.
6. **Existence.** `an_index_known_only_by_its_rejects_is_found`: the export, `index_stats` (0
   segments, 0 documents, 2 quarantined) and `indexes()`. Killed: `stats_of`'s new arm removed
   (code review round 1, M1). From the sweep:
   - `an_index_without_rejects_has_an_empty_quarantine`;
   - `a_fully_discarded_rejects_only_index_still_exists_and_drops`.

   Server: `quarantine_of_an_unknown_index_is_404`, `a_discard_without_a_through_is_refused`.
7. **Cost.** `a_rejecting_fold_costs_one_write_per_index`: (19 reads, 3 writes), against the
   same fold on the parent commit, measured at (19, 2). The fold without rejects is pinned
   unchanged by `an_uncontended_fold_compaction_and_branch_are_unchanged` (M23).
8. **HEAD.**
   - `an_empty_quarantine_changes_no_byte` asserts the exact bytes the parent commit's encoder
     wrote for two HEADs.
   - `a_head_without_a_quarantine_decodes` and `a_quarantine_alone_round_trips` cover the
     absent section, and a quarantine with no other optional section.
9. **Gates.**
   - `./scripts/gates.sh`: 17 of 17 by the pre-commit hook on `b327413`, and on this ledger's
     commit.
   - The sweep over M25's source diff (`c29c25e..b327413`) misses **0** once its ten misses were
     closed by the four tests above. Every miss was applied again by hand and fails one.
     - pstore-engine: 49 mutants, 34 caught, 5 unviable, 10 missed.
     - pstore-server: 14 mutants, 1 caught, 13 unviable.
   - ⚠️ **Not equivalent, though code review called them so.** Three of the misses are
     the `||` in the quarantine's existence check and in the drop's. They do differ: once a
     quarantine is fully discarded, the index's reject count remains without one.
   - ⚠️ Not `./scripts/mutants.sh` itself: `cargo mutants --in-diff` was run directly, with the
     engine shard limited to the six test files covering the change.

**Existing tests.** `schema.rs` `a_contradicting_index_seals_nothing` asserted one write at most.
It now asserts exactly two, the HEAD commit and the quarantine, and that no segment appeared.
⚠️ Drift: the spec's list of affected tests missed it.

**Server.** `quarantine_is_exported_counted_and_discarded` covers the export form, `$fts` in
`reserved`, `quarantined_rows`, a discard `through` the export's epoch, and an empty discard.

Spec review took two rounds: round 1 blocked on six majors, and round 2 passed. Code review took
two: round 1 blocked (existence untested), and round 2 passed. Its minors are in the M25.1-2
commit.
