# What pstore still lacks against turbopuffer

**Snapshot:** 2026-11-15. **pstore:** `1be38fb` (main, M235). **Vendor:** the public
docs (changelog last updated 2026-11-14), the `openapi.yml` on the `main` branch of
[turbopuffer-openapi](https://github.com/turbopuffer/turbopuffer-openapi) fetched
2026-11-15 (sha256 `1db3a23e…9e94`), and the official Python SDK `turbopuffer` 3.1.0
from PyPI (MIT, wheel sha256 `f5e44b54…4389`).

This is a gap list, not a full comparison. The [2026-10-25 comparison](turbopuffer-2026-10-25.md)
inventories every vendor field, and [DELIVERY.md](DELIVERY.md) tracks what has closed since.
This document answers one question: **after M235, which turbopuffer features does pstore
still lack, and which are worth building?** It also covers what the vendor shipped after the
pinned spec (2026-10-16), and whether the compatibility facade is worth building.

**How it was checked.** pstore status comes from `api/openapi.json`, the server's parsers
(`crates/pstore-server/src/lib.rs`), the generated clients under `clients/`, and the
milestone files, by reading and grepping. No suite was run and no vendor request was
made for this document. A row marked *missing* means no such field, parser arm or route
was found.

## Summary

- **The query and write surface is nearly at parity.** Of the vendor's write and query request fields,
  only `compute_attributes`, `k3`, `disable_backpressure`, `sharding`, `encryption` and
  schema `embed`/`filterable` have no pstore counterpart. The first two are cheap and
  worth building.
- **The vendor shipped eight API-visible features since the pinned spec.** pstore has
  `max_staleness` and `Avg`/`Min`/`Max` (numeric only), and empty creation without a
  schema. It lacks CJK tokenizers, fuzzy highlighting, KMS encryption context, model
  reranking and a Rust client.
- **The biggest difference is the client, not the server.** The vendor SDK retries,
  paginates, runs async and reports timings. The generated pstore clients are thin
  typed wrappers with none of that.
- **Facade: worth building, as a thin and separate layer, after the small native gaps
  below.** A vendor account is *not* needed to start: the SDK is on PyPI under MIT and
  takes `TURBOPUFFER_BASE_URL`, so it can run against a local pstore. An account is needed
  only to prove semantic equivalence against the real service.

## Effort scale

| Effort | Meaning |
|---|---|
| S | One milestone or less, no format change |
| M | One or two milestones, or a new dependency to measure |
| L | Three or more milestones, or needs infrastructure pstore does not have |

## Shipped by the vendor after the pinned spec

From the changelog's October and November 2026 entries, and the spec diff against the
pinned commit `87f0f282`. The diff touches three things only: the consistency object,
`encryption_context`, and the `Avg`/`Min`/`Max` aggregates.

| Feature | turbopuffer | pstore | Effort | Recommend |
|---|---|---|---|---|
| `max_staleness_seconds` on eventual reads | 1 to 3,600 s, default 60; the cached view is reused that long | `max_staleness_ms` on the query, `staleness_ms` in `meta` | none | Have it. The facade maps seconds to ms |
| `Avg`, `Min`, `Max` in `aggregate_by` | `Min`/`Max` take any scalar but `bytes` (string, bool, uuid, datetime too); `Avg` of int/uint/float | All three, grouped and ungrouped ([M211](../milestones/M211/SPEC.md)); numeric attributes only | S | **Build** `Min`/`Max` over strings, uuids and datetimes |
| CJK tokenizers `smartcn`, `kuromoji`, `nori`, and `whitespace` | Per-field `tokenizer` | `word_v0` to `word_v4` and `pre_tokenized_array` only (`pstore-format/src/text.rs`) | `whitespace` S; CJK M | **Build `whitespace`** now. **Build CJK** only after measuring the dictionary's binary and memory cost: the Japanese and Korean dictionaries are tens of MB. A tokenizer is already a per-field choice, so no new setting is needed |
| Fuzzy highlighting | `Highlight` matches a `Fuzzy` leg's typos inside a fragment | Highlights ([M159](../milestones/M159/VERIFIED.md)) mark analyzed terms; no fuzzy match marking was found | S | Build, low priority |
| `create_namespace: true` with a declared schema | An empty namespace whose `schema` declares at least `id` | `PUT /v1/indexes/{index}` takes `dims` and `distance_metric` only ([M147](../milestones/M147/VERIFIED.md)); attribute types are recorded on first write ([M173](../milestones/M173/VERIFIED.md)) | S | **Build**: accept `schema` on empty creation. The facade needs it too |
| `encryption_context` on AWS KMS keys | Per-namespace CMEK | No KMS | L | Skip: workstream I |
| Native reranking GA | `rerank_by` with a model | None | L | Skip: workstream J needs a model provider |
| Rust client (beta) | Generated SDK | None; the plan waits for the wire to settle | S | Skip until the facade decision. The OpenAPI contract can generate it |

## Older gaps still open

Features the pinned spec already had, still absent or partial on main.

| Feature | turbopuffer | pstore | Effort | Recommend |
|---|---|---|---|---|
| `compute_attributes` | Up to 256 named expressions evaluated on each result: `VectorDist`, any BM25 or FTS clause, `Highlight`. They never change ranking | `$dist` and the `highlight` field only; no general map | M | **Build.** It is the documented way to feed a reranker per-clause scores. M182 and M203 already evaluate these expressions. The SPEC must show that `VectorDist` on returned rows fits the existing fetch round, as vector projection does ([M179](../milestones/M179/VERIFIED.md)) |
| `rank_fragments_by` in `Highlight` | Orders fragments by a `rank_by` expression over `$fragment` | Refused ([M159](../milestones/M159/VERIFIED.md)) | S | Build with `compute_attributes`, which uses the same expression machinery |
| BM25 `k3` | Query-term frequency saturation, default 8.0 | `k1` and `b` only | S | **Build** as a per-query parameter. When it is absent, scores stay exactly as they are, so existing tests pin the old mode |
| `disable_backpressure` | Lifts the 429 above 2 GB or 1M unindexed rows | pstore has no backlog bound to lift. It bounds buffered rows and memory ([M141](../milestones/M141/VERIFIED.md), [M213](../milestones/M213/VERIFIED.md)), not unfolded backlog | none | Skip natively. The facade accepts it as a no-op |
| `filterable: false` per attribute | Skips the filter index for that attribute; cheaper storage | `equality_filters` is a per-index storage setting (M230 to M235), not per attribute | M | Skip until a measurement shows the per-attribute saving matters |
| `glob`, `fuzzy`, `regex` schema toggles | Each pattern filter needs its toggle on | `Glob`/`IGlob`/`Fuzzy` work with no toggle; `regex` is a declared set | none | Skip. pstore is more permissive. The facade accepts the toggles |
| `ann` and `sparse_knn` per field | Declared per vector field | ANN is built by rule ([M198](../milestones/M198/VERIFIED.md)), by decision | none | Skip, by decision |
| In-place schema update, attribute `drop` | `update_schema`, `{"drop": true}` | `PATCH .../schema` is refused; changes go through a reindex copy with a support matrix ([M168](../milestones/M168/VERIFIED.md)) | none | Skip, by design. The facade refuses changes that need a reindex, naming the copy route |
| `Prefer: respond-async` and `operations/{token}` | Async copy and recall | A copy is asynchronous natively with its own status route ([M166](../milestones/M166/VERIFIED.md)); recall is synchronous | S | Facade only |
| Recall `rank_by` | Recall of a chosen ranking (for example a named or multi-vector field) | `RecallRequest` has `num`, `top_k`, `filters`, `seed`, `include_ground_truth`; default vector field only | S | Build, low priority: named fields ([M190](../milestones/M190/VERIFIED.md)) cannot be checked online today |
| `performance` per query and write | `server_total_ms`, `query_execution_ms`, `cache_hit_ratio`, `cache_temperature`, `exhaustive_search_count`, `approx_namespace_size` | `meta.cost` counts blob requests and bytes; `explain` reports the plan ([M165](../milestones/M165/VERIFIED.md)); no timings or cache ratio | S | **Build** timings and the cache hit ratio natively. They are cheap, and they are what an operator reads first |
| `billing` per query and write | Billable logical bytes queried and returned | Physical blob counts only | none | Skip natively: pstore has no price list. The facade fills it from logical bytes and documents that |
| `last_write_at` in metadata | Last write time | `created_at` and `updated_at` ([M148](../milestones/M148/VERIFIED.md)) | S | Facade maps `updated_at`; skip natively |
| Sharding (`num_shards`, up to 256) | A namespace past 500M rows or 1 TB needs it | No shard count: segments are served by any node. How large one index can get is unmeasured | L | Skip the knob. Measure one index's ceiling under workstream I |
| Pinning, CMEK, audit logs, private networking, IP allowlist | Commercial service features | None | L | Skip: workstream I |
| Native embedding (`embed`) | Hosted models | None | L | Skip: workstream J |
| Narrower f16/i8 storage | `[N]f16`, `[N]i8` stored narrow | Declared, rounded and scored, stored as f32 ([M195](../milestones/M195/VERIFIED.md)) | M | Waiting on the owner's M195 decision |
| Request and response compression | Standard `Content-Encoding`; the official clients turn it off by default | A body with any `Content-Encoding` other than `identity` is refused (`ingress.rs`) | S | Skip. The vendor's own clients do not compress |

## Client SDK features

The vendor's Python SDK 3.1.0, read from its wheel, against the generated
`clients/python` and `clients/typescript`.

| Feature | turbopuffer SDK | pstore clients | Effort | Recommend |
|---|---|---|---|---|
| Automatic retry | 4 retries with backoff on 408, 409, 429 and 5xx | `PstoreError.retryable` and `retry_after` are exposed; the caller retries | S | **Build** in the generator: retry only what the server marks retryable, and honour `Retry-After` |
| Timeouts | 60 s, connect 5 s | Python: 30 s, one value | none | Have it |
| Async client | `AsyncTurbopuffer` | Sync Python only; TypeScript is async by nature | S | Build, low priority |
| Auto-paginating list | Iterates `next_cursor` | One page per call | S | **Build** in the generator |
| Client timings and telemetry | `ClientPerformance`, an observer hook, sampled telemetry | None | S | Skip telemetry. Add an observer hook only if a user asks |
| Base64 vector helpers, `exists()` | Present | Server supports base64 ([M179](../milestones/M179/VERIFIED.md)); no helper | S | Build with the retry work |

## Should we build the compatibility facade?

**Yes, as a separate layer, and after the native S rows marked "Build" above.** The
native API now covers almost every vendor capability. What remains is mostly wire shape,
which is exactly what a facade is for. Shapes alone are not enough, though. The facade
changes defaults, and three of its rules are semantic.

**What it would need.**

1. **Routes.** The 13 method/path pairs of the vendor spec: `/v2/namespaces/{ns}` (write,
   branch, copy), `/v2/.../query` (single and multi), `/v2/.../explain_query`, `/v1|v2/.../metadata`,
   `/v1/.../schema`, `/v1/.../hint_cache_warm`, `/v1/.../_debug/recall`, `/v2/.../async` and
   `/v1/.../operations/{token}`, and `/v1/namespaces`. Namespace maps to index.
2. **Identity.** A bearer API key resolved to a tenant with the M127 verifier. The tenant
   header is never accepted on facade routes.
3. **Defaults.** Strong reads and durable writes, as the vendor guarantees. These are
   facade defaults, not a change to native defaults, so native tests stay unchanged.
4. **Envelopes.** `rows` rather than `results`, `{"status": "OK"}` successes,
   `{"status": "error", "error": "…"}` failures, `namespaces: [{id}]` lists,
   `limit: {total, per}` mapped to `top_k` and `limit_per`, `rows_affected` and
   `*_ids` from M129 and M130, and `billing` and `performance` (above).
5. **Status codes.** The SDK retries 409 four times. pstore answers 409 for permanent
   refusals (`index_exists`, `index_read_only`, `index_limit`), so the facade must remap
   those, or every refusal costs five requests. A retried non-idempotent write (a patch by
   filter or a conditional write after a 5xx) can apply twice. M132 idempotency keys
   prevent that natively, but the SDK sends none, so the facade must say which writes
   are safe to retry.
6. **Refusals.** Anything pstore cannot honour (`encryption`, `sharding`, `embed`, model
   `rerank_by`, in-place schema changes) is refused typed, never ignored. Pure hints with
   no pstore meaning (`disable_backpressure`, the `glob`/`fuzzy` toggles) are accepted
   and listed in a compatibility manifest.
7. **Fixtures.** The SDK pinned by version and wheel hash. Run its own examples (from the
   docs' code blocks) against a local pstore through `TURBOPUFFER_BASE_URL`, in CI, on
   the in-memory store, as `scripts/clients.sh` does for the native clients.

**What an account adds.** Without one, the fixtures prove that the SDK works against
pstore. They do not prove that pstore answers what turbopuffer would. Recording the
vendor's answers to the same fixtures needs a service account, as the
[delivery plan](turbopuffer-next-plan.md) says. That evidence can come later. It does
not block building and testing the facade.

**Estimated cost.** About four milestones: routes and identity; envelopes and status
codes; async and metadata; fixtures and the manifest. The facade adds no blob request
and no round trip, because each facade call becomes one native call.

## Suggested order

Small native gaps first, so that the facade maps onto features rather than refusing them:

1. `k3`, `whitespace` tokenizer, `schema` on empty creation, `Min`/`Max` over non-numeric
   scalars (S each).
2. `compute_attributes` with `rank_fragments_by` (M).
3. Per-request `performance` timings and cache hit ratio (S).
4. Client generator: retries, pagination, base64 helpers (S).
5. The facade (about four milestones), then vendor-recorded fixtures once an account
   exists.
6. CJK tokenizers, fuzzy highlighting and recall `rank_by` when a user needs them.

Not planned without the owner's decision: workstreams I and J, sharding, `filterable`
per attribute, and narrower f16/i8 storage (M195).
