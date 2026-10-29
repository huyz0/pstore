# M128: bounded public request ingress and active work

Independent spec review passed in round 1. Implemented after M127 landed; evidence is in
[VERIFIED.md](VERIFIED.md).

## Serves
D-55 and D-56 in [memory management](../../research/09-rust-stack/memory-management.md).
First admission layer of workstream A in [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
Add immutable versioned public ingress limits per Api, shared by all routers and
clones. Version 1 defaults: 2 MiB encoded body, 64 active public requests,
8 active requests per tenant, 128 MiB reserved body bytes, 30 s body-read deadline.
The existing implicit Axum body ceiling becomes an explicit typed API contract.
Reserve the maximum admitted body size before polling its body, retain the
reservation through handler completion, and refuse capacity without waiting.
This bounds server-owned body collection; it is not an RSS or decoded-memory bound.
Use bounded counters whose idle tenant entries disappear when their last request
finishes. No state scales with every tenant ever observed. Admission is process
local resource protection and confers no ownership, correctness lock or lease.
Apply authentication and route scope checks before body polling or tenant admission.
Verified JWT tenant identity supplies the tenant key; explicit private mode keeps
its existing header parsing and headerless administrative behavior. Unbound
private admin requests share one bounded private cohort instead of inventing an
identity or creating arbitrary map keys. Dedicated peer admission remains M127's.
Reject non-identity Content-Encoding with typed 415 unsupported_content_encoding
before parsing; decompression is unsupported.
A body over its limit returns typed 413 request_too_large. Local global/tenant/body
capacity returns 429 overloaded with Retry-After: 1; this is a local retry hint,
not a fleet reservation. Body-read deadline returns typed 408 request_timeout.
Malformed JSON and semantic failures retain 400; auth remains 401/403; transient
backend capacity retains 503. Admission never replays or retries a mutation.
Expose a checked builder and PSTORE_REQUEST_LIMITS_CONFIG (strict JSON, version 1)
that can lower these defaults. Reject unknown fields/versions, zero values,
integer overflow, tenant capacity above global capacity, and a body pool smaller
than one maximum body. Startup validates before opening cache/storage/listeners.
Existing native consistency/durability, parsed values and result meanings stay.
Decoded structure limits, query plan byte reservations, buffered tenant bytes and
flush reserve remain required subsequent A work; this slice does not close A.

## Acceptance criteria
1. At the configured body byte boundary a valid request succeeds; one byte above returns the typed 413 before engine/blob access or mutation, with and without Content-Length and across fragmented chunks. Duplicate or misleading lengths cannot enlarge collection.
2. Authentication and scope refusals occur without polling request bodies. Both verified tenants and explicit private fixtures preserve their existing identity/refusal semantics; unsupported content encoding is refused before parsing.
3. Global, tenant and byte capacities refuse immediately with 429 and Retry-After: 1 at saturation. One tenant cannot consume another tenant's remaining per-tenant allowance; all routers of one Api share the same limits.
4. Completion, error, body timeout, cancellation and dropped connection return every permit. At most the configured global active count plus one private cohort is represented in tenant accounting, and idle entries are removed. A later valid request succeeds after release.
5. Missing/valid configuration selects documented defaults/overrides; malformed or unsafe configuration refuses standalone startup before backend/cache construction or binding. Bare and authenticated Api constructors retain M127's exposure policy.
6. Existing valid server/workspace fixtures, cost/depth gates and target coverage floors pass. Record provisional CPU/allocation and peak body-reservation measurements for valid, oversized, saturated and cancelled requests with result/refusal digests and zero added blob operations.

## Test plan
1. Add ingress_byte_limits_precede_engine_access using small configured limits, exact-boundary JSON fixtures, false/missing length metadata and a chunk-poll spy. Observe current generic extractor behavior or late refusal. Mutation: trust Content-Length, collect before reserving, or permit limit+1.
2. Add authorization_precedes_body_polling_and_admission and unsupported_encoding_never_reaches_json using pending/panic-on-poll bodies and valid/invalid public principals. Mutation: place ingress outside the auth/scope boundary, use an unverified tenant key, or ignore Content-Encoding.
3. Add ingress_capacity_is_shared_and_tenant_scoped with controlled pending handlers and two tenants/routers. Assert exact statuses, retry hint and permit counts without timing thresholds. Mutation: allocate a pool per router, wait for capacity, or share one tenant counter across identities.
4. Add ingress_permits_recover_after_all_exit_paths with an injected clock/body future, drop guards and distinct-tenant churn. Assert live table size, byte/slot counters, then a positive request. Mutation: forget a release, retain idle entries, or reserve after starting handler work.
5. Add request_limit_configuration_precedes_store_and_bind with environment-reader unit controls and spawned-server refusal controls. Mutation: silently use defaults for invalid input or validate after backend initialization.
6. Run scripts/gates.sh, scripts/recall.sh, scripts/ndcg.sh, scripts/depth.sh and the existing coverage gate. Use an isolated ignored benchmark to separate body reservation from cumulative allocator traffic and total process RSS. Compare complete outcomes before latency. No new CI timing threshold.
Typed reservation guards are gate-design rung 1; boundary, cancellation and configuration fixtures are rung 3. No opinion-based gate checks capacities.

## RA budget
Ingress validation/admission adds 0 W / 0 Rseq / 0 Rpar / 0 LIST.
A refusal does not construct an engine or change buffered writes/session/registry.
Existing application peer phases and blob sequential-depth budgets remain intact.
No timer, blob request or retained accounting record per inactive tenant/index.

## Risks
Fixed maximum-body reservation trades utilization for an admission bound; record
that tradeoff in the benchmark rather than calling it free. Header/network buffers,
decoded JSON/typed values, response bodies and engine state are separate resource
classes and cannot be counted as covered by the body pool. A request already
mutated when its client disconnects follows existing completion semantics; a 429
is only issued before dispatch. Tests must distinguish refusal before dispatch
from cancellation after dispatch and must not claim retry idempotency from either.
Read timeout is a new public limit and needs a migration entry.

## Tasks
- M128.1 Implement public ingress reservations, tenant/global admission, strict startup configuration and typed refusals; qualify resource behavior and publish migration/evidence.
