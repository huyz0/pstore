# M221: a Python client generated from the contract, run against the server

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Generate
schema/client artifacts from the same versioned contract. Start with Python/TypeScript
clients". The second part of DELIVERY's B row, after [M220](../M220/SPEC.md)'s contract.

## What holds today
- `api/openapi.json` is the contract, held against the server by M220's tests, and
  `api/fixtures/native.json` holds the steps every client must answer. No client exists.

## Delta
- **`scripts/gen-clients.py`** (standard library only) writes
  `clients/python/pstore_client/__init__.py` from the contract: one `Client` method per
  `operationId` (path parameters positional, the body next, query parameters and `session`
  keyword-only), a `TypedDict` or alias per component schema, `PstoreError` carrying the
  envelope's `code`, `message`, `retryable` and `Retry-After`, and `last_session` from the
  `x-pstore-session` header, `last_status` from the success's status. The file names the contract's version and SHA-256.
  `--check` exits 1 when the file is not what the generator writes now.
- **`scripts/clients.sh`** builds `pstore-server`, starts it unsecured on the in-memory store
  with `PSTORE_APPLIED_WRITES=5` on a free local port, and runs
  `clients/python/tests/test_fixtures.py`: every fixture step through the method its
  `operationId` names, its status and `expect` as M220's test reads them, and every client
  method called by some step.
- **Gates.** `gates.sh` and CI's `harness` job run `gen-clients.py --check`; CI runs
  `clients.sh` in a new `clients` job. AGENTS.md's Gates table names both.

Not changed: the contract, the server, the fixtures.

## Acceptance criteria
1. `py ./scripts/gen-clients.py --check` exits 0 on the committed tree, and 1 after the
   contract changes without regenerating (shown by changing a summary in a scratch copy).
2. `scripts/clients.sh` exits 0: `test_every_step_through_the_client` passes all 64 steps and
   calls all 30 methods; `test_a_refusal_carries_its_code_and_retryability` (a `404
   index_not_found`, not retryable) and `test_a_write_returns_the_session_a_read_then_passes`
   pass.
3. The generator, the generated module and its suite run on Python 3.9, the oldest `lib/py.sh`
   accepts: they compile and import under 3.9, and CI's `clients` job runs on 3.9.
4. `check-portable.sh`, `build-index.py --check` and the rest of `gates.sh` pass.

## Test plan
The client suite is new, so red is shown by breaking the client, one change at a time (recorded
in VERIFIED): the generator emitting `POST` for `PUT` (a write answers 405), dropping query
parameters (the page-size refusal answers 200), not raising on a refusal (a step expecting 409
sees no error body), and a fixture's expected `$dist` changed (the subset match fails).

## RA budget
Unchanged: no server path changes.

## Risks
- **Types are a reading aid, not a check.** A `TypedDict` is not enforced at run time; the
  fixtures check the wire, M220 checks the schemas.
- **The binary's background fold and replication run during the steps.** Expectations name
  only what those cannot change.

## Tasks
- M221.1 The generator, the Python client and its suite, `clients.sh`, and the gates.
