# M71 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory store. Overlap is on a seeded
synthetic corpus and **provisional**; no latency is claimed. Criteria 1-4 are the library layer
(M71.1); 5-6 are the engine and query path (M71.2).

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
7. **Gates (M71.1):** `./scripts/gates.sh` green by the pre-commit hook on that commit. **Mutation**
   (`cargo mutants`, `--no-config`, `fde_clusters.rs`): the first sweep, 57 mutants, 3 missed, all
   one sanity guard before a pre-allocation; the guard was removed (a damaged count is refused by
   the reads, and nothing pre-allocates from it). Final sweep: **50 mutants, 43 caught, 7 unviable,
   0 missed.**



## M71.2 — the engine and query path

Commands: `cargo test -p pstore-engine --test multi --test fde_skip --lib head`.

5. `a_big_segment_clusters_its_encoding_and_a_query_uses_it`: 9,000 two-vector documents fold into
   one segment; HEAD's ref has `fde` and `.fdec` names `late` with more than one list; an `fde`
   query with exact rerank has a top-10 mean score **at least 95% of the exact leg's**
   (ids are not compared: ~750 documents a topic are near-ties); a `.fdev` of the wrong size makes
   the query an error, not a silent flat scan; after more writes and a compaction the merged
   segment has `fde` and its own `.fdec`, and `gc(0)` reaps the buried segment's.
   `the_clusters_are_asked_for_only_by_an_fde_leg_of_a_flagged_segment` (`fde_skip.rs`, counting
   store): an `fde` leg over the flagged segment reads `.fdec` **once** and `.fdev` at least once;
   a dense query reads **neither**; the same `fde` leg over a 40-row segment HEAD does not flag
   reads **neither** (no known-404 read).
6. `the_dicts_section_round_trips_and_refuses_malformed_input`: bit 2 round-trips alone and with
   sparse and text (flags 4-7, re-encoding byte-identical); 8 is refused. The two existing
   assertions of the recorded flags (the dictionary-skip and replication suites) now include `fde`.
7. **Gates:** green by the pre-commit hook on this commit. **Mutation** (`cargo mutants`,
   `--no-config`, over the diff of `pstore-engine/src` and `pstore-query/src`): first sweep 56
   mutants, 41 caught, 7 unviable, **8 missed**. None is on a line M71.2 wrote:
   - `run.rs:1547` (the new `wants_fde && fde_clusters` gate) was missed by the first sweep's tests
     and is **caught** by the counting-store test above (re-run: caught), which was written for it.
   - The fresh-view builder's `Ok(Some(None))`: caught with the engine's whole suite (20 mutants there, 13
     caught, 6 unviable, 1 missed). The one missed, `f.trigram == trigram` (`lib.rs:5028`), is
     recorded as equivalent in M15's and M16's ledgers and is not this milestone's.
   - The flat scan's `a * b` (`run.rs:1853`, re-indented, logic unchanged): caught by
     `pstore-query`'s own tests.
   - The held-part byte estimate and the sparse and text gates (`run.rs:1555`, `1559`) are M55's and M32's;
     the diff pulled them in by touching adjacent lines. ⚠️ **Not re-run under the project's
     whole-workspace test config** (a run takes about 12 minutes a mutant); M32's ledger records
     the dictionary-skip suite as what kills the gates.

**Not covered:** more than one multi-vector field per segment clustered (a second stays a flat
scan); a configurable probe count (a third of the lists, at least two, provisional); the
in-memory fresh segment; the server's response and docs surface; a real-store coalescing
measurement. ⚠️ HEAD's flag byte now uses bit 2: an older build reads a flagged segment's HEAD as
corrupt.
