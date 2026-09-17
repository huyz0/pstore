# M7f — What an operator can see, and what we actually promise

**Serves:** M7's third bullet (*observability, SLOs, chaos testing, Jepsen-style
verification*), **Design rule 13** (per-tenant blob-request metering), and **D-104**, which is
why half the SLOs in this milestone are marked blocked rather than guessed at.

**Depends on** [M6b](../M6b/SPEC.md) (`pstore-meter`, built and never wired),
[M7c](../M7c/SPEC.md) (the server that would expose it), and M0a's fault-injecting store.

## ⚠️ The four words in that bullet are three different things

- **Observability** is buildable now: the per-tenant counters exist, the server already diffs
  them per request, and nothing exposes them to anyone but the caller of that one request.
- **SLOs** are half-buildable, and the honest half is the part nobody writes down. A latency
  SLO measured on WSL2 against a `MemoryStore` is **relative only** (D-104) — stating one
  would be the exact failure this repository keeps refusing. What *is* measurable here, and is
  already asserted in tests, is **request count and round-trip depth per operation**. So the
  SLO document states both: the budgets that are enforced today by a gate, and the latency
  objectives that are **blocked on M0b**, named with the measurement each one needs.
- **Chaos and Jepsen-style verification** exist at the library layer — `pstore-sim`,
  the fault-injecting store, M2's linearizability proof — and have never been pointed at the
  **server**. That is the new thing, and it is one assertion: under injected faults, the API
  never answers *wrongly*; it answers correctly or it refuses.

## Delta

**Adds**
- `GET /metrics` — Prometheus text format, and **not per tenant**: one series per
  `(op_class)` with the totals `Accounted` already keeps, plus a per-route request counter and
  a refusal counter by error code. ⚠️ **Per-tenant series are deliberately out**: 1M tenants ×
  5 classes is 5M series, which is how a metrics endpoint takes down the thing it observes.
  The per-tenant numbers already go to the caller that caused them, in every response.
- `crates/pstore-server/tests/chaos.rs` — the API driven against a fault-injecting store at
  rising error rates, asserting the property that matters: **every 200 is correct, and
  everything else is a refusal with a code**. Never a wrong document, never a silent empty
  answer where an error belongs. ⚠️ The durability half is read back through a **second `Api`
  over the same backend** — a second instance in the same process, not a second OS process:
  what it has that the writer does not is an **empty memtable**, which is the whole point.
- `docs/research/11-design/slos.md` — the objectives, each marked `enforced` (a gate asserts
  it today) or `blocked` (and by what).

- ⚠️ **The refusal code travels in a response extension**, not a static. `ApiError::into_response`
  inserts its `code` into the response's extensions and a small layer reads it into the
  registry this `Api` owns. A process-global counter would be simpler and would make criterion
  2 unassertable under a parallel test binary — the same argument `Config::from_vars` already
  makes about process-global state. ⚠️ Requests axum refuses before routing (unmatched path,
  method, body limit) carry no code and are counted with `route="<unmatched>"`, named here so
  it is a decision rather than a surprise in the output.
- `scripts/check-slos.py` — the gate for criterion 6, because "every `enforced` row names a
  gate" **is a predicate over files** and `gate-design`'s non-negotiable forbids asking an
  agent to check one.

⚠️ **The quota is NOT wired, and this spec's first draft said it would be.** Spec review priced
it: `Metered::admit` refuses with `BlobError::Other(..)`, which the error table maps to
`503 storage_unavailable`; a real `429 rate_limited` needs **a typed `BlobError` variant, an
`EngineError` arm, an error-table row, a header path on `ApiError` (which today emits only a
status and a body), and a "time until *n* tokens refill" on `Bucket` that does not exist** —
plus a change to `Api`'s type parameter, because `Metered<TenantView<S>>` is a different type
from `TenantView<S>` and a registry cannot hold both. That is three crates below the server and
its own milestone. **M7f is what an operator can see; refusing is not seeing.**

**Does not add** — distributed tracing (a span per request is worth having and needs a
collector to send it to, which is a deployment decision); alerting rules; a Grafana dashboard;
a fleet-wide rollup of `Meter::usage(tenant)`, which is the per-tenant surface Design rule 13
asks for and which `pstore-meter`'s own docs already record as absent; or a Jepsen harness
proper, which needs the cluster to exist as a process group.

## Acceptance criteria

1. `GET /metrics` returns Prometheus text with `pstore_blob_requests_total{class}` for **all
   four** classes — `read`, `write`, `list`, `delete` — plus `pstore_http_requests_total{route,
   status}` and `pstore_refusals_total{code}`, and **no per-tenant label anywhere**: asserted
   by searching the body for both tenants' ids after both have written.
2. The counters **move with the work**, asserted across **two tenants** so the equality is not
   one the plumbing guarantees: the write-class total equals the **sum** of the two responses'
   reported costs. A refused request increments `pstore_refusals_total{code}` for its own code
   and no other.
3. `/metrics` requires **no tenant header** and exposes **no tenant data**: two requests, one
   with a tenant header and one without, return the same series set **excluding
   `{route="/metrics"}`**. ⚠️ That exclusion is not a convenience: the route counter is
   incremented by the layer *after* the handler has formatted the body, so the **first**
   `/metrics` response cannot contain its own series and the second can — spec review round 2
   found the earlier "same series set" false for exactly that pair. Comparing bytes is wronger
   still, and exempting `/metrics` from the counter would hide the endpoint's own load.
4. **Chaos**: with the fault-injecting store at `read_error` and `slow_down` 0.3, a run of
   mixed API operations produces **zero wrong answers** over **ten seeds**. ⚠️ The oracle is
   specified rather than left to the implementer: query vectors are **exact matches** for
   written documents, `top_k` is **≥ the corpus size**, and the comparison is on the **set of
   ids**, never their order — RRF ordering over a fault-perturbed segment set is not a stable
   oracle. Every non-`200` must carry a code from the error table, ⚠️ **and a
   `404 index_not_found` for an index this test created counts as a wrong answer, not a
   refusal** — that is the shape a swallowed read error takes here.
5. **Chaos, the harder half — and it is asserted from a second process.** Every document
   acknowledged `durable` and folded is either returned by a query served from a **second `Api`
   over the same store with an empty memtable**, or that query refuses. ⚠️ Asserting it through
   the writing instance would prove only that its memtable still holds the rows, which is the
   case the code already handles; the interesting one is a `200` whose rows exist **only in
   RAM**, and only a second instance can tell the difference.
6. `slos.md` states every objective with a status of exactly `enforced` or `blocked`, and
   **`scripts/check-slos.py` is the gate**: every `enforced` row names a path under `scripts/`
   that exists or a test function that resolves in the tree, every `blocked` row names its
   blocker, and any other status is a failure. ⚠️ A script, because the rule is a predicate
   over files and `gate-design` forbids asking an agent to check one of those. No objective is
   stated in seconds unless a real cloud produced the number.
7. Region coverage ≥95% on the changed crates; every mutant in the new code caught or named.

## Test plan

| # | Test | The mutation it kills |
|---|---|---|
| 1,3 | `metrics_expose_totals_and_never_a_tenant`, `metrics_need_no_tenant_header` | a tenant label, which is 5M series at the scale the README claims and a data leak at any scale |
| 2 | `the_counters_move_with_the_work`, `a_refusal_is_counted_under_its_own_code` | counters reported as constants; a refusal counter incremented for every error, which makes the one number an operator would page on useless |
| 4,5 | `under_injected_faults_every_answer_is_correct_or_a_refusal`, `an_acknowledged_document_is_never_missing_under_faults` | an error swallowed into an empty result — the one failure mode that looks like success; a retry that returns a stale answer |
| 6 | `scripts/check-slos.py`, with `scripts/selftest-check-slos.sh` running it against a fixture row that names a gate which does not exist | an objective with no gate named, which is a promise nobody keeps; a checker that passes an `enforced` row naming a script that was deleted |

⚠️ **Criterion 5 is the one worth the milestone.** Everything else observes; this one asserts
that what we observe is true under the conditions a real backend produces.

## RA budget

| Operation | W | Rseq depth | Rpar | List |
|---|---|---|---|---|
| `GET /metrics` | **0** | **0** — process-local counters | 0 | 0 |
| Every other route | unchanged | unchanged | unchanged | 0 |
| The chaos suite | it is a test; it costs what it injects | — | — | 0 |

## Risks

- **A metrics endpoint that allocates per request is a load generator.** The counters are
  atomics already; the endpoint formats them. If formatting ever becomes a scan of per-tenant
  state, criterion 1's no-tenant-label rule is what stops it.
- **Chaos tests are flaky by construction unless seeded.** `Faulty` is deterministic per seed
  and the suite fixes ten of them; a failure is replayable, which is the only kind worth having.
- **An SLO document is where honesty goes to die.** Every line is `enforced` with a gate named
  or `blocked` with the blocker named; there is no third status, because the third status is
  how "we aim for p99 < 100ms" gets written by someone who has measured nothing.
- **The chaos suite runs inside `cargo test`, and that is a budget decision.** Ten seeds of
  mixed operations against a `MemoryStore` with injected faults is milliseconds — `Faulty`
  injects no latency unless asked, and `Congested`'s backoff on an injected 503 is 1–8 ms. If
  it ever grows past a second it belongs outside, beside `recall.sh` and `depth.sh`, for the
  reason those are: a mutation sweep runs the suite once per mutant.

## Tasks

| Id | Commit |
|---|---|
| M7f.1 | `GET /metrics`, and the counters behind it |
| M7f.2 | The chaos suite through the API, read back by a second instance |
| M7f.3 | `slos.md`, and `check-slos.py` as its gate |
