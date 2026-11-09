# M222: a TypeScript client generated from the contract, run against the server

## Serves
Workstream B of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Start with
Python/TypeScript clients". Completes DELIVERY's B row "Native OpenAPI, generated SDKs and
semantic fixtures" with [M220](../M220/SPEC.md)'s contract and [M221](../M221/SPEC.md)'s
Python client.

## What holds today
- `scripts/gen-clients.py` writes the Python client from `api/openapi.json`, `--check` gates
  it, and `scripts/clients.sh` runs it against a real server. There is no TypeScript client.

## Delta
- **`gen-clients.py` also writes `clients/typescript/src/index.ts`**, dependency-free (`fetch`
  only): a `type` per component schema, `Client` with one camelCase method per `operationId`
  (path parameters, then the body, then an options object of query parameters and `session`),
  `PstoreError`, `lastSession` and `lastStatus`, and `OPERATIONS`, each operationId's method
  and argument shape. `--check` covers it like the Python file.
- **`clients/typescript/test/fixtures.test.mjs`**: every fixture step through the client
  (status, `expect`, `absent`), every method called, and the client's own plumbing.
- **`clients.sh`** type-checks the client with `tsc --strict` (`clients/typescript/tsconfig.json`)
  and runs the suite with Node's test runner and type stripping. Each client runs the fixtures
  in its own tenant (`PSTORE_FIXTURE_TENANT`), as both share one server. CI's `clients` job
  sets up Node 22 and `typescript@5.9.3`.

Not changed: the contract, the server, the fixtures, the Python client's interface.

## Acceptance criteria
1. `./scripts/gen-clients.py --check` exits 0 on the tree, covering both files.
2. `./scripts/clients.sh` exits 0: `tsc -p clients/typescript` passes, and the Node suite's 5
   tests pass, the fixtures test answering all 64 steps and calling all 30 methods.
3. `check-portable.sh`, `build-index.py --check`, `check-links.sh` and `check-verified.py` pass.

## Test plan
New suite, so red by breaking the generated client, one change at a time: a write sent as POST,
query parameters dropped, `lastStatus` always 200 (the copy's 202 step fails), and a type
error (`tsc` fails).

## RA budget
Unchanged: no server path changes.

## Risks
- **Node before 22.6 cannot strip types.** The package says `engines: node >=22.6`; a bundler
  user compiles `src/index.ts` as any TypeScript.
- **Types describe, the wire decides**, as for Python.
- **An id above 2^53 is not exact.** `JSON.parse` reads a uint64 id as a double, so the client is
  exact only for ids up to 2^53; the generated header says so. The Python client is exact.

## Tasks
- M222.1 The TypeScript generator, client, suite and gates.
