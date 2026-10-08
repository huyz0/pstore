# M61 — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ **Where this ran:** a Linux x86-64 cloud container with 4 cores. No number here is a
latency; depth and request counts are exact.

Commands:
- `cargo test -p pstore-format --test fields`
- `cargo test -p pstore-query --test multi`
- `cargo test -p pstore-engine --test multi`
- `cargo test -p pstore-server --test multi`
- `cargo test -p pstore-server --test peers a_multi_query_is_not_split`
- `cargo test -p pstore-blob -p pstore-format -p pstore-query -p pstore-engine -p pstore-server -p pstore-index`: every test passes.

1. **MaxSim is MaxSim.** `multi_scores_by_maxsim`: two segments, rows of 0 to 4 vectors. The
   leg's ranking and raw scores (read through `max` fusion at weight 1) equal the test's own
   MaxSim bit for bit, rows without vectors are absent, and a limit of 5 cuts it.
   - Seen red first with `Prefetch::Multi` absent (compile), then with `max` as a sum.
2. **The format holds every field.**
   - `every_field_reads_its_own_vectors_and_vector_is_field_zero`: four dense fields, `late`
     among them; each reads back whole, `vector` is field 0, every span is distinct. Seen red
     on the writer before M61.
   - `the_default_field_is_field_zero`: a dense leg over a segment holding `late` answers as
     over one without it.
   - `a_segment_of_one_or_two_fields_is_written_as_before` (mutation sweep): a segment of
     `vector`, and of `vector` with a field named after it, fingerprints as the M60 writer's.
   - `a_field_of_mixed_widths_is_refused` (added in implementation): `try_finish` refuses a
     field of two widths within a row, across rows, and for `vector`. Seen red: the writer
     padded.
3. **Errors in the query layer.** `multi_errors_are_loud`: an unknown field is `UnknownField`,
   a short query vector is `DimensionMismatch` (even after one of the right width), and a
   dense leg over `late` is `Unimplemented`. Each seen red by hand: the `run` field check
   removed, the width check reading only the first vector, the dense guard removed.
   - `a_multi_leg_is_widened_past_deleted_rows` (mutation sweep): with its best rows deleted,
     a leg still answers its limit in live rows.
   - ⚠️ A survivor, equivalent: the leg's own cut, `scored.truncate(limit)`, removed. Fusion
     cuts to `top_k` anyway, so only work is saved.
4. **Through the API.** `multi_vector_documents_are_searched_by_maxsim`: under
   `cosine_distance` and `euclidean_squared`, a `multi` query's ids equal the test's own `f64`
   model (cosine MaxSim; Chamfer ascending):
   - unfolded, folded, after deletes and upserts that replace, add and remove `late`
     (unfolded and folded), and after a compaction;
   - with a filter;
   - fused with a dense and a text leg, against RRF over the legs' own rankings (equal fused
     scores compared as sets);
   - no `$dist` on a `multi` hit.
   The fixture asserts its model scores differ by more than `1e-4` relative; it caught two
   upserts that had copied another row's vectors. Seen red with the query vectors left
   untransformed.
   - ⚠️ Found writing it: the fixture's batched writes were never flushed, so the first
     "folded" checks read the memtable. Writes are `durable` now, and the compaction's
     success proves at least two segments.
   - Engine, `a_patch_beside_a_new_width_never_stops_the_fold` (code review, below).
   - Engine, `two_writers_widths_meet_in_the_fold` (added in implementation): two processes
     write `late` at widths 3 and 2, each past its own door; the fold quarantines the second
     lane's row, intact, and the first lane's rows are searched. Seen red with the fold's
     width partition removed: the fold then failed on the writer's refusal, which would have
     stopped every later fold of the tenant.
5. **Refusals.** `multi_vectors_are_refused_at_the_door`: every `400` of rules 2, 3, 5, 6 and
   7, and the limits themselves accepted (1,024 vectors, 8 fields, a 64-byte name, 16 legs).
   An unknown `multi` field is `400 unknown_field`, not retryable.
   - ⚠️ Found writing it: it was `500 internal`, retryable. `EngineError::UnknownField` is new.
   - `a_named_fields_width_is_one_an_index`: `schema_conflict` across two batches without a
     schema and with one, within a batch and within a document; once every row is folded a
     new width is accepted, and the query meeting both is `400 schema_conflict`.
6. **Depth.** `a_multi_query_keeps_the_depth`: dense, `multi` and hybrid queries on cold
   servers are four rounds deep, at one segment and at four, over a field wider than the open
   round's suffix read.
   - ⚠️ Seen red only at one segment, with the field read twice in sequence. At four, the
     meter does not see a chain inside one segment's task while another segment's reads
     overlap it: it counts a round only when a request starts with nothing in flight. That
     is the mirror of M59's caveat, and every depth test that runs a single segment as well
     keeps it honest.
7. **Not split.** `a_multi_query_is_not_split`, in `crates/pstore-server/tests/peers.rs`:
   `multi` alone, with text and with dense send no part on any of the three peered servers,
   and answer as the unpeered one. Seen red with `shares` not refusing `multi`.
8. **Gates.**
   - `./scripts/gates.sh`: green by the pre-commit hook on this commit.
   - **Mutation:** `cargo mutants --in-diff` over the changed source lines, `--no-config`,
     each crate against the tests that exercise it:
     - `pstore-format/src/writer.rs`, against `pstore-format`'s tests: 13 mutants, 3 missed.
     - `pstore-query/src/run.rs`, against `pstore-query`'s: 26 mutants, 1 missed, 8 unviable.
     - `pstore-engine/src/lib.rs`, against `multi` and `quarantine` in `pstore-engine` and
       `pstore-server`: 31 mutants, 2 missed, 10 unviable.
     - `pstore-server/src/lib.rs` and `types.rs`, against `multi` and the library's tests:
       50 mutants, 0 missed. The first run's survivor, the `sum`/`max` guard made `true`, is
       caught by the accepted text-only `sum` and `max` added to
       `multi_vectors_are_refused_at_the_door`.
     - `pstore-server/src/peers.rs`: 1 mutant, `WirePart::of` as `Default::default()`, not
       run. `WirePart` derives no `Default`, so it cannot compile.
   - **The 6 misses, each killed by hand with the test named:**
     - The writer's match arm for field 2 deleted: `a_segment_of_one_or_two_fields_is_written_as_before`
       (added; its fingerprints were taken by running the same fixture on the M60 tree).
     - The writer's `fi == 2` as `!=`, for RaBitQ and for SQ8: the code-section ids now
       asserted in `every_field_reads_its_own_vectors_and_vector_is_field_zero`.
     - A `multi` leg's widening, `limit + extra` as `-`: `a_multi_leg_is_widened_past_deleted_rows`
       (added). Also red with no widening at all.
     - The patch path's reject count, `+=` as `*=`: `a_patch_beside_a_new_width_never_stops_the_fold`,
       which now asserts the count.
     - `shares` returning no share: `a_split_query_answers_exactly_as_one_server_does`, in
       `peers.rs`, which the engine's sweep did not run.

**Code review**, two rounds:
- Round 1, **blocking, fixed:** a patch merges into its folded base row after the fold's width
  pass, so a patch beside a new width sealed two widths, the writer refused, and every later
  fold of the tenant failed. The width pass now runs again over the rows the patches resolve
  to: a row that disagrees is quarantined and its id left untouched.
  `a_patch_beside_a_new_width_never_stops_the_fold` reproduces it, seen red first.
- Round 2: pass. Its minor, that which operation is set aside depends on arrival order, is
  now what the spec and the comment say.
- Accepted minor: a hybrid query whose dense leg meets a segment without `vector` reports the
  `multi` field as `unknown_field`, where it was `500 internal`. Rare, and loud either way.

**Not covered**, as the spec states:
- pieces 2–4: backlog rows 63, 64 and 65;
- `include_vectors`;
- a schema-wide width per field: two widths folded apart are a `schema_conflict` on the query,
  and their compaction fails;
- a quarantined row refused for a named field's width carries no reason in the export:
  `quarantine` recomputes reasons from the schema, which records no such width.
