# M222 verification

Implemented in `scripts/gen-clients.py` (the TypeScript half),
`clients/typescript/{src/index.ts,test/fixtures.test.mjs,tsconfig.json,package.json}`,
`scripts/clients.sh` (tsc, Node, and a tenant per client, which the Python suite now reads too),
CI's `clients` job, AGENTS.md, DELIVERY row B and `docs/api-migrations.md`. Run in a 4-core
cloud container with Node 22.23.4 and tsc 6.0.2; CI pins tsc 5.9.3. No duration is claimed.

**Red first** (.harness/m222-client-breaks.log), each against a fresh server: the write sent as
POST (the fixtures and session tests red), query parameters dropped (the fixtures and plumbing
tests red), `lastStatus` always 200 (the fixtures test red at the copy's 202), and a type error
appended to the client (`tsc` red, TS2322).

**Acceptance criteria.**
1. PASS: `./scripts/gen-clients.py --check` exits 0, both files current.
2. PASS: `./scripts/clients.sh` exits 0 (.harness/m222-clients.log): tsc passes, the Python
   suite's 5 tests pass, and the Node suite's 5 pass, its fixtures test answering all 64 steps
   and calling all 30 methods.
3. PASS: `./scripts/check-portable.sh`, `./scripts/build-index.py --check`,
   `./scripts/check-links.sh` and `./scripts/check-verified.py`. No Rust changed.

Review (one round by a read-only subagent, given the diff and denied the reasoning): FAIL on two
majors, both taken. Nothing checked that a call sends `x-pstore-session` or the bearer token (one
server answers an eventual read with the write anyway): a stubbed-`fetch` test now asserts the
method, encoded path, body, token, tenant and session, red with the session header dropped and
with the token dropped (.harness/m222-review-breaks.log). An `Id` above 2^53 is read inexactly by
`JSON.parse`: the generated header and SPEC's Risks now say so. Not taken (minor): the component
`Error` type shares the global's name; `PstoreError` still extends the built-in.
