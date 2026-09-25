# Public API Design

**Synthesizes:** D34
**Status:** v1 proposal

## Principles

1. **The index is the noun.** `/v1/indexes/{index_id}/…`. Implicit creation on first write
   (turbopuffer does this, and it is right — it removes a provisioning step from every
   integration).
2. **Every tradeoff is client-visible**: consistency, recall, and completeness are request
   parameters, not hidden policy. This is the through-line of the whole design.
3. **Every response is self-describing about what it cost and how fresh it is.** If we are
   going to have a cold path, we must be honest about when the client is on it.
4. **Batch-first.** The API should make the cheap thing (large batches) the natural thing.

## Surface

```
PUT    /v1/indexes/{id}/documents        upsert a batch
POST   /v1/indexes/{id}/delete           delete by id or by filter
POST   /v1/indexes/{id}/query            search
POST   /v1/indexes/{id}/multi_query      several queries, one round trip
GET    /v1/indexes/{id}                  metadata, schema, stats
PATCH  /v1/indexes/{id}/schema           schema evolution
DELETE /v1/indexes/{id}                  tombstone + async reap
POST   /v1/indexes/{id}/branch           copy-on-write fork  (1 PUT internally)
POST   /v1/indexes/{id}/warm             pre-heat cache
GET    /v1/indexes                       enumerate (paginated, catalog-backed)
```

## Write

```jsonc
PUT /v1/indexes/acme_docs/documents
{
  "durability": "durable",          // durable | batched | async
  "idempotency_key": "batch-8f3a",  // dedup on retry → effectively-once
  "documents": [
    { "id": "doc-1",
      "vector": [0.1, ...],          // or "vectors": {"title": [...], "body": [...]}
      "attributes": { "tenant": "t1", "created_at": "2026-09-05T...", "text": "..." } }
  ]
}
→ 200 { "epoch": 41822, "read_token": "…", "documents_written": 10000,
        "cost": { "blob_writes": 1, "blob_reads": 0 } }
```

⚠️ **M9a:** `attributes` shipped as written here, `text` included. The query side speaks
turbopuffer's names instead — `include_attributes` (`true`, `false` or names) and
`exclude_attributes` — because M9 is parity with it ([turbopuffer-api-parity.md](turbopuffer-api-parity.md)).

`read_token` is the handle for read-your-writes; `cost` makes request amplification visible to
the caller, which is both honest and a differentiator.

## Query

```jsonc
POST /v1/indexes/acme_docs/query
{
  "consistency": { "mode": "bounded", "max_staleness_ms": 5000 },
                                     // strong | bounded | at_token
  "filter": ["And", [["tenant","Eq","t1"], ["created_at","Gt","2026-01-01"]]],
  "vector": { "query": [0.1, ...], "field": "body" },
  "text":   { "query": "quarterly revenue", "field": "text" },
  "fusion": { "method": "rrf", "k": 60 },
  "rerank": "fast",                  // none | fast | exact
  "top_k": 20,
  "include": ["id", "score", "attributes.title"],
  "deadline_ms": 200
}
→ 200 {
  "results": [ { "id": "...", "score": 0.83, "scores": {"vector": .., "text": ..} } ],
  "meta": {
    "epoch": 41822, "staleness_ms": 1203,
    "partial": false, "shards": {"queried": 16, "completed": 16},
    "cache": "warm", "round_trips": 0,
    "cost": { "blob_reads": 0, "bytes_scanned": 12400000 }
  }
}
```

Note `meta.partial` and `meta.shards`: a partial result is **never** returned silently
(`08-query-engine/query-path.md`).

## Protocol

| Layer | Choice |
|---|---|
| **HTTP/1.1 + JSON** | Default. Universal, debuggable, good enough for most. |
| **HTTP/2 + a binary body codec** | For high-throughput ingest. Vectors as raw little-endian f32/f16 buffers, not JSON arrays — JSON encoding of a 768-dim vector is ~10 KB vs. 3 KB raw and costs real CPU on both sides. |
| **gRPC** | Optional, for typed clients. |
| **Arrow Flight** | Worth considering for bulk ingest/export — zero-copy columnar, and we are Arrow-native internally. |

> **D-36.** Ship JSON first for adoption, but design the wire format so **vectors can always
> be sent as raw binary buffers** (base64 in JSON, raw in the binary codec). Vector
> serialization is a top-three CPU cost in naive implementations of this API.

## Schema

Types inferred by default, declarable explicitly (uuid, datetime, and other
non-inferrable types need declaration — turbopuffer hit the same issue). Per-attribute:
`indexed` (filter/sort), `bm25` (full-text with an analyzer), `trigram` (regex/glob).
All vector dimensions in a field must match.

Schema changes that require reindexing (analyzer change, dimension change) must **say so** in
the response and offer the branch-based migration path
(`06-indexing/incremental-maintenance.md`).

## Errors

Structured, actionable, and honest about which are retryable:

| Code | Meaning |
|---|---|
| `429 rate_limited` + `Retry-After` | Tenant quota, or WAL backpressure |
| `409 version_conflict` | Optimistic write with a stale client `version` |
| `503 storage_unavailable` | Blob store degraded; `durable` writes fail closed |
| `400 schema_conflict` | Type mismatch, dimension mismatch |
| `504 deadline_exceeded` | With `partial` results attached where possible |

## Multi-tenancy in the API

Two supported patterns, and we should be opinionated about which to use:
1. **Index per tenant** (recommended): perfect isolation, idle cost ≈ storage, no filter cost,
   trivially deletable. **This is what the architecture is for.**
2. **Shared index + `tenant_id` filter**: fewer indexes, but every query pays filtering cost.
   Only sensible for cross-tenant search.

Most "filtered vector search is slow" complaints in this industry are pattern 2 used where
pattern 1 belonged. Say so in the documentation.

## Open questions raised

- OQ-81: Arrow Flight for bulk ingest — worth the surface area in v1?
- OQ-82: Should epochs be public (real time travel: `as_of`) or internal? Leaning public.
- OQ-83: Streaming query responses (first results before all shards complete) — high value for
  RAG UX, adds protocol complexity.
