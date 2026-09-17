# M7f — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

1. **`/metrics` reports all four classes and no tenant** —
   `metrics_expose_totals_and_never_a_tenant` (`cargo test -p pstore-server --test metrics`):
   `pstore_blob_requests_total{class}` for `read`, `write`, `list` and `delete`, plus
   `pstore_http_requests_total` and `pstore_refusals_total` — and **neither tenant id appears
   anywhere in the body**, searched for as a substring rather than as a label, so a value would
   fail it too. ⚠️ Per-tenant series are refused by design: 1M tenants × four classes is four
   million series, which is how a metrics endpoint takes down what it observes.
2. **The counters move with the work** — `the_counters_move_with_the_work`, asserted across
   **two** tenants so the equality is not one the plumbing guarantees: the write-class total is
   the **sum** of what the two callers were told they spent. And
   `a_refusal_is_counted_under_its_own_code`: `tenant_required` and `bad_request` each at
   exactly 1, with an unmatched route counted under `route="<unmatched>"` rather than
   inventing a series per 404 URL, which would be unbounded and attacker-controlled.
3. **No tenant header, no tenant data** — `metrics_need_no_tenant_header`: the series sets of
   an anonymous and a tenant-bearing request are equal **excluding `{route="/metrics"}`**.
   ⚠️ Spec review round 2 found the earlier "identical series set" false: the route counter is
   incremented by the layer *after* the handler formats the body, so the first `/metrics`
   response cannot contain its own series and the second can.
4. **Every answer is correct or a refusal, over ten seeds** —
   `under_injected_faults_every_answer_is_correct_or_a_refusal`
   (`cargo test -p pstore-server --test chaos`) at `read_error` and `slow_down` 0.3, with the
   oracle fixed in the spec rather than left to the implementer: exact-match query vectors,
   `top_k` ≥ corpus, comparison on the **set** of ids. ⚠️ A `404 index_not_found` for an index
   the test wrote counts as a **wrong answer**, not a refusal — that is the shape a swallowed
   read error takes on that route.
5. **An acknowledged, folded document is never missing** —
   `an_acknowledged_document_is_never_missing_under_faults`, read back through a **second
   `Api` over the same backend with an empty memtable**, faults still on. Asserting through the
   writer would have proved only that its own RAM still held the rows.
6. **The SLO document has a gate** — `./scripts/check-slos.py` (24 objectives, every
   `enforced` one naming a gate that exists) and `./scripts/selftest-check-slos.sh`, which
   observes it **refusing**: a ghost test name, a deleted script path, a `blocked` row with no
   blocker, and a third status. Both are in `scripts/gates.sh` and in CI, and
   `scripts/build-index.py --check` refused the commit until `AGENTS.md`'s Gates table listed
   the new one — which is the gate-list gate doing its job.
7. **Coverage and mutation** — `./scripts/coverage.sh --fail-under-regions 95` passes at
   **95.08%** regions, 96.88% functions, 97.03% lines. Mutation:
   `scripts/mutants.sh --file crates/pstore-server/src/lib.rs` — **20 of 20 viable mutants
   caught**, 47 unviable. ⚠️ The first sweep missed one and it was the right one to miss: the
   route counter's increment mutated to `*= 1` leaves **every series in place at zero**, and a
   test that greps for a series name cannot tell a counter from a label. The value is asserted
   now — two writes, counted as two — observed red against that mutant.

## What this milestone found

⚠️ **A refused `durable` write leaves its rows visible to the process that attempted it.** The
chaos suite found it on seed 0: a write whose flush was refused returns an error, and the rows
are restored to the memtable, where the freshness layer serves them — so a later query from
that instance returns a document the caller was told had failed. It is **consistent with
`batched` semantics** (visible, not durable) rather than wrong, and the fix would be to discard
rows a caller may be about to retry. So the oracle says what is actually promised: a query may
return any id that was **sent**, and may never return one that was not. Recorded here and as a
stated non-property rather than smoothed out of the test.

⚠️ **The quota is not wired, and the first draft of this spec said it would be.** Spec review
priced it: `Metered::admit` refuses with `BlobError::Other(..)`, which the error table maps to
`503 storage_unavailable`; a real `429 rate_limited` needs a typed `BlobError` variant, an
`EngineError` arm, an error-table row, a header path on `ApiError` — which today emits only a
status and a body — and a time-to-refill on `Bucket` that does not exist, plus a change to
`Api`'s type parameter. Three crates below the server, and its own milestone. **M7f is what an
operator can see; refusing is not seeing.**

## Stated non-properties

- **No per-tenant series.** Design rule 13's per-tenant accounting is `Meter::usage(tenant)`,
  and the fleet-wide rollup of it does not exist — `pstore-meter`'s own docs say so.
- **A refused durable write's rows stay visible to that process** until it flushes them
  successfully or exits. Same state a `batched` write is in.
- **No tracing, no alerting rules, no dashboard.** A span per request needs a collector to send
  it to, which is a deployment decision and not this milestone's.
- **Every latency objective is `blocked`**, each naming the measurement that unblocks it. A
  latency number from WSL2 against a `MemoryStore` wearing a production label is exactly what
  D-104 exists to forbid.
