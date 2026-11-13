# Scalar and column storage: what pstore can adopt

**Snapshot:** 2026-11-13. **pstore:** `2efbcf9` (main after M210 and M211).
**Compared:** OpenSearch's sandbox Parquet data format (`huyz0/OpenSearch` at `fae98a3a`),
ClickHouse MergeTree (public documentation), hsearch (`huyz0/hsearch` at `734b069`) and
lucene-rust (`huyz0/lucene-rust` at `55cb6d78`).
**Audience:** whoever plans the next storage milestone. This is research only: nothing here
changes code, and no number in it was measured by this report. Size figures marked
*estimate* are arithmetic over the encoders' byte layouts, not runs.

## The answer in one paragraph

pstore's native segment stores attributes **row by row**: each block is a list of documents,
and each document repeats every attribute's name, a type tag and the value, with no
compression. The design document asked for columnar attributes
([file-format-and-layout.md](../research/05-storage-engine/file-format-and-layout.md),
requirement 3) and it was never built. Every system compared here stores scalars as typed,
per-block-encoded columns, and the ideas worth adopting all follow from that. **Ranked, they
are:** (1) prune string equality with what the segment already has and a small per-block
set or Bloom filter, (2) columnar attribute chunks with lightweight integer encodings and a
per-block dictionary for strings, (3) per-block aggregate statistics that answer count, sum,
min and max without fetching a block, (4) late materialization once columns exist, (5) a
predicate-mask cache, and (6) a Parquet export. **Adopting Parquet or Lance as the native
format stays rejected** (D-6), and OpenSearch's own design is evidence for that rather than
against it. The recommended next milestone is item 1, which is small, additive and needs no
format version; item 2 is the larger milestone after it.

## How pstore stores scalars today

Read from `crates/pstore-format` and `crates/pstore-query`.

| Aspect | Today | Where |
|---|---|---|
| Block layout | Row-major. A block is `count`, then per row the id (length + bytes) and `encode_attrs`: attribute count, then per attribute the name (length + bytes), a one-byte tag and the value | `docs.rs` `encode_rows`, `encode_attrs` |
| Value types | int, uint64, float, bool, string, datetime, uuid, bytes, arrays of scalars, null elements | `lib.rs` `Value` |
| Compression | None for native blocks. `snap`, `zstd` and `lz4_flex` are dependencies, used only to read external Parquet and Lance pages | `pstore-format/Cargo.toml`, `external.rs` |
| Block size | Chosen by the writer: it doubles rows per block until the index section fits the 8 KiB opening suffix read (`INDEX_BUDGET`), so a large segment has few, large blocks | `writer.rs` |
| Pruning | Per-block zone maps (min, max) for int, float, datetime and uint64. **No zone for strings, bools or arrays.** A Bloom filter of trigrams per block per *declared* attribute (M15.2), used only by pattern filters | `lib.rs` `Zones`, `trigram.rs`, `filter.rs` `could_admit` |
| Filter evaluation | Each unpruned block is fetched whole and decoded with a projection (M119), which still reads through every row's id, every attribute and the text body to reach the one attribute the filter names. The result is a per-segment bitmap (`Mask`) | `docs.rs` `decode_rows_where`, `filter.rs` `Mask` |
| Hit resolution | The blocks holding hits are fetched and only the picked rows are built (M121) | `docs.rs` `decode_rows_picked` |
| Aggregation | Count, sum, min, max, avg and grouping fold row by row over the admitted rows (M12, M210, M211) | `pstore-query/src/aggregate.rs` |
| External columns | Parquet and Lance columns read in place, including snappy, gzip, zstd, LZ4 and brotli pages, dictionary pages and Lance 2.1's FastLanes bit-packing (M68 to M102) | `parquet.rs`, `parquet/chunks.rs`, `lance.rs`, `lance/fastlanes.rs` |

What the row-major layout costs, as an *estimate* for a row with `category` (an 8-character
string), `price` (int) and `ts` (datetime): the three attributes take 25 + 18 + 15 bytes plus a
4-byte count, about 62 bytes before the id. Column-encoded, a dictionary ordinal, a
bit-packed price offset and a delta-coded timestamp typically take 1 to 4 bytes together, so
the attribute part of a block shrinks by roughly an order of magnitude, before any general
compression. This matters for three budgets pstore enforces: **query bytes** (`depth.sh`
gates them), **decode CPU** (M119 found that decoding rows was most of a filtered query's
time) and **cache footprint** (every byte a block holds is a byte the foyer cache does not
spend on another tenant).

What it does well and must keep: one suffix GET opens a segment, blocks are independently
addressable by range, an old segment stays readable forever because sections are additive
and ids are never reused, and a request count never depends on how many columns there are.

## What the others do

### OpenSearch: Parquet as a secondary format, not a replacement

The Parquet work lives in OpenSearch's `sandbox/`, not in the shipped engine.

- **`composite-engine`** writes each document to a *primary* format (Lucene by default) and to
  any number of *secondary* formats, linked by a `RowIdGenerator` row id. Fields are routed to
  a format by its declared capabilities. Lucene stays the format of record and owns merges.
- **`parquet-data-format`** (Rust, `arrow` + `parquet` crates over FFM) writes the secondary
  Parquet files. Defaults: **LZ4_RAW** compression, 1 MiB data pages, a 20,000-row page limit,
  2 MiB dictionary pages, row groups of up to 1,000,000 rows or 128 MiB, Bloom filters **off**
  by default (FPP 0.1, NDV 100,000 when on). Encoding and compression are settable per field
  and per type (PLAIN, RLE_DICTIONARY, DELTA_BINARY_PACKED, DELTA_BYTE_ARRAY,
  BYTE_STREAM_SPLIT; ZSTD, SNAPPY, GZIP, BROTLI). A sorted merge emits a row-id mapping so the
  Lucene side can be remapped.
- **`analytics-backend-datafusion`** runs SQL/PPL plans (Calcite, then Substrait) on DataFusion
  over those files. Its `indexed_table` path is the interesting part: **Lucene evaluates the
  text and keyword predicates into a RoaringBitmap per row group**, `page_pruner.rs` prunes
  pages with Parquet's page index (min, max, null count, rewritten through the whole boolean
  tree so `OR` prunes correctly), and `row_selection.rs` turns the bitmap into a Parquet
  `RowSelection` whose skips are at least `min_skip_run` rows, absorbing short gaps to keep the
  selector list small. Residual predicates run on decoded Arrow batches.
- `ScanCapability` names the split plainly: `DocValues` is "columnar doc values (Parquet,
  Lucene doc values)", `StoredFields` is row-oriented.

Lesson for pstore: OpenSearch did **not** move search onto Parquet. It added Parquet beside
Lucene for analytic scans with million-row row groups, the access pattern D-6 rejected
Parquet for. The idea that carries over is the *shape of the filter path*: an index produces
a bitmap, the bitmap becomes a coarse selection with a minimum skip run, and page statistics
prune what the bitmap cannot.

### ClickHouse MergeTree

From the ClickHouse documentation and engineering blog (sources at the end).

- **Columns per part.** A wide part keeps one file per column; a compact part (small parts)
  keeps all columns in one file. Rows are grouped into **granules** of 8,192 rows (adaptive,
  capped by bytes), and a **marks** file maps each granule to its offset in the compressed
  column file. The primary index is **sparse**: one key per granule, and rows are sorted by
  `ORDER BY`.
- **Codecs are chained**: a type-aware transform, then a general compressor. Transforms:
  `Delta`, `DoubleDelta` (timestamps), `Gorilla` and `FPC` (floats), `T64` (crops unused high
  bits of integers in 64-value blocks), `GCD`. General: `LZ4` (default), `LZ4HC`, `ZSTD(level)`.
- **`LowCardinality(String)`** stores a dictionary plus ordinals, transparently. **Sparse
  serialization** stores only non-default values and their positions when a column is mostly
  default.
- **Data-skipping indexes** per *N* granules: `minmax`, `set(N)` (the distinct values of the
  block, up to N), `bloom_filter`, the older token and n-gram Bloom filters (now deprecated for
  full text in favour of the inverted `text` index), and `vector_similarity`.
- **`PREWHERE`** reads the filter columns first and the rest only for surviving granules;
  **lazy materialization** (25.4, on by default) extends that past sorting and `LIMIT`, so a
  top-*k* reads the wide columns only for the *k* rows kept. The **query condition cache**
  remembers, per part and predicate, which granules matched, so a repeated filter skips the
  rest without reading them.

### hsearch

hsearch uses lucene-rust as it is: a keyword field gets **sorted doc values**, integer and date
fields go through `LongField` (points plus numeric doc values), and aggregations call
`lucene_search::aggs` over doc values (`crates/engine/src/aggregations.rs`). Its remote
directory fetches **whole** immutable Lucene files into a bounded local cache, with "range-native
reads arrive later" written in `remote.rs`. It has no column format of its own, so what it
offers pstore is what lucene-rust offers.

### lucene-rust

- **`Lucene90DocValuesFormat`** (`crates/lucene-codecs/src/doc_values.rs`), read and write,
  all five doc-values types. NUMERIC values are constant, table-compressed (at most 256
  distinct values, bit-packed ordinals) or delta/GCD bit-packed, optionally in 16,384-value
  blocks of varying width. SORTED is a per-document ordinal into a prefix-compressed terms
  dictionary; the multi-valued forms address a flat array through a monotonic offsets array.
  Sparse fields carry an **IndexedDISI** presence structure (sparse, dense or all, per 65,536
  documents). An optional **skip index** (`.dvs`) holds multi-level per-interval min, max and
  document counts, used by range queries and sorts.
- **`Adaptive90`** (lucene-rust's own format, `adaptive_values.rs`): per **128-value** block,
  chooses the smallest of constant, delta, offset, GCD and a lossless **decimal** transform
  (ALP without exceptions), then frame-of-reference bit-packing. Every candidate is decoded
  back and kept only if every value round-trips, so a misdetected type can never corrupt a
  column. **`adaptive_zones.rs`** adds min, max and, when exact, the **sum** for every
  1,024-value window, so a metric aggregation over a window every document of which matches
  reads three numbers instead of 1,024 values. Its logs preset measured adaptive doc values as
  19 MiB of a 40% total saving, with scans 1.2 to 2 times slower
  (`docs/logs-preset.md`, `docs/benchmarks/settings-defaults.md`). Those are lucene-rust's
  numbers, on its hardware, not pstore's.
- Building blocks already ported in Rust under Apache-2.0, the licence pstore uses:
  `for_util.rs`, `packed_ints.rs`, `direct_reader.rs`, `direct_monotonic.rs`,
  `indexed_disi.rs`, `lz4.rs`.

## Ranked adoption list

Ranked by value for pstore's enforced budgets (round trips, requests, query bytes) over
effort. Effort: **S** is a milestone of a few tasks inside one crate, **M** touches format and
query, **L** is a new format version with a compatibility story.

| # | Adopt | From | Effort | What it buys | Constraint it must respect |
|---|---|---|---|---|---|
| 1 | **String and bool equality pruning.** (a) Use the trigram sketch already in the meta region for `Eq` and `In` on declared string attributes of three or more characters: a value equal to the literal contains all its trigrams. (b) Add a per-block `set(N)` section (the block's distinct values when there are at most N, else nothing) and, beyond N, an equality Bloom filter, sized by the same "widest that fits the spare budget" rule as the trigram sketch | ClickHouse `set(N)`, `bloom_filter`; Parquet Bloom filters | **S** | Today a string equality prunes no block, so `category = "shoes"` fetches every block of every segment. Typical tenant filters are exactly this shape | A new `Section` id; absent means "prunes nothing", so no version bump. Sound, never exact: a block is skipped only on a proven "no". Fits inside `INDEX_BUDGET` or is not written |
| 2 | **Columnar attribute chunks per block.** Within each data block: the id column, then one chunk per attribute name (name written once), each with a presence bitmap and a typed encoding: integers and datetimes by frame-of-reference bit-packing after the best of delta, offset and GCD; floats plain or decimal; strings by a per-block dictionary when it is smaller, else offsets plus bytes; then LZ4 per chunk. The block keeps its range, so a query's request count is unchanged | lucene-rust `Adaptive90` (transform choice, decode-to-verify); ClickHouse codecs and `LowCardinality`; Lucene IndexedDISI and ClickHouse sparse serialization for presence; Lance 2.1 mini-blocks | **L** | Roughly an order of magnitude fewer attribute bytes (*estimate*, above); a filter decodes only its columns instead of walking every row (M119's cost); smaller blocks in cache | `VERSION` 3, refused by older readers. v1 and v2 segments stay readable forever, and compaction rewrites them. Per-column chunk offsets live in the block's own header, not the index section, so they cost no `INDEX_BUDGET`. The FastLanes unpacker already in `lance/fastlanes.rs` and lucene-rust's `for_util` are candidate kernels; `unsafe` stays in `pstore-kernel` |
| 3 | **Per-block aggregate statistics.** Count of present values and, when exact, the sum for each numeric attribute beside its zone map. When a predicate provably admits every row of a block (a new `must_admit(zones)` beside `could_admit`), `count`, `sum`, `min`, `max` and `avg` come from the index section and the block is not fetched | lucene-rust `adaptive_zones`; Lucene skip index; Parquet and ClickHouse statistics | **M** | An unfiltered or range-filtered aggregation over a large index costs only open round trips, no data GETs | Budget: 16 more bytes per numeric attribute per block, so it competes with zone maps for the 8 KiB suffix and is dropped first. Must account for rows deleted after the segment was written |
| 4 | **Late materialization.** With item 2, decode the filter's columns first, build the mask, then decode other columns only for admitted rows, and for a top-*k* only for the *k* kept | ClickHouse `PREWHERE` and lazy materialization; OpenSearch bitmap to `RowSelection` | **S** after 2 | CPU, not requests. Extends M119 and M121 to every column | Nothing new on disk |
| 5 | **Predicate-mask cache.** Segments are immutable, so `(segment key, predicate digest)` to `Mask` can never be stale. Cache the masks of recent filters in the existing node cache | ClickHouse query condition cache | **S to M** | A repeated filter (dashboards, paginated queries) skips zone checks and block decodes | Bounded bytes, charged to the memory reservations; nothing durable; correctness never depends on it |
| 6 | **Parquet export** of an index's rows and attributes, for analytics engines | OpenSearch `parquet-data-format` writer settings; D-6's promised export path | **M** | Interoperability with DuckDB, Spark and DataFusion without a pstore client | Writes customer-owned objects; uses `arrow` and `parquet` crates, which `cargo deny` must accept |

### Not recommended

| Idea | Why not |
|---|---|
| **Parquet as the native segment format** | Its footer is Thrift and its page index is a separate region, so random access needs more than the one suffix GET D-6 requires, and it has no home for postings, centroids or code sections. OpenSearch's sandbox confirms the split: it keeps Lucene for search and puts Parquet beside it for scans over million-row row groups |
| **Lance as the native format** | Closest in spirit (mini-blocks give random access in one or two reads) and OQ-26 remains open, but owning the footer contract, section ids and per-block layout is what keeps old segments valid forever. Reading Lance in place (M82 to M102) already gives the interoperability |
| **A composite primary/secondary engine** like OpenSearch's | Two formats per write doubles PUTs per fold and needs a row-id remap on every merge, against the rule that requests scale with nodes and bytes |
| **Sorting a segment's rows by an attribute** (ClickHouse `ORDER BY`, lucene-rust's index sort) | Tightens zone maps a lot, but the block order belongs to the vector and text layout, and one attribute's sort helps one filter. Revisit only if a tenant declares a primary sort key and measurements show the zone maps are loose |
| **Arrow as the in-memory form** (D-6) | Pays off for scan-heavy SQL, which pstore does not run. The `Document` model and the M119 projections are enough until item 2 lands; revisit if item 6 grows into a query path |
| **Gorilla, FPC and other float XOR codecs** | Built for time series floats from one sensor; pstore's floats are arbitrary attributes. Item 2's decimal transform covers prices and rates, the common case |

## Recommended next milestone

**Item 1, string and bool equality pruning**, at the next free milestone number when it is
claimed (M230 on main today; numbers M170 to M189 and M203 to M229 are reserved by other
threads). It is the cheapest change with the largest effect on the most common tenant filter,
it is additive (no format version, old segments unaffected), and it fits the existing
trigram-sketch mechanism. Draft acceptance criteria, each checkable by a test or a gate:

1. An `Eq` on a declared string attribute over a segment where no block holds the value
   fetches **zero** data blocks (request counter in a test), and returns the same rows as
   with the section absent.
2. Soundness: a property test over random blocks and literals never prunes a block that holds
   a matching row, for `Eq`, `In` and `Pattern`.
3. A segment written without the new section, and every segment written before it, opens and
   answers exactly as before (an existing fixture, byte for byte).
4. The index section still fits `INDEX_BUDGET` at the depth gate's 20,000 rows, and
   `scripts/depth.sh` stays green, so no query gains a round trip.
5. `scripts/depth.sh` reports query bytes for a selective string-equality filter at gate scale
   lower than on current main, with both numbers recorded in the milestone's `VERIFIED.md`
   as `provisional`.

**Item 2 follows** as its own milestone with a spec that starts from OQ-26's benchmark
question: measure segment bytes, filtered-query bytes and decode time at gate scale on v2
versus v3 before choosing the encodings to keep, the way lucene-rust's `Adaptive90` keeps a
transform only when it pays.

## Sources

Repositories, read at the commits in the snapshot line:

- pstore: `crates/pstore-format/src/{lib,docs,writer,reader,trigram,external,parquet,lance}.rs`,
  `crates/pstore-query/src/{filter,aggregate}.rs`,
  `docs/research/05-storage-engine/file-format-and-layout.md`
- OpenSearch: `sandbox/plugins/composite-engine/README.md`,
  `sandbox/plugins/parquet-data-format/src/main/rust/src/{native_settings,writer_properties_builder,field_config}.rs`,
  `sandbox/plugins/parquet-data-format/src/main/rust/src/merge/mod.rs`,
  `sandbox/plugins/analytics-backend-datafusion/README.md`,
  `sandbox/plugins/analytics-backend-datafusion/rust/src/indexed_table/{mod,page_pruner,row_selection,parquet_bridge}.rs`,
  `sandbox/libs/analytics-framework/src/main/java/org/opensearch/analytics/spi/ScanCapability.java`
- hsearch: `crates/engine/src/{schema,aggregations,remote}.rs`
- lucene-rust: `crates/lucene-codecs/src/{doc_values,adaptive_values,adaptive_zones,indexed_disi}.rs`,
  `crates/lucene-index/src/document/column.rs`, `docs/logs-preset.md`,
  `docs/benchmarks/settings-defaults.md`

ClickHouse, read 2026-11-13:

- [Column compression codecs](https://clickhouse.com/docs/sql-reference/statements/create/table#column_compression_codec)
- [Data skipping indexes](https://clickhouse.com/docs/optimize/skipping-indexes) and
  [best practices](https://clickhouse.com/docs/best-practices/use-data-skipping-indices-where-appropriate)
- [Lazy materialization](https://clickhouse.com/docs/optimize/lazy-materialization) and the
  [introducing blog post](https://clickhouse.com/blog/clickhouse-gets-lazier-and-faster-introducing-lazy-materialization)
- [Altinity: new encodings in ClickHouse](https://altinity.com/blog/2019-7-new-encodings-to-improve-clickhouse)
  (2019; older benchmark numbers)
