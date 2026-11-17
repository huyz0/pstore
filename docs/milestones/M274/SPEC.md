# M274: a resent conditional or by-filter facade write applies at most once

## Serves
Workstream C of [the plan](../../comparisons/turbopuffer-next-plan.md): "Retrying after an
ambiguous network failure needs a bounded, durable idempotency/result policy", through the B
facade. Closes the `retries` gap [M272](../M272/SPEC.md)'s manifest records. Reuses
[M132](../M132/SPEC.md)'s keys unchanged; no decision is reopened.

## Delta
The pinned SDK resends a 408, 409, 429 and 5xx up to four times. A conditional or by-filter
write resent after its answer was lost is evaluated again against the rows as they are then,
and may apply twice; the SDK sends no idempotency key. It does number its attempts: every
request carries `x-stainless-retry-count`, `0` first (checked against 3.1.0's
`_base_client.py`), and a resend carries the same body.

After, for a facade write holding a condition or a by-filter operation (M269's
`WRITE_APPLIED` keys), on a server with idempotency keys (`PSTORE_APPLIED_WRITES` 3 or more):
1. **First attempt** (count `0` or absent): the facade issues a native key (M132's issuing
   path, the engine's own call) and sends it as the native write's `idempotency_key`. It
   remembers the key under a 128-bit keyed hash of (tenant, namespace, body bytes).
2. **Resend** (count above `0`) of a remembered body: the remembered key is sent. M132 then
   applies the write at most once and answers a repeat with the first attempt's outcome
   (`replayed`), or `idempotency_expired` / `idempotency_partial` as it does natively.
3. **Resend this server cannot match** (another server, a restart, an evicted entry), or
   any resend where keys are off: refused `400 retry_outcome_unknown`, retryable false, naming
   why: the first attempt may have applied, so read the rows and call again. Never evaluated a
   second time. A fresh call (count `0`) is a new write, as the SDK's caller intends.
4. **Memory**: at most 4,096 entries a server, oldest evicted first; about 64 bytes each.

Unchanged: every write with no condition and no by-filter operation (resending one writes
the same rows), every native route, the first attempt where keys are off (unkeyed, as in
M269), the native write's costs and refusals. No setting: what it removes is a silent second
application, and what it adds is an explicit refusal where that was the risk.

## Acceptance criteria
1. A conditional patch whose first answer is lost after it applied, resent with
   `x-stainless-retry-count: 1`, is not applied again: the row is changed once, and the
   resend answers the first attempt's counts (the native answer's `replayed`).
2. A by-filter delete, resent after a row its filter matches was written, does not delete
   that row: the resend replays the first attempt's outcome.
3. A resend of a body the server never saw, and any resend with keys off, is `400
   retry_outcome_unknown`, nothing buffered; a first attempt with keys off is unchanged.
4. A write with no condition or by-filter operation is unchanged, resent or not (no key).
5. The memory is bounded: the 4,097th body evicts the first, whose resend is then refused.
6. The facade's first attempt costs the native "issue a key, then the keyed write": no more
   requests, no more depth; `facade_adds_no_request` unchanged and green.
7. The pinned SDK, against a server whose first answer to a conditional write is a 503 after
   applying, gets one application and the replayed answer (`scripts/facade.sh`).

## Test plan
In `crates/pstore-server/tests/it/facade.rs`, red against M273:
1. `a_resent_conditional_write_applies_once`. Mutation: no key sent on a first attempt.
2. `a_resent_by_filter_delete_applies_once`. Mutation: a fresh key on a resend.
3. `an_unmatched_resend_is_refused`, `a_resend_without_keys_is_refused`. Mutation: an
   unmatched resend sent unkeyed.
4. `a_plain_write_is_not_keyed`. Mutation: every applied write keyed.
5. `the_attempt_memory_is_bounded` (a unit test over the memory). Mutation: no eviction;
   evicting the newest.
6. `a_keyed_facade_write_costs_the_native_pair`, and `facade_adds_no_request` unedited.
7. `clients/turbopuffer/test_sdk.py`: a test whose first answer is replaced by a 503 through
   the SDK's own `httpx` transport hook, the write having been applied.

## RA budget
A first attempt: the native key issue (free while the engine's mark is fresh, one HEAD read
a second at most otherwise, M132) and the native applied write. A resend: the native keyed
write. A refused resend: none.

## Risks
- Two clients sending the same bytes at once share an entry: the later key wins, so the
  earlier client's resend replays the later write's outcome. Its own write applied at most
  once; only the counts it reads are the other's. Rare, since such bodies are identical.
- Behind a balancer that spreads one client's attempts over servers, a resend is refused
  rather than matched. Explicit, and listed in the manifest.

## Tasks
- M274.1 the spec. M274.2 tests, red. M274.3 the facade's keys and memory. M274.4 the SDK
  test, the manifest's `retries`, docs, VERIFIED.
