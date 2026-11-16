# M270: the turbopuffer compatibility facade, part 3: explain, warm, recall, schema and metadata updates

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): part 3 of
the facade [M268](../M268/SPEC.md) began and [M269](../M269/SPEC.md) extended. The pinned
client is still the vendor's MIT Python SDK, `turbopuffer` 3.1.0
(`resources/namespaces.py`, `types/namespace_explain_query_response.py`,
`namespace_hint_cache_warm_response.py`, `namespace_recall_params.py`,
`namespace_recall_response.py`, `namespace_update_schema_params.py`,
`namespace_update_metadata_params.py`, `namespace_metadata.py`). Part 4 takes copy and branch
with their operations, and the compatibility manifest.

## Delta
M268's rules hold: identity from the credential, the vendor's error envelope, and a permanent
409 answered 400. Every new route is **one native call**, except `update_metadata` (item 5),
which is two.

1. **`explain_query`** (`POST /v2/namespaces/{ns}/explain_query`). The body is translated as
   M268's Delta 4 translates a query, and sent as the native query with `"explain": true`
   (M165). The answer is `{"plan_text"}`: the native `meta.explain`, as indented JSON text.
   It is `null` when the native query has no plan, as for an unranked query. A body holding
   `queries` is refused, naming it.
2. **`hint_cache_warm`** (`GET /v1/namespaces/{ns}/hint_cache_warm`). This is the native warm
   (`POST /v1/indexes/{index}/warm`, M21). The answer is `{"status": "ACCEPTED", "message"}`,
   and the message names the segments warmed. The native warm finishes before it answers,
   so the warm has run when the SDK gets `ACCEPTED`.
3. **`recall`** (`POST /v1/namespaces/{ns}/_debug/recall`). This is the native recall (M162):
   - `num`, `top_k`, `filters` and `include_ground_truth` pass through. `filters` is
     translated as a query's filters are.
   - `rank_by` is refused, naming it: the native recall samples its own query vectors.
   - The answer is `avg_recall`, `avg_ann_count`, `avg_exhaustive_count` and, when asked,
     `ground_truth`. Each neighbor's `dist` is answered as `$dist`. A `null` `avg_recall`
     stays `null`: no recall is invented for a sample whose exhaustive searches found
     nothing.
4. **`update_schema`** (`POST /v1/namespaces/{ns}/schema`). This is the native schema patch
   (`PATCH /v1/indexes/{index}/schema`), which pstore refuses: `schema_immutable`, a 400.
   Segments already written cannot be reinterpreted. The SDK gets `BadRequestError` naming
   `schema_immutable`, with no request resent.
5. **`update_metadata`** (`PATCH /v1/namespaces/{ns}/metadata`).
   - `read_only` is the native `PATCH /v1/indexes/{index}` (M149).
   - The answer is the namespace's metadata, as `metadata()` answers it. That takes the
     native summary, a second native call, because the native patch answers only the flag.
     A body without `read_only` makes only the summary call.
   - `pinning` that is absent, `null` or `false` asks for nothing, since pstore pins
     nothing. `pinning: true` or an object is refused, naming it.
6. **Metadata's size.** `approx_logical_bytes` is the native summary's (M148), and 0 only
   when the native one is `null`. M268 answered 0 always.

**No setting.** Only facade translations change: nothing a native request sends or receives
changes.

## Acceptance criteria
1. **The SDK, against pstore** (`./scripts/facade.sh`; Python test names carry no code
   marks):
   - explain_query on an ANN query answers plan_text, which names the dense leg;
   - hint_cache_warm answers status ACCEPTED;
   - recall with num 2 and top_k 2 answers an avg_recall from 0 to 1, and with
     include_ground_truth, two ground truths whose neighbors carry `$dist`;
   - recall with rank_by is refused BadRequestError, naming it;
   - update_schema is refused BadRequestError, naming schema_immutable;
   - update_metadata(read_only=True) answers metadata, after which a write is refused
     BadRequestError (`index_read_only`, a permanent 409); update_metadata(read_only=False)
     lets writes in again;
   - update_metadata(pinning=True) is refused BadRequestError, naming pinning;
   - metadata's approx_logical_bytes is above 0 once a fold has written a segment.
2. **Translation, in process** (`crates/pstore-server/tests/it/facade.rs`):
   - `facade_explain_warm_and_recall_are_native_calls`: items 1 to 3, each answer compared
     to the native call's on a twin index;
   - `facade_schema_and_metadata_updates_are_native_calls`: items 4 to 6 against the native
     answers, including `read_only` set and cleared and the refusals;
   - `facade_adds_no_request` extended: explain, warm, recall and update_schema each cost
     their native call's requests, and update_metadata costs the native patch's and the
     summary's together.
3. **Gates:** fmt, clippy, the workspace tests, `scripts/facade.sh`, and the named mutations
   observed red.

## Test plan
AC1 and AC2 are red against M269, which has none of these routes and answers
`approx_logical_bytes` 0.

| AC | Mutation |
|---|---|
| 2 | `explain` not set on the native query; `plan_text` taken from something other than `meta.explain`; `rank_by` not refused; `dist` not renamed `$dist`; `read_only` not passed to the native patch; `pinning: true` accepted; `approx_logical_bytes` answered 0 |

## RA budget
None. Each route is the native call it names, and `update_metadata`'s second call is the
native summary, which reads HEAD and adds no data-dependent fetch. `facade_adds_no_request`
holds both.

## Risks
- **`hint_cache_warm` waits for the warm.** The vendor's warm is a hint answered at once. The
  native warm fetches segment metadata before it answers, as M21 measured. An SDK caller who
  fires it and moves on waits that long, and gets nothing it did not ask for.
- **`update_schema` is always refused.** pstore's schema is fixed by the first fold. A caller
  that adds an attribute by schema update gets a 400 rather than a silent no-op. A write that
  carries the attribute declares it.

## Tasks
- **M270.1** The spec.
- **M270.2** `facade.rs`, the routes, the in-process tests, the SDK tests, docs.
