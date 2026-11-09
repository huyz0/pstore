# M220 verification

Implemented in:
- `api/openapi.json`: the contract, OpenAPI 3.1, version `1.0.0`; 30 operations on 26
  `(path, scope)` pairs, 44 error codes.
- `api/fixtures/native.json`: 64 ordered steps for one tenant.
- `crates/pstore-server/src/auth.rs`, `lib.rs`: `ScopedRouter` records each `(path, scope)`;
  `Api::route_table()` answers them. `router()` is built from the same `routes()`.
- `crates/pstore-server/tests/it/openapi.rs` and `openapi/schema.rs`: the conformance tests
  and the validator.
- `docs/api-migrations.md` "M220" and DELIVERY row B.

All runs were in a 4-core cloud container on the in-memory store. No duration is claimed.

How to run: `cargo test -p pstore-server --test it openapi::`

**Red first.** The tests describe a server that already exists, so they pass against it. Each
was turned red by breaking the contract, one change at a time, applied and reverted by script
(.harness/m220-contract-breaks.log). All 11 breaks failed at least one test:
- `explain` dropped from `QueryRequest`, and `extra` added to `Document`:
  `request_bodies_name_the_fields_the_server_parses` red.
- a path added: `every_route_is_in_the_contract_under_its_scope` red (and the keyword test, for
  the repeated operationId); `gc_tenant` moved to scope `write`: route test red; `warm` served
  as GET: route test and fixtures red.
- `staleness_ms` dropped from `QueryMeta`, `$dist` typed string, and `maxItems` 17 on
  `queries`: `the_fixtures_hold_against_the_server_and_the_contract` red (the last because the
  seventeen-query step is `invalid` and the contract then accepted it).
- `index_exists` at 400, and `forbidden` dropped: `error_codes_are_the_servers` red.
- a `pattern` keyword: `the_contract_uses_only_keywords_the_validator_checks` red.

The validator's own test, `the_validator_refuses_what_each_keyword_forbids`, gives each
keyword a value it must refuse.

**What the contract got wrong first, found by these tests.** An order puts a row missing the
attribute last ascending, not first; the fixture now pins it. A replication's create answers
only `201`. A write's metric defaults to `dot_product` and must be the index's. The binary
refuses a read cache over the memory store, so the fixtures expect `warm_index`'s
`409 no_read_cache` and its `200` is validated in process by `a_warm_answer_holds_the_contract`.
No server behaviour was changed.

**Suite:** 2,759 passed and 0 failed, the two IPv6 tests skipped because the container has no
IPv6 (.harness/m220-suite.log). fmt and clippy pass.

**Coverage:** `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95` passed: lines
97.33%, regions 96.01%, functions 97.05% (.harness/m220-coverage.log).

**Acceptance criteria.**
1. PASS: `every_route_is_in_the_contract_under_its_scope`. The router's 26 `(path, scope)`
   pairs equal the contract's, and probing 5 methods on each with a token of only that scope
   finds exactly the contract's 30 `(method, path, scope)` triples.
2. PASS: `request_bodies_name_the_fields_the_server_parses`. 13 closed objects probed; each one's
   fields from serde's refusal equal its properties, plus `read_only`, `schema` and `as_of` for
   a replication, each refused by value in a fixture step. `MultiQueryRequest` alone is
   unprobed: its root keys are pinned by fixture steps instead.
3. PASS: `error_codes_are_the_servers`. 44 codes, each at one status.
4. PASS: `the_fixtures_hold_against_the_server_and_the_contract`. 64 steps; every operation
   answered; every 2xx answered but the 6 `UNEXERCISED` names (the 4 external successes, a
   repeated copy's `200`, and `warm_index`'s `200`, which `a_warm_answer_holds_the_contract`
   validates).
5. PASS: `the_contract_uses_only_keywords_the_validator_checks` and
   `the_validator_refuses_what_each_keyword_forbids`.
6. PASS for `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the suite and
   `scripts/coverage.sh --fail-under-lines 95 --fail-under-regions 95`, run here.
   `scripts/mutants.sh` is pending CI's shards.

Review (one round by a read-only subagent, given the diff and denied the reasoning): FAIL on two
majors, both taken. `top_k` said `minimum: 0` where the server refuses 0 (now 1, with an
`invalid` step at 0; recall's `num` and `top_k` now carry the server's 1..25 and 1..100). Query
parameters declared on an operation were neither keyword-checked nor validated: now each step's
query is validated against its parameters (an unknown name, a missing required one, a refused
value), `invalid` covers it, and the keyword walk reads them. Turned red after the fix
(.harness/m220-review-breaks.log): `page_size` max 5,000 (fixtures red), a `pattern` on
`page_size` (keyword test red), `top_k` minimum 0 (fixtures red). Minors taken: the missing
attribute is now asserted absent (`absent` pointers); external `k` is optional (default 10) and
`window` at least `k`; the multi-query's refusals are stated; `prefix` and `cursor` are used by
steps. Not taken: `MultiQueryRequest.queries` items are still `QueryRequest`, which names three
fields a sub-query refuses; its description says so.
