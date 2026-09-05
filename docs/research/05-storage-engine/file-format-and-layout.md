# Segment Format: One Object, One Suffix GET, Everything Derivable

**Answers:** Q16
**Status:** Complete (v1)

## Requirements

1. **One `Range: -N` suffix GET must bootstrap the entire object.** (Pattern 6.)
2. Random access to individual blocks by byte range — no full-object reads.
3. Columnar for attributes (filters, aggregations), row-ish for retrieval, plus vector and
   posting-list payloads. All three in one object.
4. Self-describing: never require a side lookup to interpret a segment.
5. Cheap to produce (compaction writes these constantly) and cheap to skip (zone maps).

## Why not Parquet

Parquet is the obvious default and the wrong choice here:
- Optimized for **large sequential scans**, not point/random access. Lance reports **~100×
  faster random access** than Parquet, which is the access pattern that dominates us
  (fetch these 200 documents by id, fetch this posting block, fetch this cluster).
- Row-group + page metadata is verbose and requires multiple reads to navigate for random
  access.
- No natural home for vector index structures, posting lists, or centroid tables.
- We would end up with Parquet for attributes + separate files for everything else, breaking
  requirement 1 and multiplying request count.

**Lance** is much closer to right (columnar + fast random access + vector indexes co-located
+ versioning, and v2.2 claims >50% storage reduction and up to 68× faster blob reads). It is
a genuine option and we should benchmark against it.

> **D-6.** Design our own segment format, but keep **Arrow as the in-memory representation**
> so we inherit the compute ecosystem, and keep a Lance/Parquet **export** path for
> interoperability. Reasons to own the format: the footer/suffix-GET contract, the
> block-alignment-to-`G*` requirement, posting lists and centroid tables as first-class
> sections, and per-block encryption/tenancy — none of which we can impose on an external
> format.

## Layout

```
┌──────────────────────────────────────────────────────────────┐
│ 0: MAGIC + format version                                    │
├──────────────────────────────────────────────────────────────┤
│ DATA SECTION  (block-aligned, each block independently        │
│   decodable, compressed, and ~64 KiB–4 MiB to match G*)      │
│   • column chunks (attributes, columnar, per-block encoded)  │
│   • document payload blocks (row-oriented, for retrieval)    │
│   • vector blocks (quantized codes; full vectors optional)   │
│   • posting-list blocks (BM25, block-max skip data)          │
│   • cluster/centroid payload blocks                          │
├──────────────────────────────────────────────────────────────┤
│ INDEX SECTION  (the "hotcache")                              │
│   • block directory: offset, length, codec, row range        │
│   • zone maps: per-block min/max/null/count per column       │
│   • bloom / ribbon filters for id and low-cardinality columns│
│   • centroid table (small; the ANN entry point)              │
│   • term dictionary (FST) + posting-list offsets             │
│   • doc-id → (block, offset) map                             │
├──────────────────────────────────────────────────────────────┤
│ FOOTER (fixed size, last N bytes)                            │
│   index_section_offset, index_section_len, checksums,        │
│   row_count, schema_ref, epoch, codec versions, MAGIC        │
└──────────────────────────────────────────────────────────────┘
```

### The footer contract
`GET Range: bytes=-4096` returns the footer plus, usually, the tail of the index section.
From it, the reader computes the index section's byte range and fetches it — **and that is
the whole bootstrap: 2 GETs, worst case, for any segment of any size.** Often 1, because a
small segment's entire index section fits in the 4 KiB suffix, and because index sections
are aggressively cached (Quickwit's hotcache is <0.1% of split size — 10 MB for a 15 GB
split; ours should be similar).

> **The index section is the single most valuable thing to cache.** It is tiny, it is needed
> by every query, and it converts every subsequent access into an exact byte range. Cache
> admission must treat it as a distinct, higher-priority class (see `07-caching/`).

### Block sizing
Blocks are sized to the range-coalescing break-even `G*` (64 KiB–4 MiB, backend-tuned). Too
small ⇒ excess requests. Too large ⇒ wasted bandwidth and decode. This is the one tunable
that most directly trades request count against bytes.

### Encoding
- Attributes: dictionary + RLE + bit-packing + FOR (frame of reference), chosen per block by
  a cheap sampler. Arrow-compatible on decode.
- **Strings and document ids use the Arrow `BinaryView` / German-string layout**: a 16-byte
  view with ≤12 bytes inlined and a 4-byte prefix for short-circuit comparison. This takes
  `filter`/`gather` from O(n·k) to O(n) — Polars reports pathological cases resolved, DataFusion
  20–200% on string-heavy queries — and most document ids inline entirely, removing an
  indirection from the doc-id lookup every query performs. **A format decision: free now,
  expensive to retrofit** ([`../09-rust-stack/hot-loop-performance.md`](../09-rust-stack/hot-loop-performance.md) §7).
- Compression: **zstd** by default at a low level; **lz4** for hot blocks where decode CPU
  matters more than size. Per-block, recorded in the block directory.
- Vectors: quantized codes stored contiguously and SIMD-aligned (see `06-indexing/quantization.md`).
  Full-precision vectors optional and stored in a **separate section** so they are never
  fetched during search, only during exact rerank.

## Segment naming

```
{h}/idx/{id}/s{shard}/seg/{level}/{epoch:020}-{ulid}.seg
```
Fully derivable from the manifest; never listed. `level` in the key makes level-scoped GC and
prefix-spread straightforward.

## Segment sizing

Target **256 MiB – 4 GiB**. The bounds come from opposite directions:
- **Lower bound** from *query fan-out*: every additional segment a query must open adds at
  least one round trip. Quickwit merges 10 splits up to 10M docs for exactly this reason.
- **Upper bound** from *compaction cost*: rewriting a 50 GB segment to apply 1% deletes is
  intolerable write amplification, and multipart upload of huge objects has its own tail
  latency.

## Forward compatibility for modalities we haven't built

The footer names which sections a segment contains, and readers skip what they do not find. A
v1 dense-only segment simply has no `sparse_postings`, `positions`, or `term_dict` section —
and because segments are immutable, old ones stay valid forever. **Reserve those section
identifiers in v1** (D-74): reserving a name costs nothing, while a format version bump across
50M indexes costs a year. See
[`../06-indexing/modalities-and-sequencing.md`](../06-indexing/modalities-and-sequencing.md).

## Sidecar principle

**There are no sidecars.** Everything about a segment lives inside the segment. A design that
wants a companion file (a `.deletes`, a `.stats`, a `.bloom`) is adding a round trip and a
consistency problem; the answer is a new section in the footer-addressed index. The one
exception is **tombstones**, which are intentionally separate because they mutate after the
segment is sealed (see `mutations-and-mvcc.md`).

## Cost summary

| Operation | RA |
|---|---|
| Open segment (cold) | 1–2 Rseq |
| Open segment (index section cached) | **0** |
| Fetch k blocks | 1 Rpar round after coalescing |
| Retrieve n documents by id | 1 Rpar round (doc-id map is in the cached index section) |

## Open questions raised

- OQ-26: Benchmark our format vs. Lance for random access and scan. If Lance wins and can
  host our sections, adopt it — owning a format is a large ongoing cost.
- OQ-27: Optimal footer suffix size (4 KiB?) — should cover the footer plus the index section
  for the small-segment common case.
- OQ-28: Should the index section be a *separate object* for very large segments so it can be
  cached and replicated independently? Trades 1 request for cache granularity.

## Sources

- [Lance: Efficient Random Access in Columnar Storage through Adaptive Structural Encodings — arXiv](https://arxiv.org/html/2504.15247v1)
- [Benchmarking Random Access in Lance — LanceDB](https://www.lancedb.com/blog/benchmarking-random-access-in-lance)
- [Apache Parquet vs. Newer File Formats (BtrBlocks, FastLanes, Lance, Vortex)](https://dipankar-tnt.medium.com/apache-parquet-vs-newer-file-formats-btrblocks-fastlanes-lance-vortex-cdf02130182c)
- [Quickwit 101 — splits, hotcache, split_footer](https://quickwit.io/blog/quickwit-101)
- [lance-format/lance — GitHub](https://github.com/lance-format/lance)
