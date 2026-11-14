# M231: columnar attribute chunks in a block

## Serves
Item 2 of [the column-storage adoption list](../../comparisons/column-storage-adoption.md).
[file-format-and-layout.md](../../research/05-storage-engine/file-format-and-layout.md)
requirement 3 ("column chunks, columnar, per-block encoded"), never built; OQ-26's
benchmark question for attributes. D-34 unchanged.

## Delta
Today a block is row-major (`docs.rs` `encode_rows`): per row the id, then every attribute's
name, a tag and the value, uncompressed. A filter decodes every row to reach one name.

After, a segment at **`VERSION` 3** (`VERSION_COLUMNAR`) has columnar blocks:
`u8 layout (1)`, `u32 rows`, the id column, then per attribute name, in name order, the name
**once**, its body's length (so a projection skips it without reading it), and its body:
- presence: every row, or a bitmap;
- one kind for the present values: int, datetime and uint64 as an integer column; float as
  decimal integers when every value round-trips through `i / 10^e` bit for bit (`e` <= 15),
  else raw; bool as a bitmap; string as a byte column; uuid raw; anything else (arrays,
  bytes, mixed types) as M9a's tagged values, in row order;
- an **integer column** is the smaller of frame-of-reference (minimum, GCD, bit width,
  packed) and the same over successive deltas; a constant column is width 0;
- a **byte column** is the smaller of plain (lengths as an integer column, then the bytes) and
  a block dictionary (entries, then ordinals as an integer column), its bytes LZ4-compressed
  when that saves an eighth.

Every codec is decoded back by the writer's tests for every kind; nothing is lossy. Decoding
builds only the projected columns' values, and with a pick only the picked rows' values.
The index section is written as a typed one (M9h.1) for every v3 segment.

Unchanged: block ranges, offsets and zone maps, so a query's requests and depth; v1 and v2
segments, read as before forever. A reader before M231 refuses a v3 segment at `open`
(`UnsupportedVersion`), never misreads it. `SegmentWriter::with_row_blocks()` writes the v1/v2
layout, for a rollout that must stay readable by an older binary and for the fixtures that
pin old bytes. Compaction rewrites what it merges by the setting below.

**A setting, by the "a feature with a measured downside is a setting" non-negotiable**
(rollback and write CPU are real downsides). `BlockOptions { columnar, equality, aggregates }`
(M230's section and M232's statistics ride on the same setting):
- the **cluster default**, `PSTORE_BLOCK_LAYOUT` (`columnar` unset, or `rows`),
  `PSTORE_EQUALITY_FILTERS` and `PSTORE_BLOCK_AGGREGATES` (`on` unset, or `off`), set alike on
  every server; `Engine::with_block_options`, `Api::set_block_options`;
- an **index override** in HEAD, a trailing section after M200's (per index one byte, two bits
  a setting: unset, off, on; written only for indexes that exist and override something), set
  by `PATCH /v1/indexes/{index}` `{"storage": {...}}` (absent leaves, `null` clears) and
  committed by CAS with a merge inside the loop (`Engine::set_storage`);
- every fold and compaction seals by the override over the sealing server's default;
  `GET /v1/indexes/{index}` reports `storage` (effective values and the overrides).

## Acceptance criteria
1. Round trip: for seeded random blocks over every `Value` kind, absent attributes, mixed
   types under one name, empty strings, extreme ints, uint64 above `i64::MAX`, `-0.0`, and
   floats that are and are not decimal, a v3 segment's `rows_where`, `rows_at` and `scan`
   return exactly what the same documents' v2 segment returns, projected and picked.
2. Every truncation of a v3 block, and a block with a corrupted codec byte, width or ordinal,
   is refused with an error, never a panic or a wrong row.
3. Old segments: the M15.2 sketch fixtures and the typed-value byte fixtures, written with
   `with_row_blocks()`, keep their recorded bytes and answers; a v3 segment's footer version
   is 3.
4. `scripts/depth.sh` green: a query's depth and requests are unchanged.
5. `./scripts/columns.sh` at 20,000 rows, v3 against v2 (`provisional`): segment bytes,
   every filter's bytes and mask time, a 10-row resolve's time, a whole-segment decode's time,
   and write time, all recorded in `VERIFIED.md`. Adopted only if segment bytes fall by at
   least a third and no filter's mask time or resolve time rises by more than 10%; otherwise
   the measured costs are recorded and the writer's default stays row-major.
6. The setting: an override wins over the default for every later fold and compaction, by any
   server, and survives a restart; a changed setting keeps the others; cleared, the default
   returns and HEAD forgets it; a HEAD without overrides keeps its bytes, and a malformed
   override is refused; the environment's values and the patch's refusals are named.
7. Both modes tested: server tests that pin bytes, read counts or pruning ratios measured on
   row blocks set the row layout and keep every assertion; new tests hold columnar pruning.

## Test plan
- 1: `columnar_blocks_answer_as_row_blocks_do` (pstore-format `tests/it/columnar.rs`), and
  unit tests in `columnar.rs` per codec (an integer column per width 0 to 64, delta chosen for
  ascending values, dictionary chosen for repeats, LZ4 chosen for redundant bytes).
  Kills: a bit-packing off-by-one; GCD dropped on decode; a dictionary ordinal width wrong;
  presence bitmap inverted; decimal exponent off by one.
- 2: `a_truncated_or_corrupt_columnar_block_is_refused` (every cut, every codec byte).
- 3: existing fixture tests, changed only to pass `with_row_blocks()`, listed in `VERIFIED.md`.
- 4, 5: the scripts.
- 6: `storage_overrides_round_trip_last`, `an_override_wins_over_the_default_only_where_set`
  (head.rs); `an_override_wins_over_the_default_for_every_later_seal`,
  `a_cluster_default_of_rows_is_overridden_per_index` (engine `tests/it/storage.rs`);
  `the_cluster_default_is_read_from_the_environment`,
  `an_index_overrides_the_cluster_default_and_says_so` (server `tests/it/storage.rs`).
  Kills: the override ignored at seal; the default ignored; the byte's codes swapped; an
  unset override written; a patch replacing rather than merging.
- 7: `columnar_blocks_prune_for_float_and_int_columns`,
  `a_datetime_column_prunes_its_columnar_blocks`.

## RA budget
Unchanged requests and depth. A storage patch: one HEAD read and at most one CAS PUT, as M149's
read-only patch. Each HEAD read grows by an override's bytes only for indexes that set one. Bytes per block fall by what the encodings save.

## Risks
- Decode CPU of a whole block may rise (bit unpacking, LZ4) while bytes fall; the bench's
  whole-segment decode and resolve times show it.
- Write CPU rises (each column tries two codecs); the bench's write time shows it.
- Rollback: v3 segments are unreadable by a pre-M231 binary. Mitigation: `PSTORE_BLOCK_LAYOUT=rows`
  before a rollback; segments already columnar need a rewrite (docs/deploy.md).
- Rollback with an override set: the override is a trailing HEAD section, which a pre-M231
  binary refuses (`CorruptHead`) for the whole tenant. Mitigation: no override until the
  rollout is complete, every override cleared before a rollback (docs/deploy.md); a HEAD
  without overrides keeps its bytes (criterion 6).
- Servers disagreeing mid-rollout write both layouts into one index: correct by construction
  (the reader dispatches on each segment's version), held by criterion 6's two-server test.

## Tasks
- M231.1: `columnar.rs` codecs, v3 writer and reader, tests, bench arm, VERIFIED.md.
- M231.2: storage settings: cluster default, index override in HEAD, PATCH and summary,
  OpenAPI and clients, deploy docs; existing server tests pinned to rows.
