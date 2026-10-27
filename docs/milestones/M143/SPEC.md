# M143: decoded request structure is bounded

Spec review round 1 failed with two blockers (a default below an existing fixture, and
counts checked only after the body was materialized) and four majors (`top_k`'s status and
its tie with existing caps, an engine assertion that could not fail, filters unbounded).
This revision checks shape before any parser runs, generically, so every field and route
is covered by the same four numbers. Round 2 passed with minors; three factual ones are
fixed here.

## Serves
D-55 in [memory management](../../research/09-rust-stack/memory-management.md).
Workstream A ("bound compressed and decoded bytes, JSON nesting, documents, attributes,
vectors, top-k") in [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
M128 bounds a public body's encoded bytes (2 MiB) and M141 a tenant's buffered rows. What
a body decodes into is bounded only by that size: one write may hold 80,000 operations or a
500,000-component vector, a filter an `In` list of 200,000 values or an `Or` of 100,000
clauses, and a document 100,000 attributes. Nesting is stopped only by `serde_json`'s
recursion limit of 128, reported as a 400. Every such body is parsed in full before
anything refuses it, into a `serde_json::Value` tree on some paths (multi-queries, a body
that repeats a key). A vector or text query's `top_k` has no bound at all.

**A shape scan before any parse.** In `ingress::admit`, after the body is collected and
before the handler runs, one pass over the bytes checks four version-1 limits of
`RequestLimits`. It allocates nothing, tracks string and escape state, and stops at the
first breach. It does not validate JSON: a body it passes still goes to the parsers, which
refuse what they refuse today with today's messages.

| Field | Default | Bounds |
|---|---|---|
| `json_depth` | 32 | arrays and objects open at once (`{}` is depth 1, a scalar body 0) |
| `array_elements` | 16,384 | elements of any one JSON array: a write's documents, deletes or patches, a vector's components, a filter's `In` list or `And`/`Or` children, an array attribute |
| `object_members` | 1,024 | members of any one JSON object: a document's attributes, a patch, named vector fields, a schema |
| `string_bytes` | 262,144 | the raw bytes between one string's quotes: an id, a text field, an attribute value or name, a base64 vector |

Each breach is `413 request_too_large`, not retryable, with a message naming the field,
its value and the nesting depth where it was found, for example "an array at depth 2 has
more than 16,384 elements".
A body that is also malformed after the breach reports the breach. Every public route that
admits through `ingress::admit` is covered: native and external writes and queries,
schema, warm, replication, idempotency and admin bodies. The peer protocol is not public
and keeps its frame bound.

The defaults sit above every fixture in the workspace suites (the largest array is
10,000 elements, in `applied_writes.rs` and `allow_partial.rs`), and are evidence-backed by
the measurement below rather than copied from a vendor.

**`top_k`.** A vector, text or hybrid query leg whose `top_k` passes 10,000 is refused
with `400 bad_request`, as `MAX_ORDERED` already refuses aggregate groups, facets and
`rank_by` (`top_k + offset`). Those three keep their own messages; the new check covers
only the legs no cap reaches today. `top_k` is validation, not size, so it is a 400.

Version-1 configuration as M128's: the builder and `PSTORE_REQUEST_LIMITS_CONFIG` lower
each of the four (never raise), refusing zero, above the default, unknown fields and
malformed values before any store, cache or listener. `top_k`'s bound is not configurable.

Unchanged: M128's encoded-body, admission and deadline limits; M141's buffer bounds; the
existing lower caps (16 queries, 8 named fields, 1,024 vectors per named field, 16 patterns,
1,000 boost window) and their 400s; every other refusal; stored data.

Not this slice: query plan byte reservations (D-55's planner budget) remain A work. No
streaming decoder: an admitted body is still parsed whole, and its decoded size is bounded
by the 2 MiB body times the factor the measurement records.

## Acceptance criteria
1. For each of the four fields, with a small configured limit, a body exactly at the limit
   is admitted and one element, member, byte or level more is `413 request_too_large`,
   on a write and on a query. The refusal is sent first, on a fresh `Api`, and leaves
   `Api::engines()` at 0 and the store's request counters at 0; the at-limit body follows.
2. The scan does not count brackets or quotes inside strings, honours `\"` and `\\`
   escapes, and reports the breach even when the body is malformed after it. One breach
   is shown on a route other than write and query.
3. A vector query leg with `top_k` 10,001 is `400 bad_request`; 10,000 is answered. A facet
   `top_k` of 10,001 and a `rank_by` `top_k` of 10,001 keep their existing 400 messages.
4. The four configuration fields are refused when zero, above the default, unknown or
   malformed, before the store configuration is read, as M128's are.
5. The existing server and workspace suites, cost/depth gates and coverage floors pass at
   the defaults.
6. Measured in a release benchmark with `stats_alloc`, provisional: a refused 2 MiB body of
   each breaching shape (wide array, wide object, long string, deep nesting), sent as one
   chunk, allocates at most 64 KiB from the end of body collection to the end of the
   response. Recorded beside it: the allocation
   of an admitted 2 MiB write at the defaults, as a factor of its body size.

## Test plan
1. `shape_limits_are_exact`: per field, the refusal first on a fresh `Api`, then the
   at-limit body, asserting status, `error.code`, message, `engines()` and request
   counters. Mutations caught: `>=` for `>`, a field not checked, the scan after the handler.
2. Unit tests of the scan: depth of `{}` and of a scalar, brackets and quotes inside
   strings, `\"` and `\\` escapes, an unterminated string, a breach before malformed bytes.
   Mutations caught: counting inside strings, an off-by-one, ignoring an escape.
3. `query_top_k_is_bounded`, and `existing_caps_keep_their_refusals` (facet, `rank_by`,
   17 queries, 9 named fields at the defaults).
4. The configuration unit tests extended with each field, and the M128 binary-ordering
   test given one invalid new field.
5. `scripts/gates.sh`, recall, NDCG and depth scripts, `scripts/coverage.sh`, and the
   ignored benchmark `refused_shapes_allocate_little`, which asserts criterion 6's bound.

## RA budget
0 W / 0 Rseq / 0 Rpar / 0 LIST added. The scan is one pass over bytes already collected and
runs before the handler, so a refusal constructs no engine and reads nothing.

## Risks
A body with arrays of more than 16,384 elements, objects of more than 1,024 members,
strings over 256 KiB or nesting deeper than 32 now gets 413 and must change; a malformed
body that is also too deep changes from 400 to 413; a query leg asking for more than
10,000 rows gets 400. All of these need a migration entry. The counts bound structure, not
RSS, and nothing here is called a memory bound.

## Tasks
- M143.1 Implement the shape scan, the `top_k` bound and the configuration, with tests, a
  migration entry and evidence.
