# What pstore should provide next

**Decision proposal, 2026-10-25.** Based on the [current comparison](turbopuffer-2026-10-25.md),
not the old M8-era parity matrix. This is a delivery plan, not an implementation claim
or an approved change to existing contracts. No runtime code changes are included.

The next product should be a safe BYOC search engine with a stable native API and a
clearly bounded turbopuffer compatibility profile. Make durable writes, tenant identity,
mutation completion, and read snapshots dependable before broadening the API.
Text-only documents, persistent typed schemas, public sparse search, and complete hybrid
retrieval are the most useful capability additions after that foundation.

## Outcomes and order

| Priority | Outcome to provide | Workstreams | Depends on | Why this order |
|---|---|---|---|---|
| First | Trusted API that rejects unsupported semantics and bounds request resources | A | Current implementation | Prevents silent misuse and tenant/resource isolation failures |
| Next | Durable/strong core contract, truthful mutation completion, and a versioned compatibility subset | B, C | A; C semantic proof | Existing applications cannot safely migrate without these |
| Next | Text-only, typed documents, schema reads, column ingestion, and sparse HTTP retrieval | D | A, B contract; C for mutation support | Broadens real workloads without outsourcing embeddings or forcing dummy vectors |
| Then | Multi-field text retrieval, missing filters, computed scores, useful highlighting | E | D | Closes common search application gaps |
| Then | Shared-snapshot multi-query, outer fusion, sorting/grouping, group quotas | F | C, D; E for expression reuse | Makes count/hit/fusion results composable and predictable |
| Alongside retrieval | Namespace lifecycle, schema evolution and diagnostics; isolated copy/export/restore | G, H | A, C/D schema; H object-liveness design | Makes deployment, migration and recovery practical |
| After cloud evidence | Capacity reservations, persistent namespace sharding, KMS and managed operation | I | G/H and benchmark campaign | Requires resource/failure/cost proofs, not field aliases |
| Optional integration | Embedding and model reranking | J | A/D/F and explicit model product choice | Convenient, but not a prerequisite for a correct search engine |

Workstreams are dependency groups, not preassigned milestone numbers. Following the
repository's [spec procedure](../../.agents/skills/spec/SKILL.md), only the next bounded
slice should become a detailed milestone spec. Later groups retain acceptance targets
and risks, then get respecified against the code when reached.

## First implementation slice: make unsupported requests fail closed

Deliver a precise native request contract before exposing compatibility endpoints.
This is a bounded slice with direct value even if full compatibility is never pursued.
It does not include authentication, new defaults, or schema evolution; those remain
required follow-on work in A/B/D, and this slice alone is not a production-readiness claim.

Proposed acceptance criteria for the next delta spec:

1. Every public JSON request object has an explicit allowed-field set. Unknown fields
   produce a typed 400 with the offending field/path. This includes native write/query
   roots, documents, multi children, and all nested option objects; supported expression
   languages retain their documented variants. Already strict external/replication
   requests retain their behavior.
2. Invalid write requests make zero blob writes and change no buffered documents,
   session state, or index registry entries. Inject an unknown key into every object
   position of a valid write fixture and compare observable state before/after.
3. Existing valid request fixtures preserve result IDs, ordering, epochs, score meanings,
   and refusal codes except for the explicitly specified new field-validation error.
   Include both the typed fast parser and its JSON-tree fallback, duplicate-key inputs,
   and malformed nested arrays.
4. Native defaults remain explicitly documented as batched/eventual. Unsupported
   `return_affected_ids`, `disable_backpressure`, `compute_attributes`, or `limit` cannot
   succeed while doing something else. A request cannot claim it requested durability
   through an unrecognized alias.
5. Validation adds **0 W, 0 R, 0 LIST**. Existing write cost and cold query depth bounds
   still hold. Benchmark valid and rejected requests to quantify CPU/allocation impact;
   do not trade a second full vector parse for strictness without measuring it.

Write the negative fixtures first and observe them fail against current code. The
decisive mutation is dropping an unknown field and accepting the request. Use one
request definition to derive decoding and schema information where possible
([gate-design](../../.agents/skills/gate-design/SKILL.md), rungs 1/2); use deterministic
fixture tests for behavior, not an agent-maintained list checked by opinion.

Migration: clients relying on ignored keys must remove them or use a documented
extension namespace. Publish this as a deliberate validation change with a release note.
Do not accept unknown safety fields under a generic permissive compatibility switch.
Rollback is the prior release/configuration, with the loss of strict validation stated;
there is no stored-data migration in this slice.

## A. Tenant trust, admission, and public contract

**Provide:** authenticated tenant selection, admin/internal separation, deterministic
resource limits, and honest overload responses. A deployment can use a trusted gateway,
but the server must accept tenant identity only through a configured authenticated
principal or authenticated gateway channel. A caller-controlled header is not identity.

Acceptance targets:

- Cross-tenant read/write/list/copy/replication requests are refused before accessing
  that tenant's store. Admin, metrics and peer endpoints have explicit exposure policies.
  Test forged headers, expired/invalid credentials and source/destination authorization.
- Bound compressed and decoded bytes, JSON nesting, documents, attributes, vectors,
  top-k, active queries, and per-tenant buffered bytes. Decode/admit incrementally or
  reserve a bounded budget before materializing the request. At configured limits the
  last valid request succeeds; one byte/item above is refused without mutation.
- Distinguish 400 validation, 401/403 authorization, 413 size, 429 tenant admission, and
  503 transient backend capacity. Retries must not duplicate a completed mutation;
  retry hints have a documented scope and duration.
- Version limits rather than copying vendor ceilings without resource evidence. A
  configured 512 MB write is a product promise only after decode-memory, concurrency,
  fault, and throughput measurements cover that size.

Budget: identity/limit validation adds no per-document/index blob request. Prefer local
credential verification or bounded cached key metadata; define revocation freshness and
rotation explicitly. Per-request auth traffic must be accounted separately. Use local
admission for resource protection; it does not become a correctness lock or tenant owner.

Risk: rejecting too late still allows memory exhaustion. Negative tests must assert
bounded allocation/active-work behavior, not only a final 413/429. Deployment TLS and
gateway configuration need a reproducible smoke test; parser tests do not prove them.

## B. Native API specification and compatibility boundary

**Provide:** a maintained native OpenAPI and generated clients, plus a separate
compatibility facade where equivalence is proven. Keep native extensions available.

- Define types, defaults, nulls, projections, errors, score direction, missing values,
  result envelopes, and limits. Generate schema/client artifacts from the same versioned
  contract. Start with Python/TypeScript clients; add Rust once the wire contract settles.
- In the compatibility profile, successful writes are durable and reads default strong.
  Explicit native batched/eventual/session/bounded options remain in the native API.
  Do not silently translate `not_folded` into a successful strong response.
- Add only verified vendor routes, payload mappings, flattened attributes, typed IDs,
  `$dist` meaning, error envelopes, projection behavior, and `limit` aliases. Return an
  explicit unsupported error for unimplemented routes/options. Resolve metadata aliases
  and analyzer-default conflicts against a pinned SDK and an authorized service account.
- Preserve native physical cost under a namespaced extension. Do not manufacture
  vendor `billing`/model token statistics or fabricate cache temperature.

Completion: for every advertised compatible operation, a pinned official SDK fixture
executes against the facade with matching validated request/response shapes and semantic
outcomes. Include negative cases and precision-sensitive IDs. Maintain an explicit
compatibility manifest linking supported operations/variants to fixtures; never advertise
"compatible" based only on JSON validation.

Budget: facade translation adds zero blob requests. Durable acknowledgement uses the
existing bundle write path and accounts for first-lane registration. Strong reads use
actual durable state; any additional recovery reads require an explicit budget in the
implementation spec. Do not introduce a per-index polling timer or LIST discovery.

Migration/rollback: separate route/version namespace and opt-in clients. Preserve native
response score semantics. Deploy reader support before any new stored format; revert
the facade independently from engine format changes.

## C. Mutation semantics and read snapshots

**Provide:** explicit mutation completion, truthful affected results, mixed-operation
ordering, and a read model an application can reason about.

This requires an architecture proof before implementation. The present cheap durable
append and deferred fold do not automatically provide vendor synchronous transaction
results. A forced fold after every request can recreate the CAS/cost bottleneck the
architecture was designed to avoid. A job token is a useful native API, but it is not
equivalent to a successful synchronous vendor write.

First define and verify the linearization/completion points for two modes: durable
intent acceptance, and fully applied mutation completion. Return actual counts/IDs only
for the latter. Decide which operations the synchronous facade can support within those
guarantees; refuse others until proved. If the product requires full vendor semantics,
that semantic work is mandatory rather than an acceptable permanent subset.

Acceptance targets:

- Conditional reads use the specified old row and `$ref_new` values; absent/null rules,
  repeated IDs, and conflicts are pinned. A 100-writer adversarial schedule never reports
  two mutually exclusive conditions as successful. Successful responses survive restart.
- A mixed-operation batch follows its selected API's order. The same ID in filter delete,
  filter patch, upsert, patch and explicit delete produces the specified final row.
  There is no half-applied successful atomic batch under injected crashes/CAS conflicts.
- Applied responses report exact counts and optional IDs. Intent responses name pending
  completion and expose a stable operation identifier; no submitted count is labeled
  affected. Partial filter mutations expose progress and resume without skipped/duplicate
  effects. Refused operations leave no hidden mutations.
- Schema conflicts are rejected before an applied acknowledgement, or return an explicit
  pending/rejected result under the intent mode. Quarantine is observable but never
  presented as successful applied ingestion.
- A strong read after an applied write sees that write; an unsupported/refused read is
  explicit. A multi/facet execution can bind every leg to the same tenant snapshot and
  durable-tail boundary. Inject a fold between legs and assert identical snapshot IDs.

Budget: keep one steady-state bundle PUT per durable batch where that contract applies;
measure separate read/commit cost for applied conditions/filter mutations. Do not hide a
whole-corpus scan behind constant request-count claims. Zero LISTs; publication fenced
by blob CAS; no leader, correctness lease, per-record durable object, or per-index tick.
If the synchronous contract cannot meet the architecture, record the incompatibility
and required design change before offering it. Never relax the existing gates to land it.

Migration: introduce truthful result fields under a new response version and deprecate
submitted counters explicitly. Retrying after an ambiguous network failure needs a
bounded, durable idempotency/result policy. Do not claim exactly-once execution without
proving its scope and retention. Format changes require mixed-version recovery tests.

## D. Documents, types, schema reads, and sparse search

**Provide:** text-only documents; typed IDs/attributes; persistent schema; row/column
ingestion; multiple vector fields; public sparse search. These are the largest next
workload gains after the contract foundation.

Sequence: remove mandatory dummy vectors and define vector-free schema; add persistent
typed attributes/IDs and schema GET; then expose existing named/sparse primitives under
the new type contract. Treat f16/i8 ingestion/output and multi-field ANN indexing as
separate increments, not as aliases for internal quantization.

Acceptance targets:

- A vector-free document can be written, filtered, BM25-ranked, projected, patched and
  exported across restart. Dense/multi-vector queries against an incompatible schema
  fail explicitly. Existing dense indexes preserve IDs, distances and recall fixtures.
- Round-trip uint64 boundaries including values above JavaScript's safe integer range,
  canonical UUIDs, bytes, nullable scalar arrays, and UTC datetimes. No lossy conversion
  or inconsistent request-local typing; schema enforcement covers concurrent first writes.
- Row/column batches normalize to identical documents. Missing/null vector columns and
  column-length mismatches are refused atomically. Null unset and patch behavior are
  specified independently from absent attributes.
- Sparse string dimensions map deterministically and losslessly across restart/writers;
  do not hash them into u32 without collision handling. Compare sparse top-k against
  exhaustive dot products, including ties/zero/negative weights as the contract permits.
- Named vector widths and metrics survive restart; advertise ANN only for fields with
  an actual ANN index. Validate f16/i8 precision and score direction with exact oracles.
  Return requested stored vector fields in the selected encoding so an HTTP export can
  reconstruct the index; projecting scalar attributes alone is insufficient.

Budget: new types live in existing records/segments; no blob per attribute/dimension or
document. Projected output shares existing fetches. Sparse and text ranking retain the
gate-scale depth bounds; returned bytes are measured. New schema publication must not
add a CAS to every ordinary write solely to confirm an unchanged type.

Migration: version schema/segment codecs with readers-first deployment. For incompatible
type/analyzer changes, build a new index and verify an epoch-bounded export before cutover;
do not reinterpret old bytes. Keep old readers until rollback/retention windows expire.

## E. Text retrieval, predicates, computed signals and highlighting

**Provide:** multiple string/string-array BM25 fields, explicit tokenizer versions,
prefix/token-array queries, full scoring expressions, missing array/fuzzy predicates,
and structured highlights.

Acceptance targets:

- Golden token streams cover every supported tokenizer/language/config combination,
  Unicode boundaries, token byte limits and arrays. Store analyzer versions in schema;
  neither side's disputed stopword default is inferred. Upgrades never silently retokenize.
  Resolve the guide-only k3 wire contract before offering query-term tuning parity.
- `AnyLt/AnyLte/AnyGt/AnyGte`, token arrays, `last_as_prefix`, and vendor Fuzzy configuration
  match exhaustive reference predicates for missing/null/empty arrays and nested negation.
  A negative predicate must never turn an unbound analyzer into an all-row match.
- Represent BM25, Sum, Max, Product (both forms), filter score, Attribute, Saturate,
  Decay and Dist as a typed expression tree. Whole-corpus results match a small exhaustive
  oracle, including zero-score exclusion and invalid negative score domains. Keep native
  windowed boost explicitly approximate; do not relabel it as exact expression execution.
- Computed clause scores/VectorDist do not change matching/ranking. Highlights return
  structured fragments and all specified offsets, including emoji, combining characters
  and string-array indices. HTML escaping/markup is an explicit client concern/contract.

Budget: use existing posting/block stats and shared fetched attributes. No per-hit blob
RPC for a computed scalar/highlight. Bound expression nesting, clauses, patterns and
fragment output; report candidate/byte costs for expressions requiring broad scans.
Keep relevance quality gates and an intentionally poor control ranker.

Risk: exactness can be expensive. Build WAND/upper-bound pruning only after the exhaustive
oracle is stable; then prove pruning preserves results under adversarial score bounds.

## F. Shared-snapshot query composition

**Provide:** atomic multi-query, outer RRF/model-ready candidate merging, multiple sort
keys, relevance offset, hard group limits, and aggregate subqueries.

Acceptance targets:

- All subqueries bind one snapshot, including ranking, aggregations and facets. A writer
  or fold racing between legs cannot change one leg's view. Preserve native per-query
  execution as a separately documented option if useful.
- Outer RRF has integer `rank_constant`, positive weights in query order, correct
  deduplication and deterministic ties. Retrieve each leg's requested global prefix;
  M125's global limit behavior remains covered. Post-fusion limit/offset are applied at
  the correct stage. Compare the complete result with a simple reference implementation.
- Up to eight sort keys handle ties/nulls/types and ascending/descending direction;
  cursor pagination and offset have explicit bounds. Hard `limit.per` counts by the
  specified key; MMR cannot substitute for it.
- Unary Count, attribute Sum, grouped counts and named `ForEachUnique` groups work in
  multi. Duplicate elements in one array do not double-count the same unique group.
  Rank-window limits do not accidentally truncate aggregate input.

Budget: acquire a snapshot once, fan out legs within rounds, reuse metadata/cache reads.
Maintain rank depth <=3 plus <=1 row-fetch round for the existing bounded paths; exact
rerank retains its declared extra-round exception. Query count multiplies CPU/bytes even
if depth stays flat. Admission reserves for total legs/windows, not only the outer top-k.

Migration: preserve native `score` and `$dist`; facade uses overall rank/rerank score.
Snapshot identifiers and partial/refusal behavior belong in both documentation and tests.

## G. Namespace lifecycle and schema evolution

**Provide:** empty creation, schema/metadata reads, logical bytes and timestamps, global
indexing progress, generic read-only, and controlled attribute/index changes.

Acceptance targets:

- Empty namespaces survive restart and list correctly. Prefix/cursor pages have a
  documented concurrent-update model. Deleted names can be reused without old WAL or
  replication work reviving a previous incarnation.
- Metadata distinguishes durable/unindexed/indexed states and folded row counts, with
  consistent timestamps and size definitions. Progress is not derived solely from the
  serving process's buffer. Freshness is explicit; metadata gathering uses bounded reads.
- Read-only rejects new document/schema mutations from every server and copy/job entry
  point. Define whether previously accepted jobs drain or cancel; indexing already
  accepted data must remain correct. A paused writer cannot publish past a newer flag
  without satisfying its fencing rules. Branch inheritance is defined and tested.
- Add/drop/index-option changes have an explicit support matrix. Backfill publishes a
  version atomically; a failed worker does not expose half an index. Incompatible type,
  metric or analyzer changes use migration rather than unsafe in-place reinterpretation.

Budget: lifecycle/schema metadata publication can use bounded HEAD CAS; unchanged ordinary
writes do not pay a lifecycle CAS. No scheduled request per inactive index and no LIST.
Backfills scale with bytes, are cancellable/retryable and expose cost/progress.

## H. Isolated copy, async operations, export and diagnostics

**Provide:** separate branch/copy/continuous-replication products; operation status;
restorable exports; online recall; explain; actual data warming options.

Acceptance targets:

- Branch shares immutable data and protects reachability under source/branch deletion.
  Isolated copy materializes independently readable destination objects; deleting the
  source and its storage credentials does not break destination queries or restore.
  Cross-tenant/store/region copies authorize both sides and enforce encryption policy.
- Async operations expose token, start/finish, state/progress, typed success/error and
  retained results. Crash/retry/CAS contention cannot publish partial success. Progress
  free text is not a machine state. Define result expiry and ambiguous-start recovery.
- Export documents from a fixed retained snapshot, including IDs/types/schema, so a restore
  into a fresh tenant produces the same exact-query/filter results and row digest. A
  concurrent writer/GC cannot make an acknowledged export silently inconsistent. Vendor
  live paged export is not itself a stable-snapshot guarantee.
- Online recall samples ANN and exhaustive results at one snapshot, with reproducible
  seeds/filters/ground truth and bounded work. Explain shows actual planning choices,
  candidate strategy and estimated/observed costs without leaking another tenant.
- Warm hint is asynchronous with status/eligibility, or explicitly remains native
  synchronous metadata warm. A data-warm option states byte/time budgets and scope across
  processes; accepting a hint is not a guarantee that the next routed query is warm.

Budget: physical copy/restore scales with bytes and is background work; no object per
document. Job discovery uses bounded registers and CAS-on-publish, not correctness leaders.
Charge transfer/duplicate storage and cap concurrency. Sampling/explain do not secretly
force gate-scale exhaustive scans on ordinary user queries.

Migration: rename/document the existing copy alias before claiming isolated copies.
DR release requires explicit RPO/RTO measured with node/store loss, not only a successful
copy call. Replication following source deletions is not an independent recovery point.

## I. Cloud operations and reserved capacity

**Provide after measurement:** deployable backend profiles, capacity controls, optional
cache/compute reservations, persistent namespace sharding where needed, and enterprise
security integrations.

- Wire and qualify GCS in the server only after its production CAS/create-if-absent and
  range semantics pass the backend matrix. Complete S3/Azure cloud profiles as well;
  emulator conformance and provisional WSL2 timings do not prove production behavior.
- For capacity reservation, expose desired/ready replicas and utilization, admission,
  scaling/draining and quota policy. Kill all reserved processes: recovery uses blob
  state, not a lost owner. Reserve resources without introducing correctness leadership.
- For namespace sharding, prove document partitioning, global snapshots, ID uniqueness,
  mutation routing, aggregation/fusion and atomic publication. Test skew, a hot key,
  shard loss, expansion and rollback. Do not call existing segment distribution sharding
  parity. Match fixed shards first; vendor automatic/manual resharding is roadmap work.
- Add per-namespace KMS configuration, immutable encryption identity, copy re-encryption,
  revocation and cache behavior only with tested backend support. Operator bucket SSE
  is useful but has a different authorization boundary.
- Deliver reproducible least-privilege deployment, TLS/private network examples,
  attributable audit events with retention, restore drills, saturation dashboards and
  rolling upgrade/rollback. Separate administrative audit from document mutation audit.
  An SLA/compliance claim requires operations evidence and the relevant audit/contract.

Budget: publish compute, disk, storage, request, transfer and operational costs at idle
and saturation. Avoid cross-AZ peer traffic by default where the topology permits.
Reservations cannot require per-index heartbeat writes. Fault scenarios cover AZ loss
and degraded-but-alive storage, not just clean process termination.

## J. Optional embeddings and model reranking

**Provide if customers need a single retrieval endpoint:** inference adapters before
running a model-hosting fleet. External embedding/reranking remains supported.

Acceptance: model/region eligibility is explicit; tenant-scoped credentials and token
limits apply; model/dimension versions persist in schema; retries do not duplicate writes
or charges; timeouts and provider errors are typed; changes do not silently backfill or
mix embedding spaces. Model reranking uses a bounded deduplicated candidate set, returns
its score separately where needed, and obeys snapshot/result projection contracts.

Budget: account inference tokens, latency and dollars separately from blob operations.
No inference call per indexed record on a search read path. Batch ingestion inference and
set concurrency/time budgets; storage's depth budget does not describe provider latency.
Roll out by model/version behind an opt-in config; disable the adapter without making
already stored vectors unreadable.

## Performance and cost evaluation before positioning

The existing [SLO file](../research/11-design/slos.md) and
[M125 measurements](../milestones/M125/VERIFIED.md) remain the starting evidence.
No vendor superiority claim follows from them. Run two evaluations:

| Evaluation | Workload and controls | Required output |
|---|---|---|
| Architecture experiment | Equal region/hardware class where applicable; real backend; cold/warm/partial cache; tiny/many tenants and a large/hot tenant; steady/burst ingest; churn; restart/AZ failure | p50/p95/p99/p999 with sample counts/errors, freshness/ack latency, recall/NDCG, throughput at fixed SLO, request classes/depth, physical/logical bytes, CPU/RSS/cache occupancy, write/compaction/transfer amplification |
| Microbenchmarks | Parse/normalize row and column JSON, array/base64 f32, text-only docs, BM25/filters, sparse dot product, MaxSim/FDE, global top-k/RRF, computed/highlight output; fixed seeds and result digests | Time and allocations/peak memory per size, compiler/CPU/options, warmups/repetitions, dispersion/confidence, unchanged correctness oracle or explained semantic delta |

Include clustered and uniform vectors, realistic ACL selectivity, multilingual/long
text, skewed attribute values, sparse vocabularies, multiple vector dimensions, and
adversarial ties/nulls. Use real representative data when authorized and public datasets
otherwise. Freeze corpus/query versions. Evaluate equal recall and projection; do not
compare exact search on one side with a faster lower-recall mode on the other.

For vendor tests, pin SDK/version, region, service settings, account eligibility and
effective pricing; record request IDs and response performance/billing. Compare durable
acknowledgement and strong reads by default. Separate client/network time from server
time. Do not compare local MemoryStore warm timings to a remote managed service.

Convert observed resources to a dated TCO model: instance/cache utilization, idle floor,
blob storage and request unit prices, replication/GC/reindex, cross-AZ/region egress,
NAT, inference, observability and staff operations. Show cost per successful query and
per durable GiB ingested at the stated quality/latency bound, plus monthly scenarios for
sparse tenants and sustained hot tenants. Include volume/plan discounts and sensitivity
to cache hit rate, batching, retention and tenant activity. Keep WSL2/emulator numbers
marked **provisional**; only cloud runs can unblock current latency objectives.

Use existing deterministic cost/depth/quality tests where they cover a requirement.
Derive new coverage from the contract and use data-driven fixtures; reserve independent
review for semantic/architecture judgments. Do not add an expensive always-on benchmark
gate before the measurement is reproducible and its noise is understood.

## Planning estimates and release decisions

These are rough engineering effort ranges for an experienced engineer with review and
test infrastructure, not delivery dates or commitments. Parallelism does not remove
semantic dependencies, and full vendor/service parity is a multi-quarter program.

| Group | Rough effort | Main uncertainty |
|---|---|---|
| First strict-contract slice | 3..7 engineering days | Consistent validation through fast/fallback parsing without parse regression |
| Remaining A and native B core | 2..4 engineer-weeks | Auth deployment choice, memory admission, client/schema generation |
| C | 4..10+ engineer-weeks | Synchronous result/isolation design under cheap append and CAS invariants |
| D | 4..8 engineer-weeks | Persisted schema/ID formats, sparse vocabulary and migrations |
| E | 4..8 engineer-weeks | Tokenizer fidelity and exact expression pruning |
| F | 3..6 engineer-weeks | Snapshot/tail binding and composition memory bounds |
| G/H | 4..10 engineer-weeks | Metadata truth, object liveness, restartable copies and restore |
| I | 8..16+ engineer-weeks, plus operations | Cloud/failure evidence, shard design, KMS and deployment qualification |
| J | 2..5 engineer-weeks for adapters | Provider eligibility, retry/billing and model version policy |

Do not sum these into a promised date. Re-estimate after the C proof and first cloud
benchmark. Before implementing a workstream, write the next delta spec and verification
ledger; preserve current thresholds and require test-first evidence for code changes.

Release criteria should use named scopes:

- **Native BYOC core:** A/B native contract plus a clearly documented C completion mode,
  qualified backend, recovery/restore and deployment authorization boundary. Unsupported
  semantics are explicit; this label does not imply managed-service assurance.
- **Compatibility profile:** per-operation/variant semantic fixtures, pinned SDKs,
  durable/strong behavior, correct envelopes/results and an explicit unsupported matrix.
  Partial API coverage stays labeled partial.
- **Managed service:** measured latency/availability/failure recovery, capacity/admission,
  tenant authorization, support/on-call/incident process and commercial assurance. An
  engine release or a green local test suite does not satisfy this scope.

The recommended next action is to implement the strict-contract slice, then establish
the applied-mutation/read-snapshot contract while adding the trusted deployment boundary.
The next visible retrieval features should be text-only typed documents and public sparse
search, followed by multi-field text and shared-snapshot hybrid composition.
