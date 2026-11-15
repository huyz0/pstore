# M238: a refused request's body read and discarded, so a proxy relays the refusal

## Serves
Workstream A of [the delivery plan](../../comparisons/turbopuffer-next-plan.md): "Distinguish
400 validation, 401/403 authorization, 413 size, 429 tenant admission", behind the public TLS
proxy [M237](../M237/SPEC.md) runs, which found those refusals reaching a client as `502`.

## Delta
M128 refuses a bad credential, a missing scope, an overloaded tenant, a content encoding and an
oversized `Content-Length` before it polls the body, and the connection then closes with the
body unread. A proxy still writing that body is reset and answers `502` (M237: 2 of 5 tries at
1 MiB streamed). After, with **`drain_refused_bodies`** on:
1. A request answered with any status but 2xx whose body was not read to its end has the rest
   read and **discarded** (never buffered) before the response is sent, at most `body_bytes`
   bytes and within `body_read_timeout`, so the peer has finished sending when it reads the
   answer. Past either bound it stops, and the connection closes as today.
2. A body whose `Content-Length` is over `body_bytes` is not read at all, nor one whose client
   sent `Expect: 100-continue` (it waits, sending nothing): today's behaviour.
3. Nothing else changes: the status, the body, the headers, admission (a drained byte reserves
   nothing; a refusal still precedes admission and dispatch), and every request answered 2xx.

**A setting, off by default.** `drain_refused_bodies` (a boolean, version 1 of
`PSTORE_REQUEST_LIMITS_CONFIG`), the cluster default, set the same on every server; a mixed
cluster is correct, each server answering in its own mode. No per-index override: every
refusal it serves precedes routing to an index. Off keeps M128's promise that a refused body
is never polled (its `authorization_precedes_body_polling_and_admission` pins it, unchanged).
On costs, per refused request, up to `body_bytes` read for nothing and the connection held up
to `body_read_timeout`, by a peer that may hold no credential: turn it on behind a proxy.

## Acceptance criteria
1. On: a 401 (none, bad), 403 (scope), 415 and 429 each read the whole body (counted), keep
   their status and code, and leave admission and the store untouched.
2. On: a declared length over `body_bytes`, or `Expect: 100-continue`, leaves the body unread.
3. On: a body with no declared length is read at most `body_bytes` plus one chunk.
4. On: a body that stops arriving is abandoned at `body_read_timeout` and the refusal sent.
5. Off (the default, and `false` in the config): every M128 test unchanged and green; the
   config reads `true`, `false` and absent, and refuses a non-boolean.
6. M237's smoke test with it on: an unauthenticated 1.5 MiB and 2 MiB body get `401` through
   the proxy streamed as well as buffered. Measured: `502`s in 20 tries per size and mode, off
   then on, recorded in VERIFIED.md.

## Test plan
In `crates/pstore-server/tests/it/ingress.rs`:
1. `a_refused_body_is_read_to_its_end_when_draining`. Mutations: no drain; drain only on 401.
2. `a_body_declared_too_large_is_never_read`, `a_body_behind_expect_continue_is_never_read`.
   Mutations: the length check dropped; the `Expect` check dropped.
3. `draining_stops_at_its_bound`. Mutation: no byte bound.
4. `a_stalled_refused_body_is_abandoned_at_the_deadline` (paused clock). Mutation: no deadline.
5. `drain_refused_bodies_is_read_from_the_config`. Mutation: default on.
6. `scripts/tls-smoke.sh`, and a counting run of its early-refusal check (VERIFIED.md).

## RA budget
Unchanged: a refusal touches no store, on or off.

## Risks
- A peer could hold connections open by trickling refused bodies: each bounded by the read
  deadline, their number by nothing in admission, which has released the request. Off by
  default for that reason. A refusal made after part of the body was read (a `413` from
  collection) drains up to `body_bytes` more, in one more deadline.

## Tasks
- M238.1 spec. M238.2 tests, red. M238.3 the layer and setting. M238.4 smoke, VERIFIED, docs.
