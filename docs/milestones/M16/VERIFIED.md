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
9. **Gates.** `./scripts/mutants.sh --check . --in-diff` over M15's and M16's source
   together (`5c7f4f1..3196cb4`), swept in a worktree at `3196cb4` in four shards. Container
   restarts and a background time limit cut the fourth, so it ran as pieces 12 to 15 of 16:
   cargo-mutants shards in contiguous slices, so those pieces are exactly the fourth shard.
   - 381 mutants: 302 caught, 43 unviable, 2 timeouts, 34 missed. The two timeouts are
     `glob_class`'s `+=` as `*=`, which hang and so are caught.
   - Every miss is accounted for in its shard's commit (`889923e`, `1e36dc1`, `3725e0f`,
     `3dc12c8`, `11db3c9`):
     - 28 are killed by tests added for them and seen failing, for example
       `a_regex_requires_its_literal_runs_and_nothing_optional`,
       `a_trigram_sets_the_bits_fnv_1a_names` and
       `the_widest_sketch_that_fits_is_chosen_and_an_exact_fit_fits`. Three of those 28 were
       not re-run one by one: `encoded_len`'s five mutants are one formula, and two were
       re-run against `a_sketch_is_as_long_as_its_encoded_len_says`.
     - 4 were in code nothing called, `Sketch::attrs` and `Engine::write_if_with`, and that
       code is removed.
     - 2 are equivalent. One is a guard that `unscoped` already implies, and it is removed.
       The other is `fresh_view`'s trigram comparison, which decides only whether an
       in-memory view is reused; a sketch never changes an answer.
   - `./scripts/gates.sh` on `11db3c9`: all seventeen PASS.

Spec review took two rounds. Code review took two rounds: round 1 blocked on two majors, the
owner read to the first `/seg/` and read paths left untested on a diverged branch; round 2
passed. Two minors are recorded in the round-1 fix commit and not changed.
