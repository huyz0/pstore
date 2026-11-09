# M221 verification

Implemented in:
- `scripts/gen-clients.py`: the generator, standard library only, with `--check`.
- `clients/python/pstore_client/__init__.py`: generated from `api/openapi.json` 1.0.0; 30
  methods on `Client`, a `TypedDict` or alias per component schema, `PstoreError`.
- `clients/python/tests/test_fixtures.py`: the fixtures through the client, and two tests of the
  client's own plumbing.
- `scripts/clients.sh`; `gates.sh`, CI's `harness` job (`--check`) and a new `clients` job;
  AGENTS.md's Gates table; DELIVERY row B.

All runs were in a 4-core cloud container, the server on the in-memory store. No duration is
claimed.

**Red first.** The suite is new, so it was turned red by breaking the client or a fixture, one
change at a time, against a fresh server each time (.harness/m221-client-breaks.log): the write
sent as `POST` (the fixtures test and the session test red), query parameters dropped
(fixtures red: the page-size and progress refusals answer 200), a refusal returned instead of
raised (the fixtures test and the refusal test red), a fixture's `$dist` changed from 9 to 8 (fixtures red), and, after review, the copy step's 202 expected as 200 (fixtures red: the runner now reads `last_status`).

**Acceptance criteria.**
1. PASS: `./scripts/gen-clients.py --check` exits 0 on the tree. With one operation's
   summary changed in `api/openapi.json` and not regenerated it exits 1 and names the stale
   file; restored, 0 again.
2. PASS: `./scripts/clients.sh` exits 0 (.harness/m221-clients.log):
   test_every_step_through_the_client answers all 64 steps and calls all 30 methods;
   test_a_refusal_carries_its_code_and_retryability and
   test_a_write_returns_the_session_a_read_then_passes pass, and so do the two plumbing tests,
   test_query_values_are_written_as_the_server_reads_them and
   test_retry_after_is_read_with_or_without_an_envelope. (Python tests, named without
   code marks: `check-verified.py` resolves only Rust tests.)
3. PASS: under CPython 3.9.25 (installed by `uv` for the check) the generator, the client and
   its suite compile, the client imports, and `./scripts/gen-clients.py --check` exits 0. CI's `clients`
   job runs on 3.9.
4. PASS: `./scripts/check-portable.sh`, `py ./scripts/build-index.py --check`,
   `./scripts/check-links.sh`, `py ./scripts/check-verified.py` and
   `py ./scripts/check-milestone-ids.py`. No Rust changed, so the suite, coverage and mutation
   gates have nothing new to measure; CI runs them.

Review (one round by a read-only subagent): PASS with 2 majors, both taken. The runner took a
success's status from the step, so a 200 where 202 was due passed: the client now keeps
`last_status` (which also lets a caller tell a copy it started from one an earlier request
had), and the runner compares it and checks the fixtures' `absent` pointers. Minors taken: the
docstring says what raises (400 and up); `Retry-After` is read for a refusal without an
envelope too, and tested; query value writing is tested. Not taken: an HTTP-date
`Retry-After` is not parsed (the server sends seconds).
