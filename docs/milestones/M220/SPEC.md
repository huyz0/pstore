# M220: the native API as an OpenAPI contract, held against the server

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "a maintained
native OpenAPI ... Define types, defaults, nulls, projections, errors, score direction, missing
values, result envelopes, and limits." D-34's surface ([api-design.md](../../research/11-design/api-design.md)).
The first half of DELIVERY's B row "Native OpenAPI, generated SDKs and semantic fixtures".

## What holds today
- The server's routes are registered in `Api::router`, each under one scope (M127). Request
  bodies are serde structs, almost all `deny_unknown_fields`, so the parser names its fields
  when it refuses one it does not know. Responses are serde structs, or `json!` built in place
  (replication, copy, quarantine).
- Nothing describes the API as a whole outside the code and `docs/api-migrations.md`. A client
  is written by hand from those, and nothing says when the two disagree.

## Delta
- **`api/openapi.json`**, OpenAPI 3.1, version `1.0.0`: every route, method and scope (as each
  operation's bearer scope); each request body's fields, with defaults and the limits the
  server refuses past (`top_k` 10,000, 16 queries, `max_staleness_ms` 3,600,000, `page_size`
  1,000, external `k` 1,000 and `window` 10,000); every success response; the error envelope
  and its 44 codes, each with its status (`x-pstore-status`); the headers `x-pstore-tenant` and
  `x-pstore-session`. Every object is closed (`additionalProperties: false`) except maps.
  Score direction, `$dist` per metric, and missing attributes are stated in the schemas'
  descriptions and pinned by fixtures. Filters, `rank_by`, facets, boosts and other expression
  grammars are typed by name only and point at their milestone.
- **`api/fixtures/native.json`**: 64 ordered steps for one tenant, each an `operationId`, its
  path, query and body, the status it answers, and what the answer must hold. They pin
  semantics a schema cannot: nearest first with `$dist` ascending under `euclidean_squared`, a
  filter, an order with a row missing the attribute last and answering no key for it, two and
  sixteen queries in one request, an applied write's ids, refusals at each limit, read-only,
  copy, replication from pause to cancel. A step marked `invalid` sends a body or query the contract
  must refuse too. The clients (M221, M222) run the same steps.
- **`Api::route_table()`**: the router's `(path, scope)` pairs, recorded as `ScopedRouter`
  registers them. The router itself is unchanged: `router()` is built from the same table.
- **`crates/pstore-server/tests/it/openapi.rs`**, with a validator for exactly the keywords
  the contract may use (`openapi/schema.rs`); any other keyword in the document is refused.

Not changed: any route, request or response. If a test finds the server and the contract
disagree, the contract is corrected, unless the server is wrong; either is named in VERIFIED.

## Acceptance criteria
1. The router's `(path, scope)` pairs equal the contract's, and probing every method of every
   path with a token of only that scope finds exactly the contract's `(method, path, scope)`
   triples (`every_route_is_in_the_contract_under_its_scope`).
2. For each of 13 closed request objects, the field names in the server's unknown-field
   refusal equal the schema's properties, plus the 3 a replication parses and refuses by value;
   every closed object reachable from a request body is probed but `MultiQueryRequest`, whose
   refusal is prose (`request_bodies_name_the_fields_the_server_parses`).
3. The error codes the server's non-peer source constructs as `StatusCode::X, "code"`, each at
   one status, equal the contract's codes and statuses (`error_codes_are_the_servers`).
4. Every fixture step answers its status, its body validates against the operation's schema
   (or the error schema), an error's status is its code's, it holds `expect`, a valid body
   validates against the request schema and an `invalid` one does not. Every operation is
   answered, and every documented 2xx status but 6 named in `UNEXERCISED` with their reason
   (`the_fixtures_hold_against_the_server_and_the_contract`).
5. The document uses only keywords the validator checks, every `$ref` resolves and every
   `operationId` is unique (`the_contract_uses_only_keywords_the_validator_checks`); the
   validator refuses a value each keyword forbids and flags an unchecked keyword
   (`the_validator_refuses_what_each_keyword_forbids`).
6. fmt, clippy, the suite and the coverage floors pass.

## Test plan
Each test is new and passes against the server as it is, because the contract was written to
describe it. Red is shown by breaking the contract, one change at a time, and seeing which test
fails (recorded in VERIFIED):
- drop `explain` from `QueryRequest` → AC 2 red (the parser names a field the contract lacks);
  add `extra` to `Document` → AC 2 red (the other direction).
- add a path → AC 1 red; move `gc_tenant` to `write` → AC 1 red; serve `warm` as GET → AC 1.
- drop `staleness_ms` from `QueryMeta` → AC 4 red (a closed object meets a field it does not
  name); `$dist` typed string → AC 4 red; `maxItems` 17 on `queries` → AC 4 red (the
  seventeen-query step is `invalid` and the contract accepts it).
- `index_exists` at 400, or `forbidden` dropped → AC 3 red.
- a `pattern` keyword → AC 5 red.

## RA budget
Unchanged: no request path changes. The tests run on the in-memory store.

## Risks
- **A prose grammar typed by name** (filters, `rank_by`, facets) is not checked beyond its
  JSON type. The fixtures pin one of each in use; a generated client passes them through.
- **A query parameter the server reads but the contract omits** is invisible: axum ignores
  unknown parameters. Each documented one is used by a fixture step, and the step's value is
  validated against its schema (a refused value marked `invalid`).
- **The error-code scan reads source text.** A code built some other way is missed by both the
  scan and the contract; a fixture answering it would still fail the enum.

## Tasks
- M220.1 `Api::route_table`, the contract, the fixtures and the conformance tests.
