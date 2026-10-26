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
| A | Authenticated tenant identity and admin/internal authorization | In progress: [M127 spec](../milestones/M127/SPEC.md), [verification](../milestones/M127/VERIFIED.md) |
| A | Bounded decoding, memory/concurrency admission, overload responses | Pending |
| B | Native OpenAPI, generated SDKs and semantic fixtures | Pending |
| B | Versioned compatibility facade with durable/strong defaults and correct envelopes | Pending |
| C | Applied mutation completion, actual affected counts/IDs, mixed-operation ordering | Partial: applied completion and exact counts verified in [M129](../milestones/M129/VERIFIED.md) ([proof](../research/11-design/mutation-completion.md)); affected IDs and a pinned mixed-operation order pending |
| C | Conditions with new-value references, partial filter progress and retry/idempotency policy | Pending |
| C/F | Shared durable snapshots for query legs, aggregates and facets | Pending |
| D | Text-only documents and persistent typed schemas | Pending |
| D | Typed IDs, uint64/UUID/bytes/datetime/nullable arrays and column upserts | Pending |
| D | Public string-key sparse vectors and named-field ANN contracts | Pending |
| D/H | Stored-vector projection, float/base64 response encoding and reconstructable export | Pending |
| D | f16/i8 and multi-vector schema/precision contracts | Pending |
| E | Multi-field/array BM25, tokenizer versions, language configuration, token/prefix queries | Pending |
| E | Missing array predicates and compatible fuzzy semantics | Pending |
| E | Exact scoring expressions and computed distance/clause signals | Pending |
| E | Structured highlights with fragment/offset contracts | Pending |
| F | Atomic multi-query, outer RRF, candidate merging and global windows | Pending |
| F | Multi-key sorting, relevance pagination, group quotas and expanded grouping | Pending |
| G | Empty creation, schema/metadata reads, bytes/timestamps/index progress and generic read-only | Pending |
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
