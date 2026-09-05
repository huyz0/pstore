# Session Protocol: Read-Your-Writes and Warm Routing in One Token

**Answers:** Q38
**Status:** Complete (v1)

## 1. Two problems, one mechanism

| Problem | Symptom |
|---|---|
| **Read-your-writes** | Client writes, immediately queries, doesn't see its own write |
| **Cold cache** | Client's query lands on a node with nothing warm for that index — 400 ms instead of 10 ms |

Both are "the follow-up request should go somewhere that already knows about me." A single
opaque **session token**, minted on write and echoed on read, solves both. This is the design's
neatest consolidation: consistency and cache affinity turn out to be the same routing problem.

## 2. Prior art

- **Azure Cosmos DB session consistency** — the closest model. *"After every write operation the
  client receives an updated Session Token from the server; the client caches the tokens and
  sends them to the server for read operations."* Session is Cosmos's **default** consistency
  level, and it is explicitly *client-centric*, guaranteeing read-your-writes and
  write-follows-reads within a session.
- **MongoDB causal consistency** — the client session tracks an ever-increasing `clusterTime`,
  and a query passes `afterClusterTime` asking the server *"to return data including all writes
  up to that clusterTime."*

Both validate the shape: an opaque, monotonic, client-held token, exchanged on every request.
Neither uses it for **routing**, which is where we add something.

## 3. The token

```
pstore_session := base64( version ‖ tenant_id ‖ [ (index_id, epoch, lane_watermark) ] ‖ hint ‖ mac )
```

| Field | Purpose |
|---|---|
| `epoch`, `lane_watermark` | The consistency half: "serve me at ≥ this state" |
| `hint` | The affinity half: node ids that were warm for these indexes |
| `mac` | Prevents forgery — an unsigned token is a DoS vector (§6) |

**Opaque to the client.** It is transported, never parsed. That keeps epochs, lanes, and node
ids as internal concepts we can change.

### Transport
```
HTTP:  X-Pstore-Session: <token>       (request and response)
gRPC:  pstore-session metadata key
```
A header, not a body field, so an LB or proxy could route on it later without parsing bodies,
and so it works uniformly for reads and writes.

### Size control at 50 indexes per tenant
A naive per-index vector for a tenant touching all 50 indexes gets large. Three measures:
- Entries are **LRU-capped** (default 8); overflow degrades that index to `bounded` reads.
- Since the **tenant is the commit unit** ([`../10-benchmarks-cost/tenancy-scale-model.md`](../10-benchmarks-cost/tenancy-scale-model.md) §4),
  one tenant-level epoch usually covers all 50 indexes — the common case is **one entry, not
  fifty**. The per-index form is only needed for indexes promoted to their own HEAD.
- Hard cap on token size (e.g. 1 KiB); beyond it the server returns a truncated token and says
  so.

## 4. The protocol

### Write
```
PUT /v1/indexes/acme_docs/documents        X-Pstore-Session: <prior token, optional>
→ 200  X-Pstore-Session: <new token>
   { "epoch": 41822, "read_token": "...", ... }
```
The token is returned in both the header and the body (`read_token`) so SDKs can be transparent
and raw HTTP users can be explicit.

### Read
```
POST /v1/indexes/acme_docs/query           X-Pstore-Session: <token>
{ "consistency": { "mode": "session" }, ... }
```

Server logic on the receiving node:

```
1. Verify the MAC. Invalid → ignore the token, serve as `bounded`, warn.
2. Extract (epoch, watermark) for this index.
3. Am I at ≥ that state?
     yes → serve.                                    [common case]
     no  → am I a placement for this index?
             yes → refresh (1 conditional GET of HEAD, usually a 304) then serve.
             no  → forward to a node from `hint`, else to a placement.
4. Return a token >= the one received (monotonic reads, never go backwards).
```

**The affinity is free.** Step 3's forward already exists for cache locality
([`../04-cluster/routing-and-placement.md`](../04-cluster/routing-and-placement.md)); the token
just makes the target *better informed*. And because the write was routed to the index's read
placements to populate the memtable
([`../05-storage-engine/batching-and-visibility.md`](../05-storage-engine/batching-and-visibility.md) §5),
**the nodes named in `hint` are exactly the nodes holding the fresh data**. Consistency and
warmth are satisfied by the same hop.

> **D-69.** `session` becomes the **default** consistency mode, as it is in Cosmos DB. It gives
> read-your-writes and monotonic reads at `bounded` cost (0 blob requests in the common case)
> and improves cache hit rate as a side effect. `strong` remains available for
> cross-session-visibility requirements; `bounded` for explicitly stale reads.

This supersedes the earlier default of `strong` in
[`../03-metadata-consistency/consistency-model.md`](../03-metadata-consistency/consistency-model.md):
`strong` costs a blob round trip on every query to guarantee something most callers do not
need (visibility of *other* clients' writes). `session` is the honest default.

## 5. Affinity is best-effort, always

> **D-70.** The `hint` is advisory. A node that is not in the hint may serve the request. Losing
> or ignoring the hint costs a cache miss, never correctness — the `(epoch, watermark)` half
> carries the correctness, and any node can satisfy it by refreshing.

This preserves the property that makes the fleet elastic: no node owns anything, no sticky
session must survive a deploy, and the external load balancer stays a plain round-robin with no
affinity configuration.

Hints go stale (nodes leave, placement changes). Staleness is self-correcting: forwarding to a
departed node fails fast and falls back to a computed placement, and the response carries a
refreshed hint.

## 6. Hazards

| Hazard | Mitigation |
|---|---|
| **Forged token demanding a far-future epoch** → the node waits forever, a free DoS | MAC the token; **and** clamp: never wait more than `max_wait_ms`, then serve with `partial`/staleness reported |
| Token references a **GC'd epoch** (client idle for hours) | Epoch below the retention floor ⇒ serve at current state, which trivially satisfies "≥". Never an error. |
| Token pins a client to a **dead node** | Hint is advisory; fall back to computed placement |
| **Hot-spotting** — a heavy client's hints keep sending it to one node | Hints name all R placements, and CHBL overload skipping applies |
| Client shares a token across users | It is per-session and opaque; it grants no authority (auth is separate). Document that it is not a credential. |
| Token grows with indexes touched | LRU cap + tenant-level epoch (§3) |
| Clock/epoch confusion across regions | Tokens are region-scoped; a token from another region is ignored with a warning |

## 7. What the SDK must do

Most of the value is in the client, and getting it wrong is easy:

1. Hold the token per logical session, not per process (a shared process serving many end users
   must not merge their sessions — that would silently strengthen consistency and destroy
   affinity).
2. Send it on **every** request, read and write.
3. **Merge monotonically** — take the maximum on concurrent responses; never move backwards.
4. Expose an explicit "start a new session" call, and support serializing a token so it can be
   handed between processes (a write in a job, a read in a web request — the common RAG
   pattern).
5. Never parse it.

> **D-71.** Session semantics live in the SDK. Raw-HTTP users get the same guarantees by
> echoing one header, and the documentation must show that path first — most integrations start
> with `curl`.

## 8. Interaction with the freshness layer

The token's `watermark` is satisfied from the **in-memory memtable** at the placement nodes, so
the common read-your-writes case costs:

| | Blob requests | Latency |
|---|---|---|
| `session` read, hint hit | **0** | ~1 ms |
| `session` read, hint miss but placement | 0–1 (conditional GET, usually 304) | ~1–30 ms |
| `session` read, cold node | 1 + forward | ~1 ms + hop |
| `strong` read | 1–2 | ~10–30 ms |

Read-your-writes at ~1 ms with zero blob requests is a strictly better product than a 10 ms
floor, and it falls out of mechanisms we already built for other reasons.

## 9. Open questions raised

- OQ-121 — Should the token carry a **tenant-level** epoch only, with per-index entries as the
  exception? §3 argues yes; confirm against how applications actually mix indexes.
- OQ-122 — `max_wait_ms` default when a node is behind the token: wait, forward, or serve stale
  with a flag? Leaning forward-then-serve-stale.
- OQ-123 — Should hints be signed separately so a proxy could route on them without holding the
  MAC key?
- OQ-124 — Cross-region sessions: out of scope for v1 (single-region deployments), but the
  token format should reserve space rather than need a version bump later.
- OQ-125 — Does `session` as default surprise anyone who expects `strong`? Naming and
  documentation matter more than the mechanism here.

## Sources

- [Consistency level choices — Azure Cosmos DB (session tokens, default level)](https://learn.microsoft.com/en-us/azure/cosmos-db/consistency-levels)
- [Manage Consistency — Azure Cosmos DB (session token handling)](https://learn.microsoft.com/en-us/azure/cosmos-db/how-to-manage-consistency)
- [How To Use MongoDB Causal Consistency — A. Jesse Jiryu Davis](https://emptysqua.re/blog/how-to-use-mongodb-causal-consistency/)
- [Mapping consistency levels for Azure Cosmos DB for MongoDB — Microsoft Learn](https://learn.microsoft.com/en-us/azure/cosmos-db/mongodb/consistency-mapping)
- [turbopuffer — Concepts (strong vs eventual consistency)](https://turbopuffer.com/docs/concepts)
