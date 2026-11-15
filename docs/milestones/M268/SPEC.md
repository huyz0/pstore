# M268: the turbopuffer compatibility facade, part 1: routes, identity, the core data plane

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md), "a separate
compatibility facade where equivalence is proven", and the facade section of the
[2026-11-15 gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md). The pinned client is
the vendor's MIT Python SDK, `turbopuffer` 3.1.0 (wheel sha256
`f5e44b540902f58e8201a62d0df0e98e8fe04987c41efec4bf8a83ea35254389` is pinned in
`scripts/facade.sh`), which honours `TURBOPUFFER_BASE_URL`, so no vendor account is needed.
Part 1 of about four: later parts take writes by filter and condition with affected ids,
`multi_query`, `explain_query`, `schema`, `hint_cache_warm`, `recall`, copy and branch with
their operations, and the compatibility manifest.

## Delta
A facade is **a translation of one request into one native call** and of its answer back:
no blob request and no round trip of its own. It lives in `crates/pstore-server/src/facade.rs`,
on the vendor's paths, beside the native routes, under the same authentication.

1. **Routes** (each under the native route's scope):
   - `GET /v1/namespaces` (`prefix`, `cursor`, `page_size`) → `list_indexes`; answers
     `{"namespaces": [{"id"}], "next_cursor"}`.
   - `POST /v2/namespaces/{ns}` (a write) → `write_documents`.
   - `DELETE /v2/namespaces/{ns}` → `delete_index`; answers `{"status": "OK"}`.
   - `POST /v2/namespaces/{ns}/query` (one query) → `query_index`.
   - `GET /v2/namespaces/{ns}/metadata` → `index_summary`.
   - `GET /v1/namespaces/{ns}/schema` → `index_summary`; answers metadata's `schema` (the
     SDK's `exists()` reads it). Changing a schema is a later part's.
2. **Identity.** A secured server's facade takes the bearer credential native routes take
   (M127): the tenant is the credential's. An unsecured server's facade takes the bearer
   value as the tenant, a decimal (`TURBOPUFFER_API_KEY=101`), so a development server needs
   no token. **`X-Pstore-Tenant` is never read on a facade route**: an unsecured request
   carrying it is refused `400`, and a secured one is held to its credential as native routes
   are.
3. **Writes.** The body's `upsert_rows` (flat rows: `id`, `vector`, and every other key an
   attribute, with `vectors`, `text` and `sparse` the native document fields of those names)
   become `documents`; `upsert_columns`, `patch_rows`, `patch_columns`, `deletes`,
   `distance_metric` and `schema` pass through, and `disable_backpressure` is accepted and
   ignored (pstore has no backlog bound to lift). Every facade write is **durable**. Answers
   `{"status": "OK", "message", "rows_affected", "rows_upserted", "rows_patched",
   "rows_deleted"}` from the native counts. Anything else in the body (`delete_by_filter`,
   `patch_by_filter`, the conditions, `return_affected_ids`, `encryption`, `copy_from_*`,
   `branch_from_*`, ...) is refused `400` naming the key: later parts translate them.
4. **Queries.** `rank_by` `[field, "ANN", vector]` becomes the native `vector` (and `field`
   for a named one), `[field, "kNN", vector]` the same with `exact: true`, and any other
   `rank_by` (an order, a BM25 expression) passes through. `limit` (an integer) or `top_k`
   become `top_k`; `limit: {"total", "per": {"attributes", "limit"}}` becomes `top_k` and
   `limit_per`. `filters`, `include_attributes`, `exclude_attributes`, `aggregate_by`,
   `group_by` and `vector_encoding` pass through. `consistency` is `{"level": "strong"}` (the
   default, as the vendor's) or `{"level": "eventual", "max_staleness_seconds": n}`, which
   becomes `bounded` with `n · 1000` ms. Answers `{"rows": [...], "aggregations",
   "aggregation_groups", "performance"}`: each row its `id`, `vector` when projected, its
   attributes flattened beside them, and `$dist` (the native `$dist`, or the native `score`
   for a ranking with no distance). `performance` is the native M264 block's
   `server_total_ms`, `query_execution_ms` and `cache_hit_ratio` (0 when the server has no
   cache). **`billing`, `cache_temperature`, `approx_namespace_size` and
   `exhaustive_search_count` are not sent**: pstore has no price list or temperature, and the
   plan forbids manufacturing them.
5. **Metadata** answers `{"approx_row_count", "approx_logical_bytes", "created_at",
   "updated_at", "schema", "index": {"status": "up-to-date"} or {"status": "updating"}}` from
   the summary; `approx_logical_bytes` is 0 until a later part measures it, and says so in
   the manifest.
6. **Errors.** A native refusal `{"error": {"code", "message", "retryable"}}` answers
   `{"status": "error", "error": "<code>: <message>"}`. **A 409 that is not `retryable`
   (`index_exists`, `index_read_only`, `index_limit`) answers 400**, because the SDK resends a
   409 four times; every other status is the native one.

**No setting.** New routes only: nothing a native request sends or receives changes. The
settings rule's opt-in case.

**Not changed:** every native route, the OpenAPI contract (the facade is not part of the
native contract), the generated clients.

## Acceptance criteria
1. **The SDK runs against pstore.** `scripts/facade.sh` starts an unsecured server on the
   in-memory store, installs the pinned wheel into a scratch virtual environment (its hash
   checked), and runs `clients/turbopuffer/test_sdk.py` with `TURBOPUFFER_BASE_URL` naming
   the server and telemetry off: a write of 3 rows (`upsert_rows`, a schema with a
   full-text attribute), `rows_affected` 3; an ANN query whose rows and `$dist` equal the
   native query's; a BM25 query; an order by attribute; a filter; `limit` with `per`;
   `namespaces()` listing the namespace (over two pages with `page_size=1` and two
   namespaces); `metadata()` with `approx_row_count` 3; a patch and a delete, then the rows
   the native query sees; `delete_all()`, then `exists()` false.
2. **Translation, in process.** `crates/pstore-server/tests/it/facade.rs`:
   `facade_write_and_query_are_native_calls` (each mapping of Delta 3 and 4 against the
   native call's answer, row by row), `facade_errors_are_the_vendors` (a 404's envelope;
   `index_exists`'s native 409, and a facade write past the index limit answered 400; a 5xx-class code kept; a refused body key named),
   `facade_identity_never_reads_the_tenant_header` (unsecured: the bearer's tenant, a header
   refused; secured: the credential's tenant, another tenant's index unseen),
   `facade_adds_no_request` (with `DepthCounting` and the accounting store, a facade write
   and query cost the same requests and depth as the native ones); and, in `facade.rs`'s own tests, `knn_is_exact_and_ann_is_not`,
   `eventual_is_bounded_in_milliseconds` and `only_a_permanent_conflict_is_answered_400` (Delta 4's translations, which a small corpus answers
   the same either way, and Delta 6's statuses).
3. **Gates:** fmt, clippy, the workspace tests, `scripts/facade.sh`, and the named
   mutations observed red.

## Test plan
AC1 and AC2 are red against the parent, which serves none of these paths.

| AC | Mutation |
|---|---|
| 2 | `$dist` taken from `score` when the native row has a distance; a non-retryable 409 kept 409; the tenant header read on an unsecured facade route; `kNN` without `exact`; `max_staleness_seconds` not multiplied |

## RA budget
None: each facade request is one native call, and AC2's `facade_adds_no_request` holds it.

## Risks
- **Vendor-recorded truth.** Without an account the fixtures prove the SDK works against
  pstore, not that pstore answers what turbopuffer would. Stated in the manifest, later.
- **`$dist` for BM25.** The vendor reports a BM25 score in `$dist`; so does the facade,
  higher-is-better, as the vendor's.

## Tasks
- **M268.1** The spec.
- **M268.2** `facade.rs`, its routes, the in-process tests, `scripts/facade.sh` and the SDK
  test, CI's job, AGENTS.md's Gates row, docs.
