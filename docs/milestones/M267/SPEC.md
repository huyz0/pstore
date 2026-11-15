# M267: client retries, index pagination, an async Python client, vector base64 helpers

## Serves
The [turbopuffer gap list](../../comparisons/turbopuffer-gaps-2026-11-15.md), its client
section: the vendor's SDK (`turbopuffer` 3.1.0) retries retryable failures with backoff,
auto-paginates `namespaces()`, ships an async client, and encodes vectors as base64. The
generated clients of M221 and M222 do none of these. Workstream B of
[the delivery plan](../../comparisons/turbopuffer-next-plan.md). No D- or OQ- id covers it.

## Delta
All of it in `scripts/gen-clients.py`'s two runtimes, so both generated clients gain it and
`gen-clients.py --check` holds them to it.

1. **Retries.** A call that fails with a `PstoreError` whose `retryable` is true (pstore
   sets it on every 5xx and 429; an unparsed body takes the same rule), or with a
   connection failure (refused, reset, timed out), is sent again, up to `max_retries` more
   times (default **2**; `0` is M221's and M222's behaviour). Before each retry the client
   waits `Retry-After` seconds when the answer gave a whole number (at most 60), and
   otherwise `min(8, 0.5 · 2^n)` seconds, n = 0, 1, ..., scaled by a random factor in
   [0.75, 1]. A non-retryable refusal is raised at once. After the last attempt the last
   error is raised as before. The wait is injectable (`sleep`) for tests.
2. **Pagination.** Python `Client.iter_indexes(prefix=None, page_size=None)` yields every
   index name, following `next_cursor` until it is `null`; TypeScript
   `Client.listIndexesAll(options)` is an async generator doing the same.
3. **Async Python.** `AsyncClient` takes `Client`'s arguments, and has every operation as
   an `async` method of the same name and signature, run on a worker thread
   (`asyncio.to_thread`), plus `iter_indexes` as an async generator. Standard library only.
4. **Base64 vectors.** `encode_vector(floats) -> str` and `decode_vector(str) -> list` in
   Python; `encodeVector(number[] | Float32Array): string` and
   `decodeVector(string): Float32Array` in TypeScript: standard-alphabet padded base64 of
   little-endian f32, the wire's `vector_encoding: "base64"` form (D-36).

**Settings rule.** A client option, not a server setting. Retries default on, at 2: they
act only on answers pstore marks retryable or on a request that never reached it, cost
nothing otherwise, and `max_retries=0` restores the old behaviour. The fixture suites run
with the default, and none of their 67 steps answers a retryable status, so they are
unchanged.

**Not changed:** the server, the contract, every generated method's signature.

## Acceptance criteria
1. **Retries.** `clients/python/tests/test_runtime.py` and
   `clients/typescript/test/runtime.test.mjs`, each against a local stub HTTP server
   scripted per test: two 503s with `retryable: true` then a 200 answer the 200 after
   exactly 3 requests, waits of the backoff's range; a 429 with `Retry-After: 2` waits 2;
   a 400 is raised after 1 request; `max_retries=0` raises the first 503 after 1 request;
   three 503s with `max_retries=2` raise after 3; a refused connection is attempted 3 times.
2. **Pagination.** The same suites against the real server (`scripts/clients.sh`): 5
   indexes created, `iter_indexes(page_size=2)` / `listIndexesAll({page_size: 2})` yield
   all 5 in order, over 3 pages.
3. **Async.** `AsyncClient` answers the fixture steps' first query and `iter_indexes`
   against the real server, and every `Client` operation has an `AsyncClient` method of
   the same parameters (compared with `inspect.signature`).
4. **Base64.** Round trips of `[0.0, -1.5, 3.25e-8, 1e30]` are exact, and the encoding of
   `[1.0, 2.0]` is `AACAPwAAAEA=`; a vector written base64-encoded and read back as floats
   through the server equals the floats.
5. **Gates:** `gen-clients.py --check`, `scripts/clients.sh` (with `tsc --strict`), and the
   named mutations observed red.

## Test plan
AC1 to AC4 are red against the parent: the options and helpers do not exist.

| AC | Mutation |
|---|---|
| 1 | a non-retryable refusal retried; `Retry-After` ignored; one retry fewer |
| 2 | pagination stopping after the first page |
| 4 | big-endian bytes |

## RA budget
Client-only. A retried request is a request the server sees again.

## Risks
- **A retried write.** A 5xx after a write applied is retried. pstore's writes are upserts,
  patches and deletes by id, which apply twice to the same state; a conditional write
  retried after it applied is refused by its own condition, which the caller sees. An
  idempotency key (M132) makes a retry exact.

## Tasks
- **M267.1** The spec.
- **M267.2** Both runtimes, tests, clients.sh running the new suites, docs.
