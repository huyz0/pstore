# Delivery status for the turbopuffer comparison improvements

Goal: deliver the improvements in the [delivery plan](turbopuffer-next-plan.md),
with the [API comparison](turbopuffer-2026-10-25.md) as the scope inventory.
The comparison itself was committed as `d1bc45b`. Existing native capabilities
described there are the baseline, not newly completed improvements.

Only a row linked to completed verification counts as delivered. A pending row is
still part of the goal; completing one milestone does not complete the program.
Implementation details are specified one milestone at a time under `docs/milestones`.

| Plan workstream | Improvement | Status / evidence |
|---|---|---|
| A | Close native write/query/document envelopes without mutation or I/O on refusal | Verified: [M126 spec](../milestones/M126/SPEC.md), [verification](../milestones/M126/VERIFIED.md) |
| A | Authenticated tenant identity and admin/internal authorization | Verified: [M127 spec](../milestones/M127/SPEC.md), [verification](../milestones/M127/VERIFIED.md) |
| A | Bounded decoding, memory/concurrency admission, overload responses | Partial: body bytes, active requests per process and tenant, a body-read deadline and typed 408/413/415/429 refusals verified in [M128](../milestones/M128/VERIFIED.md); a tenant's buffered rows flushed and bounded in [M141](../milestones/M141/VERIFIED.md) (a bound on rows, not memory); a body's JSON depth, array, object and string sizes bounded before any parse, and query legs' `top_k`, in [M143](../milestones/M143/VERIFIED.md); query plan reservations pending |
| B | Native OpenAPI, generated SDKs and semantic fixtures | Pending |
| B | Versioned compatibility facade with durable/strong defaults and correct envelopes | Pending |
| C | Applied mutation completion, actual affected counts/IDs, mixed-operation ordering | Partial: applied completion and exact counts verified in [M129](../milestones/M129/VERIFIED.md) ([proof](../research/11-design/mutation-completion.md)); affected ids and the order inside one request verified in [M130](../milestones/M130/VERIFIED.md); ids for by-filter operations, bounded at 10,000 less the named ids and paged by `allow_partial`, in [M139](../milestones/M139/VERIFIED.md) |
| C | Conditions with new-value references, partial filter progress and retry/idempotency policy | Partial: idempotency keys for applied writes (at most once, or an explicit refusal; retention is 32 tallying commits) verified in [M132](../milestones/M132/VERIFIED.md); `$ref_new` in upsert and patch conditions, resolved per operation at the door, verified in [M133](../milestones/M133/VERIFIED.md); a by-filter operation bounded at 100,000 rows and refused whole above it in [M136](../milestones/M136/VERIFIED.md); `allow_partial` progress and resume (`rows_remaining`, send again) in [M137](../milestones/M137/VERIFIED.md) |
| C/F | Shared durable snapshots for query legs, aggregates and facets | Partial: a multi-query's legs and a query's facets share one snapshot, verified in [M131](../milestones/M131/VERIFIED.md), and so do a multi-query's aggregations and facets in [M164](../milestones/M164/VERIFIED.md); lane tails are still probed per leg, and legs at mixed levels or `as_of` run apart |
| D | Text-only documents and persistent typed schemas | Partial: documents with no vector, an index of them recording width 0, and vector queries against it refused, verified in [M170](../milestones/M170/VERIFIED.md); persistent typed schemas pending |
| D | Typed IDs, uint64/UUID/bytes/datetime/nullable arrays and column upserts | Partial: request-declared `uuid` and `bytes` attributes verified in [M171](../milestones/M171/VERIFIED.md), `uint64` in [M172](../milestones/M172/VERIFIED.md), recorded per index and answered in the summary in [M173](../milestones/M173/VERIFIED.md), column upserts and `null` as an absent attribute in [M174](../milestones/M174/VERIFIED.md), null array elements in [M175](../milestones/M175/VERIFIED.md), `uint64` and `uuid` ids in [M176](../milestones/M176/VERIFIED.md) |
| D | Public string-key sparse vectors and named-field ANN contracts | Pending |
| D/H | Stored-vector projection, float/base64 response encoding and reconstructable export | Pending |
| D | f16/i8 and multi-vector schema/precision contracts | Pending |
| E | Multi-field/array BM25, tokenizer versions, language configuration, token/prefix queries | Partial: token filters over string arrays and `last_as_prefix` verified in [M138](../milestones/M138/VERIFIED.md); BM25 over a string-array text field in [M150](../milestones/M150/VERIFIED.md); several text fields in [M151](../milestones/M151/VERIFIED.md) to [M156](../milestones/M156/VERIFIED.md), split across servers in [M154](../milestones/M154/VERIFIED.md); the `pre_tokenized_array` tokenizer in [M158](../milestones/M158/VERIFIED.md); a query over several fields split across servers in [M157](../milestones/M157/VERIFIED.md); `last_as_prefix` on a BM25 leg in [M180](../milestones/M180/VERIFIED.md); the `word_v0` to `word_v4` tokenizers and `max_token_length` in [M181](../milestones/M181/VERIFIED.md); stopwords beyond English pending |
| E | Missing array predicates and compatible fuzzy semantics | Done: `AnyLt`/`AnyLte`/`AnyGt`/`AnyGte` verified in [M135](../milestones/M135/VERIFIED.md), the vendor's `Fuzzy` (substring match, `max_edit_distance` thresholds, `case_sensitive`, string arrays) in [M145](../milestones/M145/VERIFIED.md) |
| E | Exact scoring expressions and computed distance/clause signals | Pending |
| E | Structured highlights with fragment/offset contracts | Done: `fragment_by`, `fragment_limit`, `include_offsets` and `$highlights` over strings and string arrays in [M159](../milestones/M159/VERIFIED.md); `rank_fragments_by` refused |
| F | Atomic multi-query, outer RRF, candidate merging and global windows | Partial: one snapshot for legs at one level in [M131](../milestones/M131/VERIFIED.md); outer RRF with `rank_constant`, `weights`, `limit` and `offset` in [M142](../milestones/M142/VERIFIED.md); aggregations and facets inside a multi-query in [M164](../milestones/M164/VERIFIED.md), named `ForEachUnique` groups pending; model reranking is J |
| F | Multi-key sorting, relevance pagination, group quotas and expanded grouping | Partial: sorting by up to 8 keys verified in [M140](../milestones/M140/VERIFIED.md); a hard per-group limit over a relevance window in [M146](../milestones/M146/VERIFIED.md); relevance `offset` in [M163](../milestones/M163/VERIFIED.md); expanded grouping pending |
| G | Empty creation, schema/metadata reads, bytes/timestamps/index progress and generic read-only | Partial: empty creation (`PUT /v1/indexes/{index}` with dims and metric, surviving restart, listed, reusable after a drop) verified in [M147](../milestones/M147/VERIFIED.md); bytes and created/updated times in the summary verified in [M148](../milestones/M148/VERIFIED.md); generic read-only (`PATCH /v1/indexes/{index}`, refused by every write route and the drop, stale rows set aside) verified in [M149](../milestones/M149/VERIFIED.md); durable/unindexed progress across every writer (`?progress=true`) verified in [M160](../milestones/M160/VERIFIED.md); a documented concurrent-update model for list pages, with each page's HEAD epoch, verified in [M161](../milestones/M161/VERIFIED.md) |
| G | Controlled schema evolution, backfill and migration | Pending |
| H | Separate branch/isolated-copy/replication products and cross-store authorization | Pending |
| H | Restartable asynchronous operation status/results | Pending |
| H | Snapshot export/restore and independent recovery proofs | Pending |
| H | Online recall, explain and bounded data warming | Pending |
| I | Qualified real-cloud backend profiles, including server GCS support | Pending |
| I | Cache/compute reservations and persistent namespace sharding | Pending |
| I | KMS/encryption boundaries and copy re-encryption | Pending |
| I | Deployment isolation, audit retention, rolling upgrades and measured recovery | Pending |
| J | Optional embedding/reranking adapters, model versions and token accounting | Pending |
| Evaluation | Comparable cloud architecture experiments and complete TCO model | Pending |
| Evaluation | Reproducible per-feature CPU/allocation/quality microbenchmarks | Parser CPU/allocation campaign verified in M126; broader campaign pending |

Cloud eligibility, model credentials and operational/compliance evidence cannot be
substituted by local tests. Each affected workstream retains its acceptance targets
and limitations from the plan until the required evidence exists.
