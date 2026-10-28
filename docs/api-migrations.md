# Native API migration notes

## M171: UUID and bytes attributes

A write's `schema` may declare an attribute `"uuid"` or `"bytes"` beside `"datetime"`:

```json
{"schema": {"owner": "uuid", "thumb": "bytes"},
 "documents": [{"id": "a", "attributes": {"owner": "6F1C2A3B-4D5E-4F60-8A7B-9C8D7E6F5A4B",
                                          "thumb": "AAE="}}]}
```

- A UUID is 8-4-4-4-12 hex digits of either case, stored as 16 bytes and answered
  lowercase. Bytes are standard padded base64, at most 4,096 bytes, answered the same way.
  An array of either is declared the same way. A malformed value is refused, nothing stored.
- Filters, boosts and `$ref_new` conditions read a string literal as the row's type: a
  literal that does not parse matches no such row (and its negation every one). UUIDs
  compare by their bytes, bytes bytewise. `rank_by` orders the groups bool, number,
  datetime, string, UUID, bytes, array, absent.
- Segments holding these values carry two new value tags. A reader older than M171
  refuses such a segment rather than misreading it: upgrade readers first.

A write without these declarations is unchanged. No stored-data migration is needed.
The [M171 verification](milestones/M171/VERIFIED.md) records the evidence.

## M170: documents with no vector

A document may omit `vector` (or send `null`): `{"id": "a", "text": "...", "attributes": {...}}`.
Such documents are filtered, ranked by `text`, ordered by `rank_by`, projected, patched and
paged exactly as documents with vectors are.

- An index holds documents with vectors or documents without, never both: the first fold
  decides, as it decides the width today. The other kind answers `400 schema_conflict`, and
  so does a batch that mixes them.
- A query with `vector` or `multi` against an index of vector-free documents answers
  `400 schema_conflict` ("index t has no vectors").
- Refused with `bad_request`: `"vector": []`, `vectors` (named fields) on a document with
  no `vector`, and `distance_metric` on a write whose documents carry no vector.

Every request valid before answers as before. No stored-data migration is needed.
The [M170 verification](milestones/M170/VERIFIED.md) records the evidence.

## M146: a hard per-group limit on a relevance ranking

A relevance query may carry `limit_per`:

```json
{"vector": [...], "top_k": 10,
 "limit_per": {"attributes": ["brand"], "limit": 2, "window": 200}}
```

- At most `limit` rows share one group: the tuple of the row's values for `attributes`
  (1 to 8 names). An absent attribute is a group value; `1` and `1.0` are one value.
- The engine ranks `window` candidates (default twice `top_k`, from 100 to 1,000, shared
  with a boost's window). Rows past the window are never seen, so a few dominant groups
  can leave fewer than `top_k` rows; widen the window up to 1,000.
- The cut follows a boost's rescoring and reads stored attributes, whatever
  `include_attributes` selects.
- Refused beside `rank_by`, `aggregate_by` and `diversify`, and with `top_k` above 1,000.
- In a multi-query each query is cut on its own; an outer `rerank_by` does not re-apply
  the limit to the merged list.
- Vendor differences: the field is a top-level `limit_per` rather than `limit.per`, and
  `limit: 0` is refused.

Queries without `limit_per` answer as before. The
[M146 verification](milestones/M146/VERIFIED.md) records the evidence.

## M142: outer RRF over a multi-query

A multi-query's root may carry `rerank_by`, `limit` and `offset` beside `queries`:

```json
{"queries": [...], "rerank_by": ["RRF", {"rank_constant": 60, "weights": [1, 2]}],
 "limit": 10, "offset": 0}
```

- Each query runs as before. A document's fused score is the sum of
  `weight / (rank_constant + rank)` over the queries that returned it, with ranks counted
  from 1 in each query's answer. A document appears once.
- Rows are ordered by fused score, descending, then by id, ascending. Then `offset` rows
  are skipped and `limit` kept. `limit` may also be `{"total": n}`.
- `rank_constant` is an integer of at least 1, default 60. `weights` gives one number per
  query, each greater than 0 and at most 1,000,000, default 1.
- The answer's `results` holds one list, the fused one. Each row carries `id`, `score`,
  and the attributes of the first query that returned it with attributes. `meta` keeps one
  entry per query.
- Refused with `bad_request`: a model reranker object (not offered), malformed RRF
  parameters, and `limit` or `offset` without `rerank_by`.

A multi-query without `rerank_by` answers as before. No stored-data migration is needed.
The [M142 verification](milestones/M142/VERIFIED.md) records the evidence.

## M140: order by up to eight attributes

`rank_by` also accepts a list of 1 to 8 `[attribute, "asc" | "desc"]` pairs:
`[["a", "asc"], ["b", "desc"]]`. Rows are ordered by the first pair; rows it ties, by the
second; and so on; rows every pair ties, by id ascending. Each pair keeps the one-key
rules: types in their groups, absent last in both directions, arrays tied.

- `id` may be a key, but only the last one.
- Refused with `bad_request`: an empty list, more than 8 pairs, an attribute named twice,
  a pair after `id`, and a malformed pair.
- The one-pair form `["a", "asc"]` and every one-key answer are unchanged. A list of one
  pair answers as that pair does.
- A multi-key order pages by `offset`, within the 10,000 bound; there is no cursor for it.

No stored-data migration is needed. The [M140 verification](milestones/M140/VERIFIED.md)
records the evidence.

## M139: affected ids for by-filter operations

At `PSTORE_APPLIED_WRITES=5` (every node at M139; it includes levels 1 to 4),
`affected_ids` is served beside `delete_by_filter` and `patch_by_filter`. The ids are the
rows each changed, in `applied.ids`, as for per-id operations.

- Each by-filter operation of such a write is bounded at 10,000 minus the distinct ids
  the write names, below M136's 100,000. Past it a plain operation is refused whole
  (`rejected`); with `allow_partial` (M137) each pass lists the ids it changed, and the
  passes together list every row.
- A write naming 10,000 ids beside a by-filter operation is refused with `bad_request`.
- Below level 5 `affected_ids` beside a by-filter operation is refused with
  `bad_request`, as before.
- Rolling back: lower the level below 5 first; an older fold ignores the bound and could
  list up to 100,000 ids.

## M137: `allow_partial` by-filter writes

A write may set `delete_by_filter_allow_partial: true` or
`patch_by_filter_allow_partial: true` beside its `delete_by_filter` or
`patch_by_filter`. Past M136's bound of 100,000 rows the operation is no longer refused:
it changes at most 100,000 rows, and the applied answer says `"rows_remaining": true`.
**Send the same write again to resume**; repeat until `rows_remaining` is absent.

- A partial patch counts only rows it would change, so rows an earlier pass already
  patched are not taken again. Which rows a pass takes is not specified.
- Progress is promised only when the write's other operations do not make rows match the
  filter. A write that also upserts more than the bound of matching rows each time can
  re-change the same rows; split it.
- A patched row whose new value a width check sets aside counts in `rejected`, and stays
  for the next pass to take again. **A pass that changes nothing and rejects rows will do
  so again**: stop and read the rejected rows (the index's quarantine) rather than loop.
- `rows_remaining` is per write: a write with both partial operations gets one flag.
- Needs `durability: "applied"` (else `bad_request`), its by-filter operation (else
  `bad_request`), and `PSTORE_APPLIED_WRITES=4` on every node (else
  `unsupported_durability`). Levels 1 to 3 keep their meaning; 4 includes them.
- Under an idempotency key a resend is a replay of the first pass, with its
  `rows_remaining`: each pass needs a new key. On `outcome_unavailable` or
  `idempotency_partial`, whether rows remain is unknown; send it again.
- Rolling back: a node older than M137 cannot read an outcome object with the new
  remaining section, and its folds of keyed writes fail while one is listed. Lower the
  level below 4 everywhere, wait for 32 commits that tally outcomes, then roll back.

## M136: a by-filter operation is bounded

⚠️ **Behaviour change.** A `delete_by_filter` or `patch_by_filter` whose filter admits
more than 100,000 rows is now refused whole when it is folded: it changes no row. Before,
it applied to every row.

- The rows are counted over the index as the fold reads it, and again at the operation's
  place in its fold, after the operations folded before it. More than 100,000 in either
  count refuses it.
- Sent with `durability: "applied"`, the refusal shows as `rejected: 1` in `applied`.
  Sent `durable` or `batched`, **nobody is told**, and the fold moves past it.
- To change more rows, split the write by narrower filters, or wait for M137's
  `allow_partial`, which applies the first rows and says what remains.

Other operations of the same write apply as before. Nothing new is stored, and the
request shape is unchanged. Rolling back restores the unbounded behaviour.

## M133: `$ref_new` in write conditions

In `upsert_condition` and `patch_condition`, the value of `Eq`, `NotEq`, `Lt`, `Lte`, `Gt`
or `Gte` may be `{"$ref_new": "<attribute>"}`: that attribute's value in the operation's
own new row. "Newer version wins" is `["v", "Lt", {"$ref_new": "v"}]`.

- An upsert's reference is its document's attribute; a patch's is the value it sets. An
  attribute the patch unsets or does not name is absent. `{"$ref_new": "id"}` is the id.
- An absent reference: `Eq` means the row lacks the attribute, `NotEq` that it has it, and
  `Lt`, `Lte`, `Gt` and `Gte` are false. An upsert of a missing id still applies (M13).
- Refused with `bad_request`: a reference in `delete_condition`, in a by-filter operation
  or a query filter, under any other operator, beside a `Glob`, `IGlob`, `Regex`, `Fuzzy`
  or token clause, naming a reserved attribute, or resolving to an array.

Nothing new is stored: each operation carries its own resolved condition, as M13's
operations already carried theirs. Rolling back is safe at any time; an older server
refuses `$ref_new` with `bad_request`, and folds what this one buffered.

## M132: idempotency keys for applied writes

An applied write may carry `"idempotency_key"`. Of all the attempts that carry one key,
at most one applies. Get a key from `POST /v1/indexes/{index}/idempotency-keys` (write
scope), which answers `{"idempotency_key": "<epoch>-<32 hex>"}` and costs no blob request
while the server's view of HEAD is fresh. Keys are per tenant, whichever index's route
issued them. Send the same key again, with the same body, when a response is lost:

- the first attempt to be folded applies, and answers as any applied write does;
- a later attempt applies nothing. It answers `200` with `"replayed": true` and the
  winner's `applied` counts. If those counts can no longer be read, `applied` is `null`
  and `outcome_unavailable` is `true`. The body is not compared, so a retry must resend the
  same write;
- `409 idempotency_expired`: the fold could not prove that no earlier attempt applied,
  because HEAD's list of the last 32 commits that decided applied writes no longer reaches
  back to the key. Nothing of this write applied. Get a new key and send it again;
- `409 idempotency_partial`: the write was, or may have been, applied, in whole or in
  part, and the records that would say which are no longer listed. That happens when part
  of it was refused as expired, or when 32 other commits landed on a busy tenant before
  its answer was read (M134: even when it applied in full). Read the rows before sending
  it again.

Retention is 32 commits that decided applied writes on the tenant, not a duration, so a
key can expire within seconds on a busy tenant. It is refused with `bad_request` beside
`batched` or `durable`, when it is malformed, or when its epoch is ahead of HEAD. It is
refused with `unsupported_durability`, as is the issuing route, unless every node runs
this release and is started with `PSTORE_APPLIED_WRITES=3`. Level 3 adds `keys_since` to
HEAD.

To roll back, do it in this order: stop sending keys, let every keyed write be folded
(answered), and only then downgrade. An older fold applies every attempt of a key it finds
in a bundle. See the [proof](research/11-design/mutation-completion.md), §9.

## M131: one snapshot per request

The sub-queries of a multi-query, and a query's `facets` beside its hits, now answer from
one snapshot: one HEAD and one view of the unfolded rows, taken before any of them runs.
Every epoch in `meta.epochs` is the same, and `meta.facets_epoch` equals `meta.epoch`. A
request reads HEAD once instead of once per leg.

This applies when every leg has the same consistency (`session` counts as the level it
maps to; two `bounded` legs need the same `max_staleness_ms`) and none has `as_of`. Any
other request runs its legs separately, as before. If a `bounded` request's cached HEAD
names a segment that has since been reaped, the whole request is retried once from a
fresh HEAD; such a retry now also covers an `index_not_found` from a stale cached HEAD.

## M130: `affected_ids`, and the order inside one write

An applied write may add `"affected_ids": true`. Its `applied` object then carries
`ids: {upserted, deleted, patched}`: for each kind, the distinct ids an operation of that
kind changed, sorted, over every commit that folded the write. Counts stay per operation,
so `x` upserted twice is `upserted: 2` with `["x"]`, and an id upserted then deleted is in
both lists. A row the fold set aside is in no list.

It is refused with `bad_request` beside `batched` or `durable`, beside `patch_by_filter` or
`delete_by_filter`, or past 10,000 distinct ids, and with `unsupported_durability` unless
every node runs this release and is started with `PSTORE_APPLIED_WRITES=2` (`1` serves
applied writes without ids, as M129 did).

Order, now documented: inside one write, upserts apply first, then patches, then deletes,
then `patch_by_filter`, then `delete_by_filter`, even when the write is split across two
bundles. Between writes the order is unspecified unless the first was folded before the
second was sent: a `durable` answer is not enough, because a fold applies lanes in lane order,
not arrival order. Send the second after the first is answered with `durability: "applied"`.

## M129: `durability: "applied"`

A write may ask for `"applied"`: it is made durable, then answered only once a commit has
folded every operation in it, with what the fold decided. The response adds
`completion: "applied"`, `operation` (32 hex digits) and `applied` with `upserted`,
`deleted` and `patched` (rows changed), `skipped` (operations that changed nothing) and
`rejected` (operations a schema or width pass set aside, or, since M136, a by-filter
operation past its row bound). If an outcome can no longer be
read, `applied` is `null` and `outcome_unavailable` is `true`; counts are never invented.

It is refused with `unsupported_durability` unless every node runs this release and the
operator starts them with `PSTORE_APPLIED_WRITES=1`. `batched` and `durable` are
unchanged, and their `documents_*` counters still count what was submitted. An applied
write waits for a fold, so it is slower; it is not exactly-once, and a retry after a lost
response may apply again (M132's idempotency keys close that). See the [proof](research/11-design/mutation-completion.md).

## M128: public request bodies and active requests are bounded

⚠️ **Behaviour change.** Every public route now admits a request before its handler runs,
after authentication and the route's scope check:

- A body larger than 2 MiB is refused with `413 request_too_large`, whether or not it
  declares a `Content-Length`. Before, axum's own 2 MB ceiling answered with an untyped
  plain-text body.
- A body that has not fully arrived 30 s after admission is refused with
  `408 request_timeout`. **This deadline is new**: a client that streamed a slow body
  must send it faster or split the request.
- A declared `Content-Encoding` other than `identity` is refused with
  `415 unsupported_content_encoding`. Decompression is unsupported; send bodies plain.
- At 64 active public requests in the process, 8 for one tenant, or 128 MiB of reserved
  body bytes, a request is refused with `429 overloaded` and `Retry-After: 1`. It is refused
  before dispatch, so it changed nothing; retrying it is safe. The hint is local to this
  server, not a fleet reservation.

A tenant is the verified bearer's. In private mode (`PSTORE_ALLOW_UNAUTHENTICATED=1`) it is
the `X-Pstore-Tenant` header, and requests without one share one private allowance.
`PSTORE_REQUEST_LIMITS_CONFIG` (strict JSON, `{"version":1, ...}`, fields `body_bytes`,
`active_requests`, `tenant_active_requests`, `reserved_body_bytes`,
`body_read_timeout_ms`) can lower any of these, never raise them; an invalid value refuses
startup. Peer traffic keeps its own admission (M127). Nothing stored changes, and rolling
back restores the old ceilings. Evidence is in [M128 verification](milestones/M128/VERIFIED.md).

## M127: explicit authentication and encrypted peers

Standalone startup now requires `PSTORE_AUTH_CONFIG`, or the explicit
`PSTORE_ALLOW_UNAUTHENTICATED=1` private/test mode. Configure existing deployments
before updating the server. The two modes cannot be combined. Rust embeddings
choose authenticated constructors or the explicitly unsecured constructors;
bare `Api::new` and `with_cache` expose closed HTTP surfaces.

Public requests use a signed bearer token containing the tenant and explicit
operation scopes. An optional tenant header must agree with that identity.
Branch/copy/replication source access also requires read authority for the exact
source tenant/store pair. See the [authentication contract](milestones/M127/AUTH.md)
for claims, grants and key rotation. Accepted durable jobs continue until cancelled.

The public `/v1/internal/part` route is removed, including in private mode.
Configure a dedicated peer TCP/UDP address, deployment peer CA, certificate and
key. Static/gossip URLs now use HTTPS and name the peer listener. Both HTTP/2
and HTTP/3 carry versioned MessagePack over mTLS 1.3; `PSTORE_PEER_TRANSPORT`
selects outgoing `h2`, `h3` or `auto`. Public bearer tokens do not authorize peers.
Legacy peer builds cannot speak this protocol and cause local fallback during
an upgrade; plan configuration and capacity for that transition.

Public HTTP still needs trusted TLS ingress. This change does not add fleet
quotas or change native mutation/durability/read defaults. Current evidence and
remaining qualification are in [M127 verification](milestones/M127/VERIFIED.md).

## M126: unknown request fields are errors

Write/query request roots, document envelopes, and multi-query children reject unknown
keys with HTTP 400 and `error.code: "bad_request"`. The error message names the field.
Previously these envelopes could ignore an extra key, so an unsupported option or typo
could appear to succeed. Existing strict nested options keep their rejection behavior.

Remove unsupported options or correct the named key before retrying. For example,
`durabilty` is an error; the supported field is `durability`, with `"batched"` or
`"durable"`. Vendor-only options such as `return_affected_ids`, `disable_backpressure`,
`compute_attributes`, and `limit` are not accepted by this native API version.
Use `top_k` to set the native query result limit.
Use the [comparison](comparisons/turbopuffer-2026-10-25.md) for the pinned capability gaps
and [delivery status](comparisons/DELIVERY.md) for work in progress.

User data maps remain open. Attribute names under `attributes`, named vector fields,
patch columns, and aggregate labels do not become reserved merely because they resemble
request option names. Known duplicate keys retain the existing last-value behavior.

Invalid requests are validated before engine access and cannot partially buffer or
flush their valid rows. No stored-data migration is needed. Native write/read defaults
remain batched/eventual; this change does not add authentication or change mutation
completion guarantees. The [M126 verification ledger](milestones/M126/VERIFIED.md)
records the evidence and any checks still pending.
