# M60 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container, in-memory stores.

⚠️ **Found by a probe, before the spec.** M61's spec review read that `live_rows` decodes a
deleted segment's blocks alone. A throwaway test then wrote three folds, deleted three rows,
folded and compacted. The same exact dense query, which answered before, failed after with
`Query("no such vector field")`. The compacted segment held ids and attributes and no vector.
Every index that compacted a segment with deletes lost those vectors this way.

1. **Compaction keeps every vector.** `compaction_keeps_every_vector_of_a_segment_with_deletes`:
   documents with `vector` and a second named field (`words`), three folds, deletes folded
   into delete vectors.
   - Before the compaction, a filtered `Engine::scan` returns exactly the live rows with
     `n > 30`, each with both fields as written.
   - After it, the exact dense query answers with the same ids and distances, and
     `Engine::scan` returns all 57 live documents with both fields as written.
   - **Seen red on the code before M60**, with `live_rows` restored: `Query("no such vector
     field")` after the compaction.
   - Killed by hand (code review): the filter dropped from `live_rows`. The filtered scan
     then returns rows it excludes.
   - ⚠️ The test names its second field `words`, which sorts after `vector`. A field sorting
     before `vector` would take the legacy fixed-width section, which is M61's writer fix
     and not this one.
2. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `pstore-engine/src/lib.rs`'s changed lines against the engine's
     compaction, upsert, engine_query and scan_cache test files (`--no-config --profile
     mutants`): **2 mutants, 0 missed**, 1 caught, 1 unviable.

**Not covered**, as the spec states:
- a filtered `Engine::scan` over a segment with deletes reads every block, where the zone
  maps pruned some: bytes, never requests;
- a segment of several fields costs a round a field on these paths, as `Segment::scan`
  already did;
- no sparse field is in the test (code review), though the fix reads every field through
  `scan`, sparse ones included.
