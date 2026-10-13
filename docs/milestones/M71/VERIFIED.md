# M71 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. Overlap is on a seeded
synthetic corpus and **provisional**; no latency is claimed. This milestone is the library
layer: the engine, compaction and GC do not yet write or use the sidecar (M71.2).

Command: `cargo test -p pstore-index --test fde_clusters`: 4 tests.

⚠️ **A finding that changed the design.** The first version had the sidecar name only the rows
of each list and read them out of the segment's `name$fde` field by computed range. It moved
3,067,904 of 3,072,000 bytes at 4 of 12 lists: the blob layer coalesces ranges closer than its
`coalesce_gap` (64 KB here, 1 MB against S3), and rows scattered at 2 KB across a few MB are
always within it. The sidecar now holds the encoded rows themselves in list order (`.fdev`), at
the price of duplicating the field's bytes.

1. `sidecar_round_trips_and_refuses_damage`: decode is encode's inverse; an empty, truncated,
   trailing-byte, wrong-magic, row-past-the-end, absurd-list-count and absurd-dimension blob are
   each refused.
2. `build_is_deterministic_and_covers_every_row`: two builds are byte-identical in both objects;
   every one of 400 rows sits in a list; the key is `<segment>.fdec`, the vectors' is
   `.fdev`; the first vectors entry is the first list's first row.
3. `clustered_search_finds_what_the_flat_scan_finds`: 1,500 documents of 8-24 vectors, 12 lists,
   20 queries; the top 10 at 4 probes shares **0.86** of the all-lists top 10 (floor 0.8), and a
   sidecar built from another corpus shares **0**. A vectors blob of the wrong size and an absent
   sidecar are errors, not answers.
4. `clustered_search_reads_a_fraction`: bytes of `.fdev` at 4 probes **1,011,712** against
   **3,667,968** for every list (3.6 times fewer; the 3.67 MB is larger than the segment's field
   because the clustering replicates boundary rows).
5. **Gates:** `./scripts/gates.sh` green by the pre-commit hook on this commit. **Mutation**
   (`cargo mutants`, `--no-config`, `fde_clusters.rs`): the first sweep, 57 mutants, 3 missed, all
   one sanity guard before a pre-allocation; the guard was removed (a damaged count is refused by
   the reads, and nothing pre-allocates from it). Final sweep: **50 mutants, 43 caught, 7 unviable,
   0 missed.**

**Not covered:** the engine writing the sidecar at seal and compaction, GC, the coordinator
choosing it, a size below which a segment stays flat (M71.2); the exact rerank over the
clustered candidates; a real-store coalescing measurement.
