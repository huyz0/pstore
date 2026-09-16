# M7c — The first door: an HTTP surface over the engine

**Serves:** **D-34** (`11-design/api-design.md` — the index is the noun, every tradeoff is a
client parameter, every response is honest about cost and freshness), **D-36** (JSON first),
and Design rule 4 (no LIST on a serving path). Opens the caller that
[M6c](../M6c/VERIFIED.md) said the schema half is blocked on.

**Depends on** [M5g](../M5g/SPEC.md) (`Engine::query` exists), [M6a](../M6a/SPEC.md) and
[M7a](../M7a/SPEC.md) (a backend that cannot fence is refused before anything is written).

## ⚠️ What this is, and the three things it is deliberately not

Nothing in the workspace speaks a protocol. Everything built so far is a library reachable
only from `cargo test`, which is why the backlog's one remaining non-hardware blocker reads
"blocked on a server". This milestone is **the door**: transport, routing, request and
response shapes, the error table, and the cost-and-freshness meta. It is small on purpose.

**Not the schema.** Who sets the text field, whether it may change on an index that already
has segments, and what a fan-out does when a tenant's segments disagree are three policy
questions, and answering them inside a milestone whose subject is transport is how both get
reviewed badly. M7c makes the caller exist; **M7d** asks it the questions.

**Not authentication, and not the quota.** `pstore-meter` exists and is not wired. A tenant is
whatever the request says it is, which is a **stated non-property**, not an oversight — the
server is not deployable to anyone and this spec says so in one place rather than implying
otherwise everywhere.

**Not a scheduler.** A fold is an endpoint, not a timer. A background loop makes every test
about timing, and the operation is what a later milestone schedules.

## Delta

**Adds** `crates/pstore-server`: a library with the router and handlers, and a thin binary.
`main.rs` wires, `lib.rs` decides — the rule `pstore-node` already follows, for the reason its
docs give: decisions inside `main` are measured at 0% coverage.

| Route | Does |
|---|---|
| `PUT /v1/indexes/{id}/documents` | `Engine::write`, then `flush` when `durability` is `durable` |
| `POST /v1/indexes/{id}/query` | `Engine::query` — dense, text, or both, fused |
| `GET /v1/indexes/{id}` | segment count, document count, epoch |
| `GET /v1/indexes` | the tenant's index names, from HEAD |
| `POST /v1/admin/fold` | `Engine::fold` — what makes a flushed write visible to a **different** process |

- **The lane is required and never defaulted**, `PSTORE_LANE` for the binary and a parameter
  for the library. ⚠️ **Lanes are single-writer and dense**: a second process writing the same
  tenant on the same lane starts at `Seq::ZERO` and overwrites bundle 0, so acknowledged
  durable writes vanish with no error anywhere. M7c is the first thing in this repository that
  can run two writers, so it is the first thing that can commit that failure. The server
  therefore **refuses to start without a lane**, and the residual hazard — nothing *detects* a
  collision, because a bundle is written with an unconditional `put` — is filed as a backlog
  row naming create-if-absent bundles as the fix, rather than fixed inside a milestone whose
  subject is transport and whose blast radius would be M2's density invariant.
- **Tenancy is a required header**, `X-Pstore-Tenant`, parsed as a `u64`. Absent or unparsable
  is `400`, never a default: a default tenant is a cross-tenant data leak with a plausible
  name. One `Engine` per (tenant, lane) is held in a registry keyed by tenant, because an
  `Engine` owns the memtable that makes a write visible before it is folded.
- **`pstore-engine` gains three public accessors**, because the server cannot ask HEAD
  anything today: `indexes()` — the tenant's index names in **one read**, `index_stats(name)`
  — segment count and epoch, and `pending_indexes()`, which is `pending_for_test` renamed and
  made public. ⚠️ **An index exists if HEAD names it OR this process has unfolded rows for it**,
  and that union is the definition **everywhere** — the enumeration route and the query route's
  existence check both. An index written a moment ago is queryable (criterion 2), so a `404`
  decided on HEAD alone would refuse the request criterion 2 requires to answer. The two
  criteria are the same predicate or they contradict each other.
- ⚠️ **Amended during implementation: resolving a hit was impossible inside the budget.**
  `Engine::query` answers with `(segment, row)`, and the only public way to turn a row
  ordinal into an id was `scan`, which reads every unpruned block and every vector — a read
  that scales with **documents**. So `Segment::ids_at` reads *only the blocks holding the
  wanted rows*, in one coalesced round and touching no vector section; `Answer` carries the
  `segments` snapshot it was computed against; and `Engine::resolve` maps hits to ids with
  **no second read of HEAD** — which is a correctness property as well as a cost one, since
  a fold between the two reads renumbers the segment list.
- **Every response carries `cost` and, on a query, `meta`** — blob reads, writes and bytes for
  *that request*, plus the epoch the answer was computed at and how many hits came from the
  freshness layer rather than a segment. `api-design.md` principle 3: if we are going to have a
  cold path we must be honest about who is on it.
  ⚠️ **The mechanism is a before/after delta on the tenant's `Accounted` counters, which are
  monotone and per-tenant rather than per-request.** So `cost` is exact for a tenant with one
  request in flight and **cross-attributes under concurrency** — a stated non-property, tested
  in the form that can be tested: two sequential requests, where the second's cost must be its
  own and not the running total.
- **The error table is code**, one `impl` from `EngineError` and the request's own failures to
  a `(status, code)` pair, with the body `{"error": {"code", "message", "retryable"}}`.

- **The server refuses to serve at startup on a backend that cannot fence**, using the
  `admits_durable_writes()` that M7a made public for exactly this caller, at **zero requests**.
  ⚠️ Necessary and not sufficient: `Engine::write` only calls `check_storable`, so a *batched*
  write to a divergent backend would otherwise return `200` and be refused only at the flush.
  The door guard is what closes that window.
- ⚠️ **Amended: `deny.toml` is unchanged, because the binary serves an in-memory store.**
  The plan was to add `pstore-server` to `object_store`'s wrapper list on the argument already
  recorded there for `pstore-node`. It turned out not to be needed and therefore not taken: a
  cloud code path that nothing has ever executed is worse than an honest local one, and the
  wrapper line belongs to the milestone that runs against a real backend. Criterion 3 proves
  the architecture with two servers over **one store**, which is the property, not the vendor.

**Does not add** `delete`, `multi_query`, `PATCH schema`, `branch`, `warm`, the binary codec,
streaming, or `as_of`. Each is in `api-design.md` and none is transport.

⚠️ **`durability: "async"` is not implemented and is refused**, rather than accepted and
treated as `batched`. An accepted-and-downgraded durability level is the exact shape of a
promise a storage system must never make.

## Acceptance criteria

1. A write of 10 documents with the default durability issues **0 blob requests of any class**.
   With `"durability": "durable"` it issues **2 write-class requests and 1 read on the lane's
   first flush** — the lane registration, which is one CAS per lane lifetime — and **exactly 1
   write-class request, 0 reads** on every flush after it. Both phases asserted, because a test
   that only ever sees the first cannot tell a per-lane cost from a per-write one.
2. A document written and then queried **in the same process is returned without a fold**, and
   `meta.unfolded_hits` is non-zero for it.
3. A document written with `durable`, folded through `POST /v1/admin/fold`, is returned by a
   query served from a **second server instance** built over the same store with an empty
   memtable, with `meta.unfolded_hits == 0` — it has no memtable — and `cost.blob_reads > 1`,
   so the answer came out of the blob store. ⚠️ **Amended: before the fold that query is
   `404 index_not_found`, not an empty result.** The index does not exist for that process by
   the same existence union criterion 6 uses — HEAD names nothing and its memtable is empty —
   and inventing an empty-result answer would mean the union disagreeing with itself. The
   witness is the code and the second instance's own `meta.epoch` on the query that follows.
4. **Tenancy is enforced and never defaulted**: no header, an empty header and a non-numeric
   header are each `400 tenant_required`; two tenants writing the **same index name** read back
   only their own documents; and `POST /v1/admin/fold` folds the header's tenant, under the same
   rule as every other route.
5. **Every endpoint issues zero LISTs**, asserted on `OpClass::List` per request, including the
   enumeration one — the endpoint whose obvious implementation is a LIST.
6. The error table is asserted per row: an unknown index → `404 index_not_found`, decided by
   **the existence union** — HEAD's names or this process's unfolded ones — rather than by an
   empty result, which is not an error, and rather than by HEAD alone, which would `404` the
   just-written index criterion 2 answers; `top_k: 0` or a query with
   no legs → `400 bad_request`; a backend that cannot fence → refused **at startup**, and on a
   store that turns divergent afterwards `503 storage_unavailable` with `retryable: true`;
   malformed JSON → `400 bad_request`; `"durability": "async"` → `400 unsupported_durability`.
   ⚠️ **The width is per INDEX, not per request.** A four-dimensional write followed by a
   two-dimensional one, each batch consistent with itself, is `400 schema_conflict` — measured
   in code review as accepted, with the short vector then outranking an exact match.
   ⚠️ **A dimension mismatch was deferred here and then earned**: it is `400 schema_conflict`
   on the query path and on the write path both. The spec deferred it because
   `Engine::query` collapsed `FormatError::DimensionMismatch` into `EngineError::Query(String)`
   and a table that string-matches a message is a table no mutation can pin — so the typed
   variant was added rather than the row dropped. ⚠️ **What that uncovered is the more serious
   half**: the dense leg built its index with `query.len()` as the dimension, so it took the
   dimension **from the query** and answered a two-dimensional vector over a four-dimensional
   segment with scored, ranked rows and a `200`. The exact path had always refused it. Fixed
   in `pstore-query`, pinned by a test at that layer as well as through the API.
7. A query's `meta` names `epoch`, `unfolded_hits`, `cost.blob_reads` and `cost.bytes_read`; a
   query served entirely from the memtable reports `blob_reads` of **exactly 1** (HEAD), one
   over a folded segment reports **> 1**, and a second sequential request reports **its own**
   cost rather than the running total.
8. The binary **serves over a real socket**: bound on an ephemeral port, a write and a query
   round-trip over TCP, and `serve` returns `Ok` on a shutdown signal rather than being killed.
9. `cargo deny check` passes with `pstore-server` added to `object_store`'s wrapper list, and
   `axum` appears in no other crate's dependencies.
10. Region coverage ≥95% on `pstore-server`; every mutant in its handlers caught or named
    equivalent with the reason on the line.
11. ⚠️ **Added after code review measured it.** One query's **sequential depth** is asserted
    through the router against the depth-counting store, and is **flat in the segment count**:
    the same number at one segment, four and eight. The number itself is 4 — three for the
    ranking, one for the ids — and it is written down rather than bounded, because what this
    criterion exists to catch is the *slope*. The first implementation was `3 + 2 × segments`,
    which nothing asserted and `blob_reads > 1` happily passed.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1 | `a_batched_write_costs_nothing`, `a_durable_write_costs_its_lane_registration_once_then_one_put` | `durable` ignored, so every write flushes; the reverse, `durable` silently batched, which is a lost write; and a per-lane cost read as a per-write one, which a single-flush test cannot see |
| 2 | `a_write_is_queryable_before_it_is_folded` | the handler reading HEAD's segments only — the freshness layer disconnected, green in every test that folds first |
| 3 | `a_folded_write_is_visible_to_a_second_server` | a fold endpoint returning 200 without folding; a query answering from a memtable the second process does not have |
| 4 | `a_missing_tenant_header_is_refused`, `two_tenants_do_not_see_each_others_documents` | a defaulted tenant (`unwrap_or(TenantId(0))`), which passes every single-tenant test in this file |
| 5 | `no_endpoint_lists` | an enumeration implemented as a LIST over the tenant prefix: correct, obvious, and the thing the architecture forbids |
| 6 | `the_error_table_maps_every_row`, `a_backend_that_cannot_fence_is_refused_before_the_socket_is_bound`, `a_just_written_index_is_not_a_404` | a catch-all `500`, green on the happy path and useless to a client; `retryable` hard-coded; a fencing check only at the flush, which lets a batched write to a divergent backend answer `200`; existence decided on HEAD alone, which is the contradiction spec review round 2 found |
| 7 | `a_memtable_query_reads_head_and_nothing_else`, `cost_is_per_request_not_cumulative` | `cost` reported as a constant, or as the tenant's running total rather than this request's delta |
| 11 | `a_query_costs_four_round_trips_however_many_segments_it_has` | resolution that re-opens each segment serially — depth growing with the segment count, and therefore with elapsed writes, which no cost assertion in this crate could see |
| 8 | `the_server_answers_over_a_real_socket`, `the_server_refuses_to_start_without_a_lane` | a router only ever exercised by `oneshot`, so an extractor or middleware that fails in a real stack is never seen; a defaulted `LaneId(0)`, silent and correct-looking, which overwrites another process's bundles |

⚠️ **Criterion 3 is the one that decides whether this is a database or a cache.** Every other
criterion is satisfiable by a process that holds everything in its own memtable. Only a second
instance reading what the first made durable proves the blob store is the tier.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `PUT documents`, `batched` | **0** | 0 | 0 | 0 |
| `PUT documents`, `durable`, lane's first flush | **2** — bundle + the lane registration CAS | **1** (the lane set) | 0 | 0 |
| `PUT documents`, `durable`, thereafter | **1** (the bundle) | 0 | 0 | 0 |
| `POST query`, memtable only | 0 | **1** (HEAD) | 0 | 0 |
| `POST query`, folded segments | 0 | **4**, flat in the segment count | fan-out within a round | 0 |
| `GET /v1/indexes`, `GET /v1/indexes/{id}` | 0 | **1** (HEAD) | 0 | 0 |
| `POST /admin/fold` | `Engine::fold`, unchanged | unchanged | unchanged | 0 |

⚠️ **Four, not three, and measured rather than assumed.** The *ranking* is three — HEAD, the
open round, the leg round. Carrying the **ids** is a fourth, because a payload's address cannot
be known until the ranking exists. ⚠️ Code review measured the first implementation at
**`3 + 2 × segments`** — 19 sequential round trips at eight segments — because resolution
re-opened every segment serially. It is now one fan-out round over segments the query already
opened, so depth is **flat in the segment count**, which is the property that matters: a depth
growing with segments grows with elapsed writes. Getting to three needs ids fetched alongside
the vectors a leg already reads, which is a decision about what a segment stores; it is a
backlog row, not a thing to decide inside a milestone about transport.

⚠️ **No request scales with documents, indexes, or elapsed time.** The write path's cost is
per *batch*; the read path's is per *query*. That is the property the accounted assertions in
criteria 1, 5 and 7 exist to pin, rather than to describe.

## Risks

- **Two servers on one lane is the failure this milestone can newly cause**, and requiring the
  lane makes it an operator error rather than a default. Nothing detects it — a bundle is an
  unconditional `put`. The backlog row names create-if-absent bundles as the fix and M2's
  density invariant as the reason it is not a one-line change.
- **A registry of engines is a memory leak with a plausible name.** One `Engine` per tenant,
  held forever, is 1M of them at the scale the README claims. M7c holds them unbounded and
  **says so**: the eviction policy is a decision about the memtable's durability contract —
  evicting an engine with unflushed rows discards acknowledged writes — and that argument
  belongs with the schema questions in M7d, not smuggled in behind an LRU.
- **The startup refusal is a capability check, not a probe.** `admits_durable_writes()` reads
  the recorded profile, so a backend whose *recorded* capabilities are fine and whose behaviour
  is not passes the door. That is M7a's `--check` problem and not a new one, and the CAS guards
  underneath are what make it safe rather than merely tidy.
- **The freshness layer makes a single process look like a database it is not.** Criterion 3
  is the defence, and it is the only criterion that cannot be satisfied by accident.
- **JSON vectors are a top-three CPU cost** (D-36). Accepted for v1 by the same document that
  names it; the request shape keeps `vector` an array so a base64 variant can be added without
  moving the field.

## Tasks

| Id | Commit |
|---|---|
| M7c.1 | The crate, the router, the tenant extractor, and the error table |
| M7c.2 | Write and query, with the accounted cost in every response |
| M7c.3 | Enumeration, index metadata, and the fold endpoint |
| M7c.4 | The binary, over a real socket |
| M7c.5 | The ledger, the backlog row it closes, and the roadmap |
