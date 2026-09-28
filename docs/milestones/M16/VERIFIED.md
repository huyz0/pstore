# M16 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, `cargo-mutants` 27.1.0).

The engine tests are in `cargo test -p pstore-engine --test branch`, and the API's in
`cargo test -p pstore-server --test branch`.

⚠️ **Observed red means by hand mutation.** Each mutation below was made by replacing the line and
running the named tests, and it failed them. This ledger does not claim that the engine tests were
seen failing before the engine existed. The server tests were seen failing, both of them, with the
wire's hook removed.

1. **Equal at birth.**
   - `a_branch_equals_its_source_and_then_diverges`: 297 rows, three of them deleted before
     the branch.
   - `a_branch_holds_every_acknowledged_durable_write`: over the API, with a durable delete
     still unfolded when the branch is asked for. It checks the schema, and both field names.
     It failed with the fold before the branch removed, and with the token's `saw` removed:
     the token's epoch must equal the answered one.
   - `a_branch_and_its_source_diverge_on_every_read_path`: dest at its birth epoch, by `as_of`.
2. **Independent after.** `a_branch_and_its_source_diverge_on_every_read_path` covers both
   directions:
   - a delete, a patch and a `delete_by_filter`, in shared segments;
   - every path: ordered, vector, BM25, `as_of`, a count aggregate, the stats count, and
     `updated_epoch`;
   - the two indexes lose different numbers of rows, so a count read through the other
     index's vectors differs.

   `a_branch_equals_its_source_and_then_diverges` adds `scan`, each index compacting under
   its own vectors, and `dest` compacted, dropped and `gc(0)` with `src` unchanged.
   **Each of the twelve `head::dv_ref` sites** in `pstore-engine/src/lib.rs`, with the plain
   segment key put back, failed at least one of these tests. That was twelve of twelve, after
   code review round 1 found six the first tests missed.
3. **GC-safe.**
   - `gc_never_reaps_what_a_branch_names`: `src` compacted, and `src` dropped. It failed with
     the drop's removal of `dest`'s vector reading the plain key.
   - `a_doubly_buried_segment_waits_for_its_last_burial`: the horizon inside [E2, E4). It
     failed with `later`, the check that a key waits for its newest burial, removed, and with
     a marker not mapped to its segment.
4. **Deep** — `branches_nest_and_a_dropped_name_restores_from_its_branch`: three levels, then a
   restore.
   - It failed when copying from `src`'s plain key in place of `dv_ref(src)`.
   - It failed when copying to `dv_ref(src)` in place of `dv_ref(dest)`.
5. **History** — `a_branchs_past_is_its_own_and_begins_at_the_branch`: before and after
   compaction and a drop, and `dest` absent below `branched`. It failed in each of these cases:
   - a marker resurrected under its owner in place of its own index;
   - the liveness filter reading plain keys;
   - `branched` unread.

   ⚠️ **One hand mutation survived:** removing `as_of`'s dedup of a resurrected segment. No
   reachable sequence lists one segment twice in one index today. A branch to a dropped name
   needs GC past the drop first, which lifts the horizon above the first burial, and each
   other double burial is by two indexes. The dedup stays, as the spec's rule.
6. **Cost** — `a_branch_costs_a_read_and_a_cas_and_two_requests_per_vector`: 1 HEAD read, `d`
   GETs, `d` PUTs and 1 CAS, exactly. `a_branch_that_loses_its_cas_buries_its_copies` failed
   with the lost attempt's copies dropped rather than buried.
7. **HEAD** — `a_branched_set_round_trips_and_is_pruned_by_the_reap` (`cargo test -p
   pstore-engine --test head`): a non-empty `branched` beside default full text and no trigram
   set.
   - Every cut inside the new bytes is refused, except the two zero counts, which read as no
     branches.
   - It failed with M15's count omitted when a branch follows, with the section unread, and
     with the reap's prune removed.
8. **Refusals** — `what_a_branch_cannot_mean_is_refused`: each is `400` naming its rule. It
   failed with the request's other fields not counted.
   `an_index_whose_name_holds_seg_owns_its_segments` came from code review M1: an index named
   `a/seg/b`, which only a branch's names forbid, owns its segments. It failed on the first
   `/seg/`.
9. **Gates** — NOT-RUN yet: M16's sweep follows M14's and M15's.

Spec review took two rounds. Code review took two rounds: round 1 blocked on two majors, the
owner read to the first `/seg/` and read paths left untested on a diverged branch; round 2
passed. Two minors are recorded in the round-1 fix commit and not changed.
