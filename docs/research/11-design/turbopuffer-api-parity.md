# API Parity with turbopuffer

**Synthesizes:** D34
**Status:** v1 comparison, 2026-09-25

What a turbopuffer client can do that a pstore client cannot, row by row, and which
milestone closes each gap — or why it is not closed. The architecture comparison is
[`turbopuffer.md`](../01-prior-art/turbopuffer.md); this is the **API** one.

⚠️ **Provenance.** turbopuffer's docs site is blocked by this environment's egress proxy.
Its side is taken from the official Python SDK `turbopuffer` **2.10.2** (PyPI, 2026-09-23),
whose types are generated from turbopuffer's OpenAPI spec (3.1.0, base
`https://{region}.turbopuffer.com`, 16 endpoints), and from search snippets of the docs
pages for limits and guarantees. Names and shapes are exact; the limits marked *snippet* are
not. Ours is read from the code at `329a7c1` (M8m), not from [`api-design.md`](api-design.md),
which is the proposal — **most of it is not built.**

## The one-line summary

turbopuffer is a **document store with search**: rows are upserted and deleted by id, carry
typed attributes, and every query can filter, order and return them. pstore today is a
**search index with ids**: an append-only write of `{id, vector, text}`, and a query that
returns `(id, score)`. Every gap below follows from that one difference, and the order of the
M9 milestones is the order in which the difference has to be closed.

## Write — `POST /v2/namespaces/{ns}` vs `PUT /v1/indexes/{index}/documents`

| turbopuffer | pstore today | Gap → milestone |
|---|---|---|
| `upsert_rows` / `upsert_columns`: **overwrite by id** | append; a repeated id is a **second row**, and both can be returned | **M9c** — last write wins by id, as [`mutations-and-mvcc.md`](../05-storage-engine/mutations-and-mvcc.md) already specifies |
| `deletes: [id]` | none | **M9c** — tombstones, then delete vectors |
| an attribute set to `null` means "unset" | refused, `400` (M9a) | **M9c** decides, once a write can overwrite: `null` only means something against a prior value |
| Row = `{id, vector?, <attr>: value…}` | `{id, vector, text?}` only; the engine's `attrs` are unreachable over HTTP | **M9a** |
| `patch_rows` / `patch_columns`, `patch_by_filter`, `delete_by_filter` | none | Deferred: each is a read-modify-write at fold time, priced after M9c |
| `upsert_condition` / `patch_condition` / `delete_condition` | none | Deferred with them |
| `distance_metric`: `cosine_distance`, `euclidean_squared` | **dot product only**, not selectable | **M9d**, recorded in HEAD's schema like `dims` |
| `schema` inline in a write | inferred at first fold, immutable (M7d) | M9a accepts types by inference; declared schemas with M9h |
| `copy_from_namespace`, `branch_from_namespace` | none | Deferred: branching is one PUT by design ([mutations-and-mvcc.md](../05-storage-engine/mutations-and-mvcc.md)); no caller yet |
| ids: string ≤ 64 bytes, u64, UUID (*snippet*) | any string | M9c bounds it, because the id becomes a lookup key |
| base64 vectors | JSON arrays only | D-36 wants it; small, with M9d |
| durable on return; group commit ~1 s | `durable` or `batched` (process memory until a later durable write) | Kept: `durability` is a client choice by design (D34) |
| `encryption` (CMEK), `sharding`, `disable_backpressure`, 429 | none | Declined for now: BYOC owns the bucket (M7g); quotas are M6b's crate, not yet wired |

## Query — `POST /v2/namespaces/{ns}/query` vs `POST /v1/indexes/{index}/query`

| turbopuffer | pstore today | Gap → milestone |
|---|---|---|
| `include_attributes: bool \| [names]`, `exclude_attributes` | `{id, score}` only | **M9a** — at **zero extra requests**: the blocks that resolve ids already carry attributes |
| `filters`: `Eq NotEq In NotIn Lt Lte Gt Gte And Or Not` | none over HTTP (the engine has `Eq`, `Gt`, `Lt` on an exact-scan path) | **M9b** — prefiltered clustered ANN is the architecture's advantage ([`filtering.md`](../06-indexing/filtering.md)) |
| `Contains`, `ContainsAny`, `Any*` (arrays), `Glob`, `IGlob`, `Regex`, `Fuzzy` | none | Arrays with M9h; glob/regex need the trigram index (refused today as `Unimplemented`) — deferred |
| `ContainsAllTokens`, `ContainsAnyToken`, `ContainsTokenSequence` | none | Deferred with FTS options |
| `rank_by: [attr, "ANN", vec]`, `"kNN"` (exact) | ANN only; exact only below 25,000 rows | **M9d** exposes `kNN` (the `Rerank::Exact` rung already exists) |
| `rank_by: [attr, "BM25", q]`, `Sum`, `Max`, `Product`, `Saturate`, `Decay` | one BM25 field, RRF-fused with the vector leg | **M9g** |
| `rank_by: [attr, "asc"\|"desc"]`, `offset`, paging by `id` | none | **M9e** — and it is the export path, as turbopuffer made it |
| `aggregate_by: Count, Sum`, `group_by` | none | Deferred: needs M9b's filter evaluation first |
| multi-query (≤ 16), `rerank_by: ["RRF", {rank_constant, weights}]` | two legs, RRF `k = 60` fixed; `score` is the RRF value | **M9g** |
| `consistency: strong \| eventual` (strong default) | other processes see a write only after an operator `fold` | **M9i** — a scheduled fold; `session` tokens per [session-and-affinity-protocol.md](session-and-affinity-protocol.md) |
| `$dist` per row; `billing`, `performance` | `score`; `meta.cost` in blob requests and bytes | Ours is already more honest about cost (D34); `$dist` with M9d |
| `compute_attributes` (`Highlight`, `Embed`, `VectorDist`), `explain_query` | none | Declined for now: embedding is a model-hosting product decision |
| — | `as_of`: **pstore has it** (M7e, by epoch) | No time-travel parameter in SDK 2.10.2's query params |

## Schema

| turbopuffer | pstore today | Gap → milestone |
|---|---|---|
| `string, int, uint, float, uuid, datetime, bool`, arrays of each, `[N]f16`, `[N]f32`, `{}f16` (sparse), `[][N]f32` | `Int(i64)`, `Str` in the engine; one dense `f32` field and `text` over HTTP | M9a: `int`, `string`. **M9h**: `float`, `bool`, `datetime`, arrays |
| `full_text_search`: tokenizer versions, 18 languages, stemming, stopwords, case, ASCII folding, `k1`, `b` | one analyzer: split on non-alphanumerics, lowercase; `k1 = 1.2`, `b = 0.75` | Deferred: an analyzer is part of the segment format, and changing one is a reindex |
| `GET`/`POST /v1/namespaces/{ns}/schema` | `PATCH …/schema` refuses by design (M7d) | Kept: a schema change is a new index (M7d's argument) |

## Namespaces

| turbopuffer | pstore today | Gap → milestone |
|---|---|---|
| `GET /v1/namespaces?prefix&cursor&page_size` | `GET /v1/indexes`, unpaginated | **M9f** |
| `DELETE /v2/namespaces/{ns}` | none, and the engine has no delete-index path | **M9f** |
| `GET …/metadata`: `approx_row_count`, bytes, `created_at`, index status | `GET /v1/indexes/{index}`: segments, documents, epoch, `unfolded`, schema | **M9f** adds the missing fields |
| `hint_cache_warm` | none | Deferred: needs the NVMe tier (D-23) |
| `_debug/recall` | `scripts/recall.sh`, offline | Deferred: ours is a CI gate, not an endpoint |
| pinning, read-only | none | Declined: a replica provisioned for one namespace is a node holding that namespace, against AGENTS.md's "nodes own nothing" |

## The M9 plan, in order

The order is by dependency, then by what a client notices first:

| # | Milestone | Why here |
|---|---|---|
| **M9a** | **Attributes**: `attributes` on write (`int`, `string`), `include_attributes` on query | Everything after it filters, orders or overwrites *attributes*. Costs no request. |
| M9b | **Filters**: `Eq NotEq In NotIn Lt Lte Gt Gte And Or Not` on the ANN and BM25 paths | The architecture's claimed advantage, untested through the API |
| M9c | **Upsert and delete by id**: last write wins, `deletes` | Needs filters' row-exclusion machinery; delete vectors AND with filter bitmaps |
| M9d | **Distance metrics and exact kNN**: `cosine_distance`, `euclidean_squared`, `kNN`, `$dist`, base64 vectors | A schema field like `dims`; small |
| M9e | **Order by attribute and paging**: `[attr, asc\|desc]`, `offset`, id-cursor export | Needs M9b's evaluation |
| M9f | **Index lifecycle**: delete, paginated list, metadata | The engine has no delete-index path; HEAD must learn one |
| M9g | **Ranking composition**: multi-query, RRF weights, BM25 `Sum`/`Max`/`Product` | The `prefetch[]` machinery exists (M5b); only the API is missing |
| M9h | **Types**: `float`, `bool`, `datetime`, arrays; `Contains`/`ContainsAny` | The block encoding gains tags; zone maps gain types |
| M9i | **Visibility**: a scheduled fold, `consistency` | The largest behavioural gap, and the one the session protocol exists for |

**Not planned, with the reason in the tables above:** patches and conditional writes (after
M9c), copy/branch, aggregations, FTS analyzer options, glob/regex/fuzzy, highlighting and
embedding, warm hints, CMEK, sharding, pinning.
