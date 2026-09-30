# M20 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container (4 cores, a virtual disk, `cargo-mutants`
27.1.0). The one number below is `provisional`: a local directory stands in for the NVMe
device D-23 means, which is enough for every correctness property and no absolute latency.

Tests are in `cargo test -p pstore-cache --test disk`, `cargo test -p pstore-engine --test
scan_cache`, `cargo test -p pstore-blob --test scanning` and `cargo test -p pstore-server --test
read_cache`.

**Observed red.**
- Seven cache tests failed against a stub tier that opened and stored nothing.
- Three engine tests failed with every scan admitted.
- Five of eight server tests failed against a stub that ignored the cache and the variables.
- A test that passes on a stub guards a mutation instead; each named below was run by hand.

1. **A restart is not a flush.** `a_restarted_server_is_not_a_flush` checks identical answers,
   at HEAD plus one read per segment without a centroid table (see the amendment in the spec,
   and BACKLOG row 46). Beneath it, `a_reopened_tier_serves_without_a_request` checks ranges,
   `get_ranges`, suffixes and whole objects at 0 requests. These fail with the disk not
   consulted, not admitted, recovery off, `close` not flushing, or the tier dropped.
2. **Warm equals cold.** `cached_answers_equal_uncached_ones`: a vector query, a `rank_by`
   and a filtered query, cold, warm and after a reopen, against an uncached server.
3. **Another store's directory serves nothing.** `another_stores_directory_serves_nothing`.
   - `a_tier_emptied_for_another_store_stays_empty` came from code review round 1. It fails
     with the files kept and recovery merely off.
   - `a_missing_identity_empties_the_tier`.
   - `a_tier_of_another_format_is_emptied`, added because dropping the key's tag aborts the
     process on a misread length. The entry format is now part of the identity.
   - These fail with the identity not compared, a missing identity read as a match, the files
     not deleted, and the format left out.
4. **Quotas hold on disk.** `a_bulk_burst_leaves_meta_on_disk` fails with one disk instance
   shared, and with meta's and bulk's shares swapped. Pinned and meta are the same size, so
   swapping those two cannot be observed.
5. **Corruption is a miss.** `a_flipped_byte_is_a_miss` finds the value in the tier's files,
   flips one byte, and reads the store's bytes at one request. It relies on `foyer`, and pins
   that behaviour across upgrades.
6. **A broken disk bypasses.** `a_directory_that_is_a_file_bypasses`,
   `truncated_tier_files_read_correctly` and `a_server_over_a_bypassed_cache_serves`.
7. **A hit is not billed.** `a_hit_is_not_billed` uses a fresh cache per query and checks the
   warm cost of criterion 1. It fails with `with_cache` dropping the core, and with the engine
   ignoring it.
8. **`get` is never cached; entries are distinct.** `get_is_never_cached_on_disk`,
   `a_range_a_suffix_and_a_whole_are_three_entries`,
   `an_empty_range_and_a_whole_object_do_not_collide`, and
   `an_entry_larger_than_a_block_stays_in_memory`. `foyer` refuses the entry itself, so an
     explicit size check here was equivalent, and was removed.
9. **Scans do not admit, and do hit.**
   - `a_cold_compaction_admits_no_bulk`, `a_cold_scan_admits_no_bulk`,
     `a_folds_delete_pass_admits_no_bulk`, `a_folds_patch_pass_admits_no_bulk`,
     `a_compaction_over_deleted_rows_admits_no_bulk` and `a_query_admits`. Each of the five
     call sites left unadapted fails one of them.
   - `bulk_and_unclassed_reads_arrive_as_scans_and_others_unchanged` fails with the bulk arm
     of the re-classing removed.
   - `a_scan_is_served_from_bulk_and_admits_nothing` and
     `a_scan_is_served_from_the_memory_tiers_bulk` fail with `Scan` admitted, not probing
     bulk (in memory, and on disk), or promoted from disk.
   - Criterion 9's second half was amended: see the spec. The engine tests build a
     memory-only cache, so the disk half of "admits nothing" is checked at the cache level
     only (code review, minor).
10. **Reopening is bounded.** `a_full_tier_reopens_within_a_second`, run by hand with
    `--ignored`: a full 256 MiB tier reopened in **18 ms**. `provisional`. Not a gate.
11. **One directory per lane.** `lanes_do_not_share_a_tier` fails with the lane dropped from
    the path.
12. **Configured by name.** `the_cache_is_configured_by_name`,
    `a_bad_cache_setting_is_refused_by_name`, `the_store_id_is_created_once_and_read_back`,
    `a_node_that_loses_the_id_race_takes_the_winners`, `open_cache_opens_what_the_config_names`.
    - Eleven hand mutations of the parse, the defaults, each refusal, the id and `open_cache`
      each fail one of these.
    - The id is read through tenant 0's view, so its one GET per node counts against tenant 0
      (code review, minor).
13. **Uncached is unchanged.** Every existing test binary of `cargo test -p pstore-server`
    passes under `Api::new`. `the_uncached_api_repeats_its_reads` and
    `without_a_core_each_call_is_forwarded_as_itself` fail with a core-less `Caching` going
    through its `_as` forms.
14. **Sealing a key twice writes the same bytes.**
    `sealing_one_key_twice_writes_the_same_bytes` (`cargo test -p pstore-engine --test
    abandon`): the twin committed the key ours sealed, and every byte, sidecars included, is
    the same. It passed on the tree as it was, so M20 went ahead.
15. **Gates.**
    - `./scripts/mutants.sh --check . --in-diff` over the source diff `56e454c..7adaf8f`, in
      a worktree at `7adaf8f`, in four shards: 127 mutants, 72 caught, 34 unviable,
      6 timeouts, 15 missed.
      - Seven are killed by tests added after the sweep, each run by hand: `Scanning`'s four
        pass-through forms (`everything_but_a_classed_read_is_forwarded_unchanged`),
        `CacheCore`'s `Debug` (`a_core_says_what_its_disk_is_doing`), and the memory split
        (`the_memory_budget_is_split_a_tenth_a_tenth_and_the_rest`). That last test also
        fails outright with the two mutants that timed out in `shares`.
      - The emptying's `NotFound` guard as `true` could not be provoked under root. The guard
        is gone (`2f69c10`): only directories that exist are deleted, and any error bypasses.
      - Seven are **equivalent**. `sync_dir` as a no-op makes the emptying durable across a
        crash, which no test can observe short of one. Six mutants of `estimated_size`:
        `foyer` uses it only to choose among disk engines, and there is one.
    - The source changed after that sweep (`7adaf8f..2f69c10`, `disk.rs` and `main.rs`) was
      swept too: 1 mutant, unviable.
    - `./scripts/gates.sh` passed, all 17 gates, at `2f69c10`.

Spec review: two rounds (3 blockers, 6 majors; then approved). Code review: two (one major,
criterion 3's test; then passed). `main.rs`'s SIGTERM handling and `close` are untested wiring.
