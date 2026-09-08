# BM25 and Full-Text Search on Object Storage

**Answers:** Q21
**Status:** Complete (v1)

## The good news

Inverted indexes are *already* designed for exactly our access pattern: a term dictionary
mapping to byte ranges of posting lists, with skip structures so you read only what you need.
The classic on-disk inverted index is a ranged-read structure. Almost nothing needs inventing.

Quickwit proves it: a self-contained Tantivy index per split on S3, with a hotcache making
queries answerable by fetching only the relevant byte ranges, at **~10× lower cost than
Elasticsearch**.

## The structures we need

| Structure | Purpose | Where it lives |
|---|---|---|
| **Term dictionary (FST)** | term → posting-list offset; compact, fast prefix lookup | ⚠️ **C-10** — a sibling immutable object, not the index section |
| **Posting lists** | doc ids + term freqs, block-compressed | Data section, ranged GET |
| **Skip lists** | metadata per 128-doc block, seek without scanning | Beginning of the postings data |
| **Block-max metadata** | `fieldnorm_id` + `max_term_freq` per block ⇒ max BM25 contribution | With the skip data |
| **Fieldnorms** | doc length for BM25 | Columnar, cached |
| **Fast fields** | columnar numerics for filter/sort/aggregate | Data section |

> **C-10 — the term dictionary's home, corrected. M5a, measured.** The row above puts it in the
> segment index section, which is bounded by `INDEX_BUDGET` = `SUFFIX_FETCH` − footer =
> **8,150 bytes** so that it arrives with the footer in one suffix read. A SPLADE-sized 30,000
> term vocabulary at 12 bytes an entry is **360 KB — 44× that budget**, and `try_finish`
> *refuses* an over-wide segment rather than opening it slowly: the letter of this row does not
> make the open expensive, it makes the segment **unwritable**.
>
> The dictionary is therefore a **sibling immutable object** whose key is derived from the
> segment's (`<segment>.sdict`), fetched in parallel with the footer and cached `Pinned` —
> which is what M3 already does with the centroid table, and which a section could not be,
> because a section has no cache class of its own. The row's *intent* — cached, fetched before
> the postings, never interleaved with them — is met exactly.
>
> ⚠️ **D-13 is untouched.** It governs block-max/skip metadata, which M5a does not add. Evidence:
> [`M5a/SPEC.md`](../../milestones/M5a/SPEC.md) and its `VERIFIED.md`.

Tantivy's format is directly usable as a reference: FST term dictionary, skip lists written
at the head of the postings when `doc_freq >= 128`, VInt encoding below that (avoiding
skip-list overhead for the long tail of rare terms), and block-max data enabling
**Block-Max WAND / MaxScore** to skip whole blocks that cannot enter the top-K.

## Round-trip budget

```
RT 1: term dictionary lookup            (cached in the index section → usually 0 RTs)
      [compute: plan the query, compute block-max bounds]
RT 2: fetch posting-list blocks for all query terms   (parallel)
      [compute: BMW/MaxScore, accumulate top-K]
RT 3: fetch documents / stored fields for top-K       (parallel)
```

**2–3 round trips, same as vector search.** turbopuffer reports BM25 cold p90 285 ms and
warm p90 18 ms on 1M docs / 300 MB — consistent with this structure.

**Block-Max WAND is more valuable here than on local disk**, because a skipped block is a
skipped *network fetch*, not a skipped page read. The upper-bound pruning must be applied
*before* issuing the fetch, not while decoding. That means the block-max metadata must live
in the cached index section, separate from the posting data — a layout decision that differs
from a local-disk engine, where they are usually interleaved.

> **D-13.** Block-max/skip metadata goes in the **index section** (cached, tiny); posting
> payloads go in the **data section** (fetched by range). This split is the single most
> important FTS layout decision for object storage.

## Build vs. borrow

| Option | Assessment |
|---|---|
| **Embed Tantivy directly** | Fastest path to a good BM25. Mature, Lucene-class, Rust, battle-tested by Quickwit at scale. But: its own segment/directory abstraction, its own merge policy, its own file set — we would be running two storage engines, and our segment format wants FTS as *sections*, not as separate files. |
| **Reimplement, borrowing the format** | Full control, single format, but months of work and a large correctness surface (tokenization, scoring, phrase queries, positions). |
| **Tantivy behind our `Directory` trait** | Tantivy has a `Directory` abstraction (this is how Quickwit does it). We implement `Directory` over `BlobStore` + cache and let Tantivy sub-index live *inside* our segment as a byte range. |

> **D-14.** Start with **Tantivy behind a custom `Directory`** backed by our `BlobStore` +
> cache, with the Tantivy index stored as an opaque section of our segment. This gets a
> production-grade BM25, tokenizers, and query parser immediately, and preserves the
> one-object/one-suffix-GET contract. Revisit a native implementation only if the
> `Directory` indirection costs us round trips we can't recover.

Risks to watch: Tantivy's `Directory` assumes cheap `atomic_read`/small reads; a naive
implementation will produce request storms. The `Directory` must aggressively coalesce and
must prefetch the hotcache-equivalent up front. Quickwit solved this; study their approach.

## Sparse / learned retrieval

Beyond classic BM25, the important adjacent capability is **learned sparse retrieval**
(SPLADE, miniCOIL) — Qdrant supports these natively and they are increasingly standard in
hybrid pipelines. Structurally they are inverted indexes with float impacts instead of term
frequencies, so the same posting-list machinery serves them.

> **D-15.** Design the posting-list format with a **generic impact payload** (u8/f16 impact)
> from day one, so BM25 and learned-sparse share one code path. Retrofitting this later is
> expensive.

> **Sequencing (D-72–D-74).** Sparse vectors should ship **before** BM25: sparse search over
> postings is *exact*, so it is a far smaller subsystem than BM25's tokenizers, analyzers, and
> two-pass IDF — and it delivers a real hybrid story much sooner. Full prerequisites checklist
> for deferring FTS safely:
> [`modalities-and-sequencing.md`](modalities-and-sequencing.md) §6.

## Also needed

- **Trigram index** for regex/glob (turbopuffer offers this) — a second inverted index over
  character trigrams, same machinery.
- **Phrase queries / positions** — an optional positions section, fetched only for phrase
  queries. Keep it in its own section so it is never fetched otherwise.
- **Tokenization/analysis** — language-aware; Tantivy provides this. Stored in the schema, in
  HEAD, versioned (changing analysis requires reindexing — surface that clearly).

## Cost summary

| Query | RA |
|---|---|
| BM25, cold | 2–3 Rseq depth |
| BM25, index section cached | 1–2 Rseq depth |
| BM25, warm | 0 |

## Open questions raised

- OQ-43: Measure Tantivy-over-`Directory` request amplification for a realistic multi-term
  query; set a hard budget and enforce it in tests.
- OQ-44: Can block-max metadata be hoisted out of Tantivy's format into our index section, or
  must we accept its layout?
- OQ-45: Impact-ordered vs doc-ordered posting lists — impact-ordered enables early
  termination with fewer fetched bytes, which matters more on object storage than locally.

## Sources

- [Inverted Index Storage — tantivy (DeepWiki)](https://deepwiki.com/quickwit-oss/tantivy/8.3-term-dictionary-and-posting-lists)
- [What is Tantivy? Rust Full-Text Search Library — Spice AI](https://spice.ai/learn/tantivy)
- [Quickwit 101 — Architecture of a distributed search engine on object storage](https://quickwit.io/blog/quickwit-101)
- [A hypothetical search engine on S3 with Tantivy and warm cache on NVMe — shayon.dev](https://www.shayon.dev/post/2025/314/a-hypothetical-search-engine-on-s3-with-tantivy-and-warm-cache-on-nvme/)
- [RISE: A Rust Library for Inverted Index Search Engines — arXiv](https://arxiv.org/html/2606.07187v1)
- [turbopuffer: fast search on object storage (BM25 cold/warm latencies)](https://turbopuffer.com/blog/turbopuffer)
