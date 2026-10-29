# pstore and turbopuffer: API and feature comparison

**Snapshot:** 2026-10-25. **pstore:** `f9c50a15d6caf180a80c14238c06a9ddb6090b68` (M125).
**Audience:** engineers choosing the next product capabilities and evaluating migration.

pstore has substantial retrieval and storage functionality, but it is not a drop-in
turbopuffer replacement. The highest-priority gaps are durability and consistency
defaults, mutation semantics, tenant authentication, and a stable public contract.
Adding matching field names before resolving those gaps would make migration unsafe.

Use this report with the [delivery plan](turbopuffer-next-plan.md) and the
[formal inventory](turbopuffer-contract-inventory.json). The inventory enumerates every
operation definition, component schema, property, and union alternative in the pinned
OpenAPI, including inline request and response properties: **89 schemas, 200 property
declarations, and 166 composition alternatives**. This report explains their
meaning and the implementation gaps. The older [API parity document](../research/11-design/turbopuffer-api-parity.md)
is historical evidence, not a current feature list.

## Evidence and interpretation

The vendor contract is the official [OpenAPI at commit 87f0f282](https://github.com/turbopuffer/turbopuffer-openapi/blob/87f0f2821c72c1de4ba05ca0686ef7264874cf47/openapi.yml),
dated 2026-10-16, cross-checked with current public documentation. Its blob SHA is
`381ce0472fb49aa4fd75ef4f70f603760b272988`. The spec is MIT licensed and carries
SDK-generation extensions; its README warns that it can change incompatibly.
`stainless_overload` denotes SDK overloads, not additional HTTP capabilities.

pstore evidence is the current router, parsers, engine entry points, and integration
tests. Test links below identify executable specifications inspected for this comparison;
the Rust suites were **not rerun for this documentation task**. No authenticated
turbopuffer requests, cross-service benchmarks, production load tests, or compliance
audits were performed. Vendor documentation establishes a documented capability, not
independent verification of the service.

Labels apply to capability, not to wire compatibility:

| Label | Meaning |
|---|---|
| Equivalent | Same broad capability exists; payloads, routes, defaults, or guarantees can still differ |
| Partial | A useful subset exists, with consequential limits or semantic differences |
| Library | Engine/index support exists but the public HTTP API does not expose it |
| Missing | No corresponding capability found in the inspected public implementation |
| Operational | Requires deployment infrastructure, service operations, or external assurance |

There are **no exact turbopuffer routes** in the current pstore router. An "Equivalent"
row is therefore not a claim that an existing turbopuffer SDK works against pstore.

### Conflicts and qualifications in vendor sources

| Subject | Evidence conflict or qualification | Consequence |
|---|---|---|
| Metadata read route | OpenAPI uses `GET /v2/.../metadata`; [metadata docs](https://turbopuffer.com/docs/metadata) show `/v1/.../metadata` | Preserve both observations; verify supported aliases against a service account before promising compatibility |
| Attribute types | OpenAPI's `AttributeType` prose omits `bytes`, `[N]i8`, and vector arrays present in [write docs](https://turbopuffer.com/docs/write) | Include these documented types in the gap analysis; schema prose alone is not exhaustive |
| Stopword default | OpenAPI says `remove_stopwords: true`; write docs say `false` | Configure it explicitly in fixtures and migrations; do not infer analyzer equivalence |
| BM25 query-term tuning | [FTS guide](https://turbopuffer.com/docs/fts) describes `k3` with default 8.0; pinned schema does not expose it | Record as documented tuning with an unresolved wire contract; pstore has no public k3 control |
| Eventual staleness | [Query docs](https://turbopuffer.com/docs/query) describe both a 60-second cached view and potentially hour-scale indexing lag | A cache refresh interval is not a maximum data-staleness guarantee |
| Release status | [Roadmap](https://turbopuffer.com/docs/roadmap) calls native reranking private beta in September; [reranking docs](https://turbopuffer.com/docs/reranking) include preview models | Treat model availability and account/region eligibility as conditional; highlighting is also documented beta |
| Future features | Roadmap lists nested attributes, more aggregates, and resharding | Planned features are not counted as currently available |

## The differences that change application behavior

| Contract | turbopuffer | pstore at this snapshot | Migration consequence |
|---|---|---|---|
| Successful write | Documented durable acknowledgement | `durability` defaults to `batched`, held in process memory; `durable` flushes a bundle | Set durable explicitly or provide a facade with durable defaults; measure acknowledgement latency on equal terms |
| Default read | Strong | Eventual; also supports strong, session, bounded, and epoch reads | The same write-then-query application can see different results without any API error |
| Deferred mutations | Conditions and results belong to the documented write transaction; filter mutations have documented two-phase semantics | Patches, conditions, and filter mutations resolve at fold; strong/session can refuse `not_folded` | Matching names do not provide matching completion or isolation semantics |
| Mixed mutation order | Filter delete, filter patch, then other writes, per write documentation | Upserts, patches, explicit deletes, filter patch, filter delete | A row touched by several operations can finish in a different state |
| Affected counts | Actual affected counts and optional affected IDs | Submitted upsert/patch/delete counts, not actual applied counts; filter effects not represented by those counts | Clients cannot infer which conditional actions succeeded from current counters |
| Invalid schema writes | Typed schema and write validation | Some conflicts across folds/writers are acknowledged then rejected/quarantined at fold | Durable bytes do not imply that every submitted row became searchable |
| Multi-query | Documented atomic shared snapshot | One snapshot for legs at one level ([M131](../milestones/M131/VERIFIED.md)), aggregations and facets included ([M164](../milestones/M164/VERIFIED.md)); mixed levels or `as_of` run apart | Count and hits differ only across sub-queries at mixed levels |
| Copy | Physical isolation, cross-region/org source configuration, optional new encryption | `copy_from_namespace` is a branch alias sharing immutable objects | Do not sell it as an isolated backup or encryption boundary |
| Tenant identity | Bearer API key; service resolves authorization | Caller supplies `X-Pstore-Tenant`; no native authentication or quota wiring | An untrusted client must not be allowed to select another tenant through a header |

Sources: [guarantees](https://turbopuffer.com/docs/guarantees), pinned OpenAPI;
pstore [wire types](../../crates/pstore-server/src/types.rs),
[write/read handlers](../../crates/pstore-server/src/lib.rs),
[patch tests](../../crates/pstore-server/tests/it/patch.rs),
[quarantine tests](../../crates/pstore-server/tests/it/quarantine.rs),
[consistency tests](../../crates/pstore-server/tests/it/consistency.rs).

## Every vendor endpoint

The pinned spec contains **16 operation definitions**, representing **13 distinct
method/path pairs** after stripping overload metadata. The three duplicates specialize
write into branch/copy and query into multi-query.

| Vendor method and route | pstore counterpart | Status and consequential difference | Plan |
|---|---|---|---|
| `GET /v1/namespaces` | `GET /v1/indexes` | Equivalent pagination/prefix; response is strings under `indexes`, not objects under `namespaces`; union includes local unfolded indexes | B |
| `GET /v1/namespaces/{namespace}/schema` | Summary's `schema` | Partial: no standalone schema read; not a general typed attribute schema | D |
| `POST /v1/namespaces/{namespace}/schema` | `PATCH /v1/indexes/{index}/schema` | Missing evolution: current PATCH always returns `schema_immutable` | D, G |
| `GET /v2/namespaces/{namespace}/metadata` | `GET /v1/indexes/{index}` | Partial metadata; see route conflict above | G |
| `PATCH /v1/namespaces/{namespace}/metadata` | `PATCH /v1/indexes/{index}` with `read_only` ([M149](../milestones/M149/SPEC.md)) | Partial: read-only only; pinning missing | I |
| `GET /v1/namespaces/{namespace}/hint_cache_warm` | `POST /v1/indexes/{index}/warm` | Partial: synchronous process-local metadata warm, rather than accepted asynchronous hint | H |
| `POST /v1/namespaces/{namespace}/_debug/recall` | Offline `scripts/recall.sh` | Missing online sampled recall/ground truth endpoint | H |
| `POST /v2/namespaces/{namespace}` | `PUT /v1/indexes/{index}/documents` | Partial write contract; separate field matrix below | B, C, D |
| `DELETE /v2/namespaces/{namespace}` | `DELETE /v1/indexes/{index}` | Equivalent logical deletion; retention/GC and response differ | B, G |
| `POST /v2/namespaces/{namespace}/query` | `POST /v1/indexes/{index}/query` | Partial query contract; separate expression matrix below | B, E, F |
| `POST /v2/namespaces/{namespace}/async` | Replication jobs only | Missing one-shot async copy; replication is a different lifecycle | H |
| `GET /v1/namespaces/{namespace}/operations/{token}` | Replication status only | Missing generic token-based copy operation polling | H |
| `POST /v2/namespaces/{namespace}/explain_query` | None | Missing query explanation (`plan_text`) | H |

The spec deliberately omits deprecated `GET /v1/namespaces/{namespace}` export and
`HEAD /v1/namespaces/{namespace}`. Current export is ordered query pagination; schema
and count replace HEAD. A compatibility policy should explicitly support or reject
these deprecated contracts rather than treating their omission as a new feature gap.

Recall requests carry `num`, `top_k`, `filters`, `include_ground_truth`, and `rank_by`.
There is no public pstore request with those sampling/ground-truth controls. The online
diagnostic must be separate from a per-query recall guarantee.

## Write fields and row representation

This table covers all **21 properties of `Write`**, plus nested source and filter-patch
configuration. Formal names and shapes come from the pinned OpenAPI.

| Vendor field | Current pstore capability | Status / next work |
|---|---|---|
| `upsert_rows` | `documents: [{id, vector, vectors?, text?, attributes?}]`; overwrite by string ID | Equivalent operation, different shape; B |
| `upsert_columns` | Columns normalized to `documents` rows (M174) | Delivered; D |
| `patch_rows` | Nested `attributes` instead of flattened row attributes; vectors refused | Partial, deferred; C |
| `patch_columns` | ID column plus attribute columns | Partial, deferred and string IDs; C |
| `deletes` | String ID list, after upserts/patches | Equivalent primitive; ordering/response differ; C |
| `upsert_condition` | Fold-time predicate; missing IDs insert | Partial; no public `$ref_new` expression parser; C |
| `patch_condition` | Fold-time predicate; missing rows ignored | Partial; C |
| `delete_condition` | Fold-time predicate | Partial; C |
| `distance_metric` | Dot product default, cosine distance, squared Euclidean | Partial contract: vendor enum is cosine/squared Euclidean; no implicit metric equivalence; B/D |
| `schema` | Request-scoped datetime conversion; index FTS/regex declarations | Partial typed schema; D/E |
| `branch_from_namespace` | Same-tenant source string, alone in request, folded state | Partial source/visibility/inheritance contract; G |
| `copy_from_namespace` | Same branch operation | Missing isolated copy; H |
| `delete_by_filter` | Deferred predicate; executed after other operations | Partial with different ordering/completion; C |
| `delete_by_filter_allow_partial` | Same name; needs `durability: "applied"`, answers `applied.rows_remaining` ([M137](../milestones/M137/SPEC.md)) | Partial (applied only); C |
| `patch_by_filter` | `{filters, attributes}` instead of `{filters, patch}` | Partial, after explicit deletes, before filter delete; C |
| `patch_by_filter_allow_partial` | Same name; needs `durability: "applied"`, answers `applied.rows_remaining` ([M137](../milestones/M137/SPEC.md)) | Partial (applied only); C |
| `return_affected_ids` | `affected_ids` on applied writes ([M130](../milestones/M130/SPEC.md)), by-filter operations bounded at 10,000 less the named ids ([M139](../milestones/M139/SPEC.md)) | Partial (applied only); C |
| `encryption` | Deployment bucket encryption only | Missing namespace `{mode, key_name}` contract; I |
| `sharding` | Segment distribution, no initial `num_shards` setting | Missing persistent namespace shard contract; I |
| `disable_backpressure` | No compatible ingestion/backlog mode | Missing; A/C |
| `create_namespace` | `PUT /v1/indexes/{index}` with `dims` and `distance_metric` ([M147](../milestones/M147/SPEC.md)); no FTS or typed-attribute declarations at creation | Partial; G/D |
| `source_namespace`, `source_api_key`, `source_region` | Branch strings; replication/external copy use operator-configured named stores | Missing copy source contract; H |
| `dest_encryption` on copy request | None | Missing re-encryption; H/I |

### Types, nulls, and encoding

| Surface | turbopuffer contract | pstore implementation | Assessment |
|---|---|---|---|
| IDs | uint64, UUID, bounded string | String, `uint64` or `uuid` per index (M176); no 64-byte bound on a string id | Partial; D |
| Row layout | ID plus attribute/vector columns on the row | Required default `vector`; attributes nested | Text-only documents are currently refused, including an empty default vector; D |
| Scalar attributes | string, int, uint, float, uuid, datetime, bool, bytes | i64, finite float, string, bool, request-declared RFC3339 datetime | Missing uint64/UUID/bytes types and persistent declarations; D |
| Arrays | Typed scalar arrays; `[]string` can be FTS | Arrays of accepted scalars; text field must be one string | No homogeneous schema contract or array FTS parity; D/E |
| Null | Nullable attributes, unset on mutation | Upsert attribute null refused; patch null unsets | Different write contract; D/C |
| Datetime | Typed normalized time values | Datetime declaration applies per request; omitted declaration stores a string | Inference and comparison can change between writes; D |
| Dense vectors | f32/f16/i8 schema types, array/base64 transport | f32 array/base64 input; internal quantization is an index mechanism | Internal int8 reranking does not establish i8/f16 public schema support; D |
| Multiple dense attributes | Per-attribute ANN configuration | Up to 8 named fields in addition to required default vector; named dense search scans exactly | Partial indexing and wire semantics; D |
| Multi-vector attributes | Vector arrays and late-interaction ANN configuration | Named arrays, exact MaxSim or opt-in FDE, optional exact rerank/probes | Equivalent family with different scoring/configuration contract; D/F |
| Sparse vectors | String dimension names and f16 weights, `SparseKNN` dot product | One public field, `sparse`: string keys (M177) and f16 weights, written in rows or columns and queried as a dot-product leg (M178); not returned in results | One field per document rather than per-attribute schema; D |
| Returned vectors | Vector attribute projection and `vector_encoding` float/base64 | Ordinary native result projection only walks scalar attributes, not stored vector fields; no corresponding response encoding control | Missing public round-trip/export capability; B/D/H |
| Nested JSON objects | Not counted as available; vendor roadmap item | Public scalar parser refuses nested objects | Neither side has established current general nested-attribute parity |

Source: [conversion and parsers](../../crates/pstore-server/src/lib.rs),
[document/vector representation](../../crates/pstore-format/src/lib.rs),
[typed-value tests](../../crates/pstore-server/tests/it/typed_values.rs),
[array tests](../../crates/pstore-server/tests/it/arrays.rs),
[named-field tests](../../crates/pstore-server/tests/it/named_dense.rs).

## Query fields and responses

All **11 `Query` fields**, both `QueryConfig` fields, and the inline single/multi-query
extensions are included here. pstore's `rank_by` is only its attribute-order syntax;
vector and BM25 searches use separate fields.

| Vendor field | pstore equivalent or gap | Plan |
|---|---|---|
| `rank_by` | `vector`, `field`, `exact`, `multi`, `text`, `fusion`, or scalar attribute `rank_by`; no common expression AST | B, E, F |
| `top_k` | Supported, default 10; zero refused | B |
| `offset` | Attribute orders and relevance rankings ([M163](../milestones/M163/VERIFIED.md)), `top_k + offset` at most 10,000; an approximate ranking's pages are slices at their own depth, not a cursor | F |
| `filters` | Many matching operator names; exact gaps below | D, E |
| `include_attributes` | Boolean/list projection, nested result attributes | B |
| `exclude_attributes` | Projection exists | B |
| `aggregate_by` | Count/Sum; unary `Count` missing, current form includes attribute | F |
| `group_by` | Up to 8 attribute names, no named expression or `ForEachUnique` | F |
| `compute_attributes` | No general computation map; separate single-string highlight feature | E, F |
| `distance_metric` | Metric selected on write/index, no per-query wire override | B, D |
| `limit` | No integer/object alias on a single query (`top_k`); `Limit.per` is served as a top-level `limit_per` over a relevance window ([M146](../milestones/M146/VERIFIED.md)) | B |
| `vector_encoding` | No float/base64 response option | B, D |
| `consistency.level` | Different shape: string modes; default eventual; session/bounded are extra | B, C |
| single-query `rerank_by` | Windowed hand-written boosting/MMR, no model configuration `{model,attribute,query}` | F, J |
| multi-query `queries` | 1..16 queries, entire root body; aggregations and facets per sub-query ([M164](../milestones/M164/VERIFIED.md)); no nested multi | F |
| multi-query `rerank_by` | `["RRF", {rank_constant, weights}]` over the queries' answers ([M142](../milestones/M142/VERIFIED.md)); no model reranker | J |
| multi-query `limit`, `offset` | Outer window beside `rerank_by`, `limit` as `n` or `{total}` ([M142](../milestones/M142/VERIFIED.md)) | Refused without `rerank_by` |

Projection also differs: pstore accepts `include_attributes` together with
`exclude_attributes` and applies exclusion to the selected attributes. Vendor query
documentation makes the two mutually exclusive. Unknown projected names also need
contract fixtures, rather than assuming identical validation.

### Every ranking and computation family

| Vendor expression/option | pstore | Difference |
|---|---|---|
| `ANN` / `kNN`, scalar dense vector | Default ANN, `exact` option | Different transport; named dense fields scanned exactly; vendor documents filters for kNN |
| `ANN` / `kNN`, vector array | Exact MaxSim and opt-in FDE | Existing mechanism must be evaluated against vendor late-interaction semantics/quality, not just matching labels |
| ANN/kNN `Embed` expression, optional `model` | None | Model integration; J |
| `SparseKNN` | A query's `sparse` leg, exact dot product over f16 weights (M178) | Spelled as a leg, not a `rank_by`; D |
| `BM25` string | `text` query on default text field | Single-field only |
| `BM25` token array and `last_as_prefix` | A text leg object's `query` may be an array of tokens (M158) and carry `"last_as_prefix": true` (M180) | Equivalent, scored 1.0 once a row. The prefix meets analyzed terms, so under stemming a complete word also expands through its stem; a prefix query is not split across servers |
| `Sum`, `Max` | Text-leg fusion exists | Limited composition, no recursive per-field expression tree; E |
| `Product` in either argument order | Some weighting via fusion/boost | Not an arbitrary nested weighted scoring expression; E |
| Filter as score | Windowed equality boost | Not whole-corpus filter scoring; E |
| `Attribute` | Windowed numeric boost | Not whole-corpus numeric ranking expression; E |
| `Saturate(midpoint, exponent)` | Boost saturation uses pivot | Different parameterization/domain; E |
| `Decay(midpoint, exponent)` | Boost decay uses origin/scale/shape | Different function, not a field rename; E |
| `Dist(expression, origin)` | Limited numeric/geo boost mechanisms | No equivalent general expression; E |
| One attribute `asc`/`desc` | Supported | Ties/type/null behavior require conformance fixtures |
| Up to 8 attribute sort keys | Up to 8 keys as a list of pairs ([M140](../milestones/M140/VERIFIED.md)) | Paged by `offset` only, no cursor; F |
| Outer `RRF(rank_constant, weights)` | Multi-query `rerank_by` ([M142](../milestones/M142/VERIFIED.md)): integer constant, positive weights; also RRF across one query's legs | Weights capped at 1,000,000 |
| Model reranking | None | Optional service integration; J |
| `VectorDist` computed output | Dense `$dist` only for scored dense leg | No arbitrary extra distance expression; F |
| Computed BM25/ranking scores | No computed output map | F |
| `Highlight` | One marked-up fragment string, or, structured ([M159](../milestones/M159/VERIFIED.md)), a ranked list of fragments over a string or `[]string` | On `highlight`, not a computed attribute (no compute map; F) |
| `HighlightConfigParams` | Markers, character limit, `fragment_by`, `fragment_limit` (0 to 16) and `include_offsets` ([M159](../milestones/M159/VERIFIED.md)) | `rank_fragments_by` refused (no published shape); defaults are pstore's (`word`, 1); a sentence ends at `. ` and `。`, not by Unicode segmentation |
| `HighlightMatch` | `$highlights`: `text`, `fragment_range`, `match_ranges`, `array_index` ([M159](../milestones/M159/VERIFIED.md)) | Ranges into the value, not the marked text; `$highlight` is absent when structured |
| `ForEachUnique` grouping | None | F |
| `Count` / `Sum` aggregates | Attribute count and sum | Unary Count missing; distinct/min/max are vendor roadmap, not a current gap |

Highlight offset units in the spec are UTF-8, UTF-16, and codepoints; fragment modes
are none, sentence, paragraph, and word. pstore accepts all of them from
[M159](../milestones/M159/VERIFIED.md); its `max_chars` still counts Unicode scalar
values. MMR chooses diversity by result similarity;
it does not enforce the hard group quota represented by `limit.per`; `limit_per` does
([M146](../milestones/M146/VERIFIED.md)), exactly over a window of at most 1,000.

Sources: [query planning](../../crates/pstore-server/src/lib.rs),
[boost](../../crates/pstore-server/src/boost.rs), [diversify](../../crates/pstore-server/src/diversify.rs),
[highlight](../../crates/pstore-server/src/highlight.rs),
[multi tests](../../crates/pstore-server/tests/it/multi.rs),
[composition tests](../../crates/pstore-server/tests/it/composition.rs),
[FDE tests](../../crates/pstore-server/tests/it/multi_fde.rs).

### Every filter operator

| Vendor operators | pstore support | Compatibility issue |
|---|---|---|
| `Eq`, `NotEq` | Present, null maps to absent/present | Mixed types, arrays, nulls and unknown attributes need conformance fixtures |
| `In`, `NotIn` | Present | Present on `uint64` and `uuid` ids too (M176) |
| `Contains`, `NotContains`, `ContainsAny`, `NotContainsAny` | Present | Same type limitations; do not infer array ordering predicates |
| `Lt`, `Lte`, `Gt`, `Gte` | Present | Scalar semantics only for the compatible subset |
| `AnyLt`, `AnyLte`, `AnyGt`, `AnyGte` | Missing from HTTP parser | E |
| `Glob`, `NotGlob`, `IGlob`, `NotIGlob`, `Regex` | Present; optional trigram sketch | Different schema eligibility; currently at most 16 pattern predicates |
| `Fuzzy` | Different three-element form with `{value,max_edits}` | Vendor four-element substring form uses `{max_edit_distance:[{min_query_chars,distance}],case_sensitive}`; not equivalent |
| `ContainsAllTokens`, `ContainsAnyToken` | String input only | No token-array variant or `last_as_prefix` params |
| `ContainsTokenSequence` | String input only | Token-array variant missing |
| `Not`, `And`, `Or` | Present | Analyzer binding/missing-value behavior must remain correct under negation |
| Conditions' `$ref_new` | No public reference expression | C |

pstore additionally exposes `GeoRadius` and `GeoBox`. These are useful native extensions,
not evidence of vendor API compatibility. The vendor [permissions guide](https://turbopuffer.com/docs/permissions)
requires application-supplied document permission filters; it does not provide native
document-level RBAC. Both products need trusted application enforcement for those filters.

Source: [predicate parser](../../crates/pstore-server/src/lib.rs),
[filter tests](../../crates/pstore-server/tests/it/filters.rs),
[pattern tests](../../crates/pstore-server/tests/it/patterns.rs),
[token tests](../../crates/pstore-server/tests/it/tokens.rs),
[geo tests](../../crates/pstore-server/tests/it/geo.rs).

### Every response and accounting field

| Vendor response family and fields | pstore counterpart / gap |
|---|---|
| `WriteResult.status`, `message` | Different success envelope; pstore has `epoch`, `durable`, `session` |
| `rows_affected`, `rows_upserted`, `rows_patched`, `rows_deleted`, `rows_remaining` | `documents_written/deleted/patched` count submitted operations; no actual/remaining counts |
| `upserted_ids`, `patched_ids`, `deleted_ids` | Missing |
| `WriteResult.billing`, `performance` | `cost`; no vendor-equivalent commercial bill or timing envelope |
| `WriteBilling.billable_logical_bytes_written`, `query` | Blob operations and physical bytes counted; no logical pricing model |
| `WritePerformance.server_total_ms`, `embedding_tokens`, `embedding_ms` | Not returned as this per-write contract |
| `SingleQueryResult.rows` | `results`; row attributes nested rather than flattened |
| `aggregations`, `aggregation_groups` | Present for standalone aggregation; in a multi-query, one entry per sub-query ([M164](../milestones/M164/VERIFIED.md)) |
| `QueryResult.performance`, `billing` | `meta.cost`, epoch, consistency, staleness, session instead |
| `MultiQueryResult.results` | Array of row arrays, rather than sub-result objects; independent `meta.epochs` |
| `MultiQueryResult.performance`, `billing` | Aggregate physical cost in native meta; no matching commercial envelope |
| Row `id`, `vector`, arbitrary attributes, computed attributes, `$dist` | String ID, projections; `$dist` is dense-leg metric estimate, optional `score` is fused score; not overall vendor rank/rerank score |
| `QueryBilling.billable_logical_bytes_queried`, `billable_logical_bytes_returned` | Missing logical billing values |
| `QueryPerformance.cache_hit_ratio`, `cache_temperature`, `server_total_ms`, `query_execution_ms`, `exhaustive_search_count`, `approx_namespace_size`, `embedding_tokens`, `embedding_ms` | No equivalent per-query envelope; metrics/cost counters cover other questions |
| `ErrorResponse.status`, `error` | Native nested `error` has code/message/retryable; not vendor error shape |
| Warm `status`, `message` | Native warm metadata/cost; synchronous semantics |
| Delete `status` | Native epoch/cost response |
| List `namespaces[{id}]`, `next_cursor` | `indexes: [string]`, `next_cursor`, `cost` |
| Recall `avg_recall`, `avg_exhaustive_count`, `avg_ann_count`, `ground_truth[{query_vector,nearest_neighbors}]` | Offline recall output only |
| Explain `plan_text` | Missing |
| Async start `token`; operation `status`, `start_time`, `progress`, `finish_time`, `result` | Missing general copy job contract |
| Operation `result.success` (WriteResult) or `result.error.{status_code,detail}` | Replication status has different fields and lifecycle |

Native cost fields are `blob_reads`, `blob_writes`, `blob_lists`, `bytes_read`, and
`bytes_written`. They are useful physical accounting, not a substitute for logical
billing or a proven dollar-price advantage. Concurrent-request attribution and cache
effects also need a published accounting definition.

## Schema and namespace control

| Vendor schema field | pstore | Plan |
|---|---|---|
| `type` | Limited declarations/inferred index shape; see type matrix | D |
| `filterable` | No per-attribute public on/off contract | D/E |
| `regex` | Trigram sketch declaration exists | Partial; E |
| `glob`, `fuzzy` | Predicates exist, separate schema toggles absent | E |
| `full_text_search` | Default text field only; not arbitrary string/string-array attributes | E |
| `ann.distance_metric` | Index metric, not general per-field schema | D |
| `ann.late_interaction` | Exact/FDE multi-vector query options | Different configuration surface; D/F |
| `sparse_knn.distance_metric` | Dot product only, no declaration (M178) | No metric choice or schema entry; D |
| `embed.{attribute,model,dims}` | No native model integration | J |
| Schema `drop` | PATCH always refuses | G |

All ten `FullTextSearchConfig` properties:

| Setting | pstore coverage |
|---|---|
| `k1`, `b` | Configurable; pstore permits k1 in [0,3], b in [0,1]; differing validation matters |
| `language` | Same 18 language names in the enum; this alone does not prove identical stemming/tokenization |
| `stemming`, `ascii_folding`, `case_sensitive` | Configurable |
| `remove_stopwords` | Configurable only for English; other languages refused when true; vendor supports additional languages with exceptions; default conflict recorded above |
| `max_token_length` | Configurable from [M181](../milestones/M181/VERIFIED.md), 1 to 255 characters; a longer token is dropped from the document and its length, and a query token over it matches nothing. The vendor's default is not published; pstore's is no limit. Refused under `pre_tokenized_array` |
| `tokenizer` | `word_v1` and, from [M158](../milestones/M158/VERIFIED.md), `pre_tokenized_array` (each string of a `[]string` value is one token, kept byte for byte; `language`, stemming, stopwords, folding and `case_sensitive: false` refused; no highlighting). `word_v0` to `word_v4` from [M181](../milestones/M181/VERIFIED.md): pstore's `word_v1` (the default) and `word_v0` keep no emoji, unlike the vendor's `word_v1`; `word_v4` is `word_v3`'s token stream, since the vendor's fixes are not published; both use Unicode 16.0, not 17.0; the vendor's default for a new namespace is `word_v4`. ⚠️ Operating rule: declare `pre_tokenized_array` only once every server runs M158 or later, since an older build reads its `v2` stamp as no stamp and could record the index as `word_v1` |

The vendor FTS guide excludes Arabic, Greek, Romanian, Tamil, and Turkish from its
stopword-removal support. Pre-tokenized arrays have their own case/stemming restrictions
and do not support highlighting. Supporting the same language enum or tokenizer name
does not establish token-stream equivalence.

| Vendor metadata field/control | pstore summary/control | Assessment |
|---|---|---|
| `schema` | dims, metric, text field, FTS, regex fields, named-vector widths | Partial, not typed attributes |
| `approx_row_count` | Alias of folded `documents` count | Partial freshness/count definition |
| `approx_logical_bytes` | Summed segment object lengths ([M148](../milestones/M148/SPEC.md)); deleted rows count until compaction, delete vectors do not | Partial; G |
| `created_at`, `updated_at` | RFC 3339 times from HEAD ([M148](../milestones/M148/SPEC.md)); `updated_at` is fold time and compaction does not move it | Yes |
| `index.status`, `unindexed_bytes`, `unindexed_rows` | `progress` with `?progress=true` ([M160](../milestones/M160/SPEC.md)): durable unfolded operations across every writer, bounded reads, `exact` past 64 bundles | Partial: opt-in, operations rather than rows |
| `encryption.mode`, `key_name` | Bucket deployment configuration only | I |
| `pinning` boolean/config, `replicas`, `pinning.status.{updated_at,ready_replicas,replicas,utilization}` | No namespace capacity reservation | I |
| `sharding.num_shards` | No namespace shard configuration | I |
| `read_only` | `PATCH /v1/indexes/{index}` `{"read_only": bool}` ([M149](../milestones/M149/SPEC.md)); a replica is read-only by its replication | Yes |
| Docs-only `last_write_at`, `branching.parent` | Not in pinned spec; no matching API metadata | Resolve vendor contract, then G |

[Branching](https://turbopuffer.com/docs/branching) and physical copies must remain separate
products. pstore branches share objects and use folded state. Physical backup needs
separate object reachability, retention, restore and source-deletion proofs.
[Pinning](https://turbopuffer.com/docs/pinning) reserves compute/cache capacity; it need not
make a node the durable owner of data. A reservation can fit pstore's architecture if a
node loss only affects availability/cache warmth and CAS still fences publication.

Sources: [schema tests](../../crates/pstore-server/tests/it/schema.rs),
[analyzer implementation](../../crates/pstore-format/src/text.rs),
[analyzer tests](../../crates/pstore-server/tests/it/analyzer.rs),
[branch tests](../../crates/pstore-server/tests/it/branch.rs),
[lifecycle tests](../../crates/pstore-server/tests/it/lifecycle.rs).

## Limits and request behavior

These are vendor documented limits, not measured capacity. A larger nominal limit does
not imply better throughput; matching it requires bounded decoding, admission and query
execution. pstore's absence of a public limit is a contract gap, not unlimited capacity.

| Dimension | turbopuffer documented | pstore evidence / gap |
|---|---|---|
| Write payload | 512 MB | No matching explicit product limit or streaming admission; HTTP framework limits are not this contract |
| Document / attribute | 64 MiB / 8 MiB | No matching public per-document/attribute cap |
| Filterable value | 4 KiB | No matching public type/index eligibility cap |
| ID | 64-byte string, uint64, UUID | String only; no corresponding cap found at HTTP conversion |
| Attribute / namespace name | 128 bytes | Attribute empty/reserved names refused; no matching 128-byte namespace contract |
| Attribute names/namespace | 1024 | No matching public cap found |
| Vector columns | 8 | Required default plus at most 8 named fields; not identical counting |
| Dense dimensions | 10,752 | No matching advertised/validated ceiling |
| Sparse nonzeros | 1024 | 1,024 a document or query, keys of 256 bytes (M178) |
| Embedded attributes | 4 | Native embeddings absent |
| Embedded upsert rows/batch | 256 | Native embeddings absent |
| Full-text query length | 8192 | No matching public text-query cap |
| Multi-query | 16 | 16, but different snapshot and payload semantics |
| Concurrent queries | 16 unpinned | No corresponding tenant query admission policy |
| Filter patch / delete | 50,000 / 5,000,000 affected rows | Deferred operations, no matching partial-progress contract |
| Unindexed backlog | 2 GB with documented backpressure behavior | No compatible indexed-backlog admission mode |
| Results/groups | 10,000 | Ordered `top_k+offset` and groups bounded at 10,000; no equal public relevance-query bound established |
| Computed attrs / highlights | 256 / 1024 fragments | No compute map; one native fragment |
| Namespace shards | Up to 256, fixed at creation | No persistent namespace sharding API |
| Shard size guidance | 1 TB or 500 million docs/shard | No comparable production sizing evidence |
| Namespace capacity | 256 TB / 128 billion documents in the limits table | No comparable measured capacity contract |
| Pinned namespaces | 256 | No capacity reservation API |
| Namespaces / branches / read replicas | No fixed maximum listed; observed examples are vendor reports | No comparable validated fleet-scale claim |
| Native multi-vector | Per-schema configuration | Native pstore fields allow 1..1024 vectors each, names 1..64 bytes |
| Native boost/MMR | Different vendor mechanisms | pstore windows at most 1000; approximate candidate rescoring, not whole-corpus expressions |

Vendor [API overview](https://turbopuffer.com/docs/api-overview): bearer authentication,
JSON, compression, typed HTTP failures, and optional `Prefer: respond-async` for supported
operations. An asynchronous response includes polling information and bounded result
retention. pstore has a different error envelope and retry classification; no compatible
compression/async contract or generated cross-language client suite was found.
Unknown top-level native write/query fields can be ignored by serde, despite strict
parsing of many nested clauses. A typo in an unsupported safety option can therefore
look successful. Workstream A addresses this before a compatibility facade.

Sources: [limits](https://turbopuffer.com/docs/limits), pinned OpenAPI,
[request types](../../crates/pstore-server/src/types.rs), [body parser](../../crates/pstore-server/src/body.rs).

## Every pstore route, including its extra capabilities

The router has **24 method/path operations** across 19 route strings. This includes
administrative/internal routes and one schema operation that only refuses. Listing a
route here does not recommend exposing it to untrusted clients.

| pstore method and path | Capability and vendor comparison |
|---|---|
| `GET /v1/indexes` | List with prefix/cursor/page size; vendor equivalent uses namespaces |
| `GET /v1/indexes/{index}` | Summary/schema/fold state; partial metadata equivalent |
| `DELETE /v1/indexes/{index}` | Logical namespace deletion |
| `PUT /v1/indexes/{index}/documents` | Native write, conditional/deferred mutations, branch alias |
| `POST /v1/indexes/{index}/query` | Native retrieval, filtering, aggregation, geo, MMR, boosts, highlight, facets, as_of/session/bounded modes |
| `POST /v1/indexes/{index}/warm` | Synchronous local metadata warm |
| `PUT /v1/indexes/{index}/external` | Register a Parquet/Lance vector column in an allowed customer object |
| `DELETE /v1/indexes/{index}/external` | Remove external registration |
| `POST /v1/indexes/{index}/external/query` | Vector search returning external row numbers/scores; not full document/FTS/filter query API |
| `PUT /v1/indexes/{index}/external/copy` | Copy external index encodings; does not turn referenced data into an isolated backup |
| `GET /v1/indexes/{index}/quarantine` | Export rows acknowledged then rejected at fold |
| `DELETE /v1/indexes/{index}/quarantine` | Discard quarantined rows |
| `GET /v1/replications` | List continuous replication jobs |
| `PUT /v1/indexes/{index}/replication` | Create read-only destination following folded source, using configured store/tenant/index |
| `GET /v1/indexes/{index}/replication` | Status/progress/reconciliation |
| `DELETE /v1/indexes/{index}/replication` | Cancel replication |
| `POST /v1/indexes/{index}/replication/pause` | Pause continuous replication |
| `POST /v1/indexes/{index}/replication/resume` | Resume continuous replication |
| `PATCH /v1/indexes/{index}/schema` | Always refuses; migration requires a new index |
| `POST /v1/admin/fold` | Explicit tenant fold; background folding also exists |
| `POST /v1/admin/gc` | Retention/garbage collection |
| `GET /v1/admin/duties` | Background duty state |
| `GET /metrics` | Prometheus metrics |
| `POST /v1/internal/part` | Peer subquery execution, internal only |

External input is opt-in and prefix restricted. Registration takes `kind`, `source`,
`column`; query takes `vector`, `k`, `window` (k <=1000, window <=10,000, default 4k);
copy takes `index`, optional `tenant`, `store`. Replication creation takes
`source.{index,tenant,store}`. Neither is a direct counterpart to vendor namespace copy.

Additional native distinctions:

| pstore capability | Current scope | Product opportunity |
|---|---|---|
| `as_of` epoch reads | Retained history only; GC advances the floor | Reproducible retrieval/evaluation, with explicit retention and export semantics |
| Session tokens / bounded reads | Tokens cover writes; bounded staleness up to configured API maximum | Useful explicit consistency choices beside a strong-compatible default |
| Facets beside retrieval | Counts filtered set, not rank matches; `facets_epoch` can differ | Make counts/hits snapshot-consistent before claiming transactional facets |
| Geo predicates / boosts | Radius/box filters and distance-based boosting | Keep as native extensions with unit/date-line fixtures |
| MMR diversity | Candidate window, similarity-based diversification | Useful retrieval control; independent from group quotas |
| External Parquet/Lance | Vector column access with range reads, row-number results | Expand zero-copy BYOC workflows, subject to source versions/access/cost |
| Continuous replication | Follows folded source; pause/resume/cancel | DR building block, requires RPO/RTO and isolated restore proof |
| Quarantine visibility | Exposes deferred schema rejection | Useful diagnostics; not a substitute for rejecting invalid writes before acknowledgement |
| Physical request/byte accounting | Native responses and metrics | Transparent cost debugging; publish precise attribution rather than a commercial price claim |
| Open-source/self-host deployment | Apache-2.0 workspace; operator controls environment | Different buying model from a managed service; carries operational responsibility |

Sources: [router](../../crates/pstore-server/src/lib.rs),
[external handlers](../../crates/pstore-server/src/external.rs),
[replication handlers](../../crates/pstore-server/src/replication.rs),
[facets](../../crates/pstore-server/src/facets.rs),
[time travel tests](../../crates/pstore-server/tests/it/time_travel.rs),
[session tests](../../crates/pstore-server/tests/it/session.rs),
[bounded tests](../../crates/pstore-server/tests/it/bounded.rs).

## Architecture, operations, security, and cost

| Area | turbopuffer documented product | pstore observed implementation / assurance gap |
|---|---|---|
| Durable tier | Object storage, WAL, asynchronous indexing | Blob storage plus immutable segments, tenant HEAD CAS and dense WAL lanes; no external durable metadata tier |
| Write aggregation | Namespace group commit | Tenant engine can bundle cross-index records in a lane flush; first-lane setup adds cost; do not extrapolate research node-level economics to every deployed path |
| Placement | Service operates routing/cache/indexing | Segment peer fan-out and gossip exist; 10,000-node objective is not a demonstrated deployment |
| Namespace sharding | Fixed configured shards with a shared write log and consistent strong snapshot | Distributing segment work does not establish independent persistent shard capacity/resharding semantics |
| Storage backends | Managed cloud regions; [Azure preview](https://turbopuffer.com/docs/regions), BYOC by arrangement | Server entry wires memory/S3/Azure; GCS library support is not an exposed production server backend |
| Caching | Managed memory/NVMe, warm hint, reserved pinning | Cache implementation and persistent local cache configuration; warm only serving process metadata; no replica reservation API |
| Read-only | General namespace flag | Per-index flag ([M149](../milestones/M149/SPEC.md)); rows a stale server accepted are set aside into the quarantine |
| Availability/scaling | Managed operations and commercial SLA | No equivalent externally demonstrated SLA, fleet-scale failure history, or autoscaling capacity guarantee |
| Auth/quotas | API keys, organization controls, limits | Native server trusts tenant header; meter library not wired; blob concurrency admission is not tenant rate/memory admission |
| Transport/isolation | Documented TLS and service tenant isolation | Operator perimeter required; admin/internal routes need separate authorization |
| Encryption | Default storage encryption; enterprise per-namespace KMS support | Operator bucket encryption; no public per-namespace KMS/rotation/revocation contract |
| Private networking | Enterprise AWS/GCP private endpoints; region eligibility varies | Deploy in private customer network; no managed PrivateLink/PSC product |
| Audit | [Administrative audit events](https://turbopuffer.com/docs/audit-logs), retention/streaming options | Metrics are not an attributable administrative/document audit log |
| Compliance | [Vendor reports SOC 2 Type 2 and offers DPA/BAA](https://turbopuffer.com/docs/security) | No corresponding audited assurance established; engine tests cannot prove it |
| Backup | [Copies across regions](https://turbopuffer.com/docs/backups); no automatic scheduler documented | Branch/copy alias shares objects; replication follows source; independent restore and retention remain product work |
| Native embeddings | [Schema/query inference](https://turbopuffer.com/docs/embedding), region/model/token constraints | Missing; external application embedding works but is a different product |
| Model reranking | Managed candidate reranking, availability qualified above | Native numeric boosts/MMR are different; no model hosting/billing |
| SDKs/developer tools | C#, Go, Java, Python, Ruby, TypeScript SDKs and Rust beta; live testing guidance | Rust libraries and raw HTTP; no discovered generated SDK/OpenAPI product suite or TP-compatible local emulator |

The old parity document's rejection of pinning on ownership grounds is too broad for
capacity reservations. Immutable durability and cache residency are different concerns.
Conversely, its claim that physical copy buys nothing is wrong for independent backup,
cross-account authorization, and encryption boundaries. These corrections are linked
from the historical document; they do not justify adding correctness leaders or leases.

Pinning replicas do not currently autoscale. Pinning changes the billing model to
namespace GB-hours times replicas, with a documented 128 GB and ten-minute floor.
That cost model needs a separate comparison from usage-priced queries. The vendor also
provides a dashboard for document browsing/querying; no equivalent pstore administration
UI was found. Neither product supplies an automatic document chunking service through
the compared API; the vendor publishes a chunking guide and expects an ingestion pipeline.

The vendor [testing guide](https://turbopuffer.com/docs/testing) uses isolated live
namespaces/branches; no local service emulator is documented there. pstore has
MemoryStore and backend-emulator test harnesses, which are useful development tools
but do not emulate the turbopuffer wire contract.

### What can be said about performance and price

No comparative speed or dollar advantage is established. Vendor retail pricing and
pstore's physical cloud bill are different units. A fair comparison needs equal
durability, consistency, recall, data types, region, result projection, and workload.

The [published pricing](https://turbopuffer.com/pricing) snapshot has plan minimums
($16 launch, $256 scale, enterprise starting at $4096), usage pricing, volume/batch
discounts, and enterprise terms. These are vendor commercial terms, not a pstore TCO
baseline. Its calculator displays storage <=$0.33/GB, writes <=$2/GB, and queries
<=$1/PB; effective billing includes minimum queried size, returned bytes and discounts.
Use the applicable region/plan rate card rather than treating calculator units as a
cloud request tariff. The model must include compute, cache disks, blob requests, stored bytes,
compaction/replication amplification, cross-AZ/region transfer, idle capacity, inference,
and operations labor. Cost per successful query at a stated recall/SLO matters more
than a list price per byte.

pstore's [service objectives](../research/11-design/slos.md) explicitly block production
latency claims pending cloud measurements. The corrected D-34 budget is **at most three
sequential blob rounds to rank plus one to fetch rows**; opt-in exact rerank can add a
ranking round. This overrides a shorthand claim of three end-to-end rounds.
Zero LISTs and request/depth assertions are meaningful evidence about mechanisms, not
a proof of 100 ms production latency or a universal small byte footprint.

The [M125 verification record](../milestones/M125/VERIFIED.md) reports provisional WSL2
warm synthetic microbenchmarks (48,000 rows, 64 dimensions). They are neither cloud
latencies nor a comparison with turbopuffer. Its cited clustered recall@10 is 0.9810,
while the same record notes 0.7380 on uniform data; the clustered gate is not a universal
recall guarantee. The [plan](turbopuffer-next-plan.md) specifies a fair evaluation before
any performance or cost positioning.

## Recommendation

Provide a trusted, durable, explicitly versioned search API first. Add a turbopuffer
facade only over semantics pstore can actually honor. Then expose text-only/typed/sparse
documents, complete retrieval composition and shared snapshots, and deliver isolated
copy plus operational diagnostics. Pursue reserved capacity and managed model inference
after core contracts and cloud economics have evidence.

This is a product sequence, not a claim that the entire vendor service can be reproduced
by renaming endpoints. The [next-delivery plan](turbopuffer-next-plan.md) gives dependency
order, proposed acceptance criteria, architectural budgets, migration/rollback rules,
and the first bounded implementation slice.

## Validation of this snapshot

The inventory was checked against the pinned source for exact operation, schema,
property-pointer and composition-alternative set equality. All 13 normalized vendor
routes and all property names occur in this report, every inventory section anchor
resolves, and the 19 pstore route strings / 24 method operations were counted from the
current router. These are coverage checks, not semantic or live-service conformance tests.

`scripts/check-links.sh` passed over 425 Markdown files;
`python scripts/build-index.py --check` passed. The comparison also received a source-based semantic pass over defaults,
mutations, schema, projection, scoring, copies and snapshots. No independent human
review, live vendor probe, Rust suite run, or new performance measurement is claimed.
The report and plan change documentation only. Refresh this dated snapshot when either
public contract changes, retaining the source pins and recording new discrepancies.
