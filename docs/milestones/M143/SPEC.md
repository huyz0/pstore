# M143: decoded request structure is bounded

Draft, not yet reviewed.

## Serves
D-55 in [memory management](../../research/09-rust-stack/memory-management.md).
Workstream A ("bound compressed and decoded bytes, JSON nesting, documents, attributes,
vectors, top-k") in [the delivery plan](../../comparisons/turbopuffer-next-plan.md).

## Delta
M128 bounds a public body's encoded bytes (2 MiB) and M141 a tenant's buffered rows. What
a body decodes into is not bounded except by that size: one write may hold 80,000
operations, a 1 MB id, a 500,000-component vector or 100,000 attributes; a query may ask
for any `top_k`; and nesting is stopped only by `serde_json`'s recursion limit of 128,
reported as a 400 like any malformed body.

Add version-1 structure limits to `RequestLimits`, lowered (never raised) by the builder
and `PSTORE_REQUEST_LIMITS_CONFIG` with M128's refusals (zero, above the default, unknown
field, malformed). Defaults:

| Field | Default | Counts |
|---|---|---|
| `json_depth` | 32 | arrays and objects open at once, anywhere in a public JSON body |
| `write_operations` | 10,000 | a write's documents + patches + deletes; a by-filter operation is one |
| `id_bytes` | 1,024 | the UTF-8 bytes of any id a write or query names |
| `vector_dimensions` | 4,096 | components of any one dense vector, written or queried |
| `document_attributes` | 256 | attributes of one document or patch (`set` plus `unset`) |
| `attribute_bytes` | 65,536 | the UTF-8 bytes of one string value, or of all elements of one array |
| `array_elements` | 1,024 | elements of one array attribute |
| `top_k` | 10,000 | rows any one query leg, aggregate group list or facet asks for |

Defaults are chosen so every fixture in the workspace suites passes unchanged, and are
evidence, not a vendor's ceiling: the measurement below records decoded bytes at each.

**Where each is checked.** Depth by a byte scan of the collected body before any JSON
parser runs: one pass, no allocation, tracking string and escape state so brackets inside
strings do not count. The others after the typed decode and before the engine is
constructed or any blob is read, in the existing door checks (`scalar`, the write and
query validators), so a refusal mutates nothing and issues no request.

**Refusal.** Each is `413 request_too_large` with a message naming the field and its
value, for example "the write has 10,001 operations; at most 10,000". It is not retryable.
A depth over the limit is refused before parsing, so a body that is also malformed reports
its depth, not its syntax.

Unchanged: M128's encoded-body, admission and deadline limits; M141's buffer bounds;
existing per-feature caps that are lower (16 queries, 8 named fields, 1,024 vectors per
named field, 1,000 boost window, 10,000 ordered rows, 16 aggregates); every other refusal.
The peer protocol is not public and keeps its own frame bound.

Not this slice: query plan byte reservations (D-55's planner budget) remain A work, and no
streaming decoder is added: decoded memory is bounded by these counts and the 2 MiB body,
and the measurement states by how much.

## Acceptance criteria
1. For each field, a request exactly at the configured limit succeeds and one item (or
   byte) more is `413 request_too_large`, with the store's request counters unchanged and
   no engine constructed for the tenant. Shown with small configured limits on writes and
   on queries.
2. A body nested one level past `json_depth` is refused before parsing, including when it
   is also malformed after that point; brackets inside strings, and escaped quotes, are
   not counted; exactly at the depth is accepted by the scan.
3. The new configuration fields are refused when zero, above the default, unknown or
   malformed, before any store, cache or listener, as M128's are; defaults are reported by
   `RequestLimits::default()` and documented in the migration entry.
4. Lower existing caps still answer with their existing refusal where they bind first.
5. The existing server and workspace suites, cost/depth gates and coverage floors pass.
   Record, provisional: peak decoded allocation for a 2 MiB write at the default limits
   for small rows, wide vectors and wide attributes, against the body's size.

## Test plan
1. `write_structure_limits_are_exact`, `query_structure_limits_are_exact`: per field,
   at and one past a small configured limit, asserting status, `error.code`, message,
   request counters and `Api::engines()` unchanged. Mutations caught: `>=` for `>`, a field
   not checked, a check after the engine is built.
2. Unit tests of the depth scan: exactly at depth, one past, brackets and `\"` inside
   strings, an unterminated string. Mutations caught: counting brackets in strings, an
   off-by-one, ignoring escapes.
3. Configuration unit tests extended with each new field; the M128 binary-ordering test
   gains one invalid new field. Mutations caught: a field read but not validated.
4. `lower_caps_keep_their_refusals`: a 17-query multi-query and a 9-field write keep their
   current 400s at default limits.
5. `scripts/gates.sh`, recall, NDCG and depth scripts, `scripts/coverage.sh`, and an
   ignored release benchmark `decoded_bytes_at_the_limits` with `stats_alloc`.

## RA budget
0 W / 0 Rseq / 0 Rpar / 0 LIST added: every check is in memory before the engine. A refusal
constructs no engine and changes no buffered state. The depth scan is one pass over bytes
already collected.

## Risks
A client that sends more than 10,000 operations, or wider vectors, in one write now gets
413 and must split; that is a behaviour change and needs a migration entry. Counts bound
structure, not RSS: the measurement states the decoded factor, and nothing here is called
a memory bound. Limits applied to query ids and vectors must not break existing fixture
sizes; criterion 5 covers it.

## Tasks
- M143.1 Implement the depth scan and the structure limits, with configuration, tests, a
  migration entry and evidence.
