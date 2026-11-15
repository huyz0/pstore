# M267 verification

The spec was not sent to a separate reviewer before code; the PR's code review covers it.

How to run the tests: `./scripts/clients.sh`. (Python and TypeScript tests are named without code marks: `check-verified.py` resolves only Rust tests.) It starts a server, then runs
`clients/python/tests` (`test_fixtures.py`, `test_runtime.py`) and
`clients/typescript/test/fixtures.test.mjs` and `runtime.test.mjs`, the TypeScript after
`tsc --strict`.

Red runs. With the parent's generated clients, `test_runtime.py` fails 8 tests and skips the
3 that need a server, run without one: `max_retries`, `sleep`, `encode_vector`,
iter_indexes and `AsyncClient` do not exist. `runtime.test.mjs` fails at import:
`MAX_RETRY_AFTER`, `encodeVector` and `decodeVector` are not exported. Each test was also seen
red by the mutations below, applied to `scripts/gen-clients.py`'s runtimes by a script that
regenerated the clients, ran `scripts/clients.sh` and reverted. All ten were killed, five
per language.

1. Retries (Retries in test_runtime.py; the first six tests of runtime.test.mjs), by
   `./scripts/clients.sh`: green. Two retryable 503s then a 200 answer the 200 after 3 requests, with waits of
   0.375 to 0.5 s and 0.75 to 1 s. A 429 with `Retry-After: 2` waits 2 s, and one of 600 waits
   60 s. A 400 is raised after 1 request. `max_retries=0` raises the first 503 after 1
   request. Three 503s raise the last after 3. A refused connection is tried 3 times. Killed
   in each language: a non-retryable refusal retried (R1), `Retry-After` ignored (R2), one
   retry fewer (R3).
2. Pagination (test_iter_indexes_follows_every_page; "listIndexesAll follows every
   page"), by `./scripts/clients.sh`: green. 5 indexes, a page size of 2, all 5 in order over 3 pages. Killed: stopping
   after the first page (R4).
3. Async (test_async_client_answers_and_pages, test_every_operation_has_its_async_twin),
   by `./scripts/clients.sh`: green. A write, a query and iter_indexes through `AsyncClient` against the server.
   Each of the 31 operations is a coroutine with `Client`'s parameters.
4. Base64 (Vectors, test_a_base64_vector_is_written_and_read_back; the last two
   TypeScript tests), by `./scripts/clients.sh`: green. `[0.0, -1.5, 3.25e-8, 1e30]` round trips exactly to its f32
   values, `[1.0, 2.0]` encodes as `AACAPwAAAEA=`, a 3-byte vector is refused, and a vector
   written base64 is read back as the same floats and as base64. Killed: big-endian
   bytes (R5).
5. Gates: `./scripts/gen-clients.py --check` passes. `./scripts/clients.sh`: the Python suite
   ran 16 tests, OK, and the TypeScript suites passed 14 with 0 failed, after `tsc --strict`.

The two retryability tests from M221 and M222 (`test_retry_after_is_read_with_or_without_an_envelope`
and its TypeScript twin) pinned a single attempt at a retryable 503. They now construct
their client with `max_retries=0` (`maxRetries: 0`), the old mode by name, and their
assertions are unchanged. With the default they would still pass, but they would wait 14 s
on real timers.

No Rust changed, so the workspace suite was not rerun for this milestone.
