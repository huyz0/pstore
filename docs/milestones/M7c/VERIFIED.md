# M7c — Verified

One line per acceptance criterion in [SPEC.md](SPEC.md). Gate: `scripts/check-verified.py`.

⚠️ Every "observed red" below is either a test written before the code and run against its
absence, or a mutation applied to the shipped code and the failure read.

1. **A batched write costs nothing; a durable one costs its lane registration once** —
   `a_batched_write_costs_nothing` and
   `a_durable_write_costs_its_lane_registration_once_then_one_put`
   (`cargo test -p pstore-server --test cost`). Measured through the handler on the
   per-tenant counter: batched is **0 requests of every class**; the first durable flush is
   **2 write-class and 1 read** — the bundle plus the one CAS that records the lane — and
   every flush after it is **1 write, 0 reads**. ⚠️ The two-phase assertion is spec review
   round 1's finding: the spec first claimed a flat `1 W`, which is false on a lane's first
   flush, and a test that only ever saw one flush could not tell a per-lane cost from a
   per-write one.
2. **A write is queryable before it is folded** — `a_write_is_queryable_before_it_is_folded`
   (same command): nothing flushed, nothing folded, the documents come back, and
   `meta.unfolded_hits` is non-zero. The assertion is on the **set** of ids and not their
   order: which of three near-identical vectors ranks first is the scorer's business and this
   milestone does not own it.
3. **A folded write reaches a second server** — `a_folded_write_is_visible_to_a_second_server`
   (`cargo test -p pstore-server --test durable`). Two `Api`s over one backend on **different
   lanes**. Before the fold the second answers `404 index_not_found`; after `POST
   /v1/admin/fold` it returns the document with `meta.unfolded_hits == 0` — it has no
   memtable — and `cost.blob_reads > 1`, so the answer came out of the blob store.
   ⚠️ The spec first described the pre-fold answer as an empty result set and the code
   returns `404`; **the spec was corrected, not the code**, because the index genuinely does
   not exist for that process under the same existence union criterion 6 uses. Code review
   found the two documents disagreeing.
   ⚠️ `a_batched_write_survives_nothing` is the other half: a fold cannot make durable what
   was never flushed, and the second server still sees nothing.
4. **Tenancy is enforced and never defaulted** — `a_missing_tenant_header_is_refused`
   (absent, empty, non-numeric and negative all `400 tenant_required`) and
   `two_tenants_do_not_see_each_others_documents`, where both tenants write the index name
   `shared` and each reads back only its own rows. The fold route takes the same header.
5. **No endpoint LISTs** — `no_endpoint_lists` (`cargo test -p pstore-server --test cost`)
   asserts `cost.blob_lists == 0` on the write, query, enumeration, summary and fold routes.
   ⚠️ The enumeration one is the point: a LIST over the tenant's prefix is the obvious
   implementation, and it is priced like a PUT and capped at 1000 keys.
6. **The error table is asserted per row** — `the_error_table_maps_every_row`,
   `the_requests_that_ask_for_nothing_are_refused`,
   `a_contended_commit_is_a_409_and_not_a_500`, `a_storage_failure_is_a_retryable_503`,
   `a_backend_that_stops_fencing_maps_to_a_retryable_503`,
   `an_unreadable_head_is_a_500_and_says_nothing_it_does_not_know`,
   `a_dimension_mismatch_is_a_client_error_on_both_query_paths` and
   `a_just_written_index_is_not_a_404`. ⚠️ **The engine half of the table was written and
   never executed**, which the coverage number found: `409`, `503` and the catch-all `500`
   were all unexercised while the request-level refusals were asserted. A table nothing runs
   is a table whose `409` may be a `500` in production and green in CI.
7. **Cost and freshness are this request's** — `a_memtable_query_reads_head_and_nothing_else`
   (**exactly 1** read: HEAD) and `cost_is_per_request_not_cumulative`. ⚠️ The first of those
   failed at **3 reads** when written — existence, query and resolution each read HEAD — and
   the fix is a property rather than an optimisation: `Answer::segments` carries the snapshot,
   so existence is decided from it and resolution cannot race a fold that renumbers the
   segment list between two reads.
8. **Over a real socket** — `the_server_answers_over_a_real_socket`
   (`cargo test -p pstore-server --test socket`): bound on an ephemeral port, a durable write
   and a query round-tripped over TCP with a hand-written HTTP/1.1 client, a refusal that
   survives the real stack, and `serve` returning `Ok` on the shutdown signal rather than
   being killed. `the_server_refuses_to_start_without_a_lane` covers the other half.
9. **`cargo deny check`: advisories ok, bans ok, licenses ok, sources ok.** ⚠️ It was **red
   on `main` before this milestone** — `RUSTSEC-2026-0285`, a rustls TLS 1.3 handshake
   vulnerability reached through the object-store adapter's `reqwest`. `cargo update -p rustls`
   (0.23.43 → 0.23.45) closes it. Recorded because a gate that was already failing is a gate
   nobody was running, and the fix belongs to whoever next looks. ⚠️ `axum` is in
   `pstore-server` only, and `deny.toml` is **unchanged**: the binary serves an in-memory
   store, so it constructs no cloud backend at all and needs no wrapper entry. Named as a
   limitation rather than a design: a real backend is the next milestone's, with the
   `deny.toml` line that permits it.
10. **Coverage** — `./scripts/coverage.sh --fail-under-regions 95` passes at **95.12%**
    regions, 96.75% functions, 97.00% lines. ⚠️ It **failed at 94.78%** on the first run and
    the floor was not touched: `pstore-server` was at 86.58% because the error table and the
    door checks were unexercised, and covering them is what found the defects in criteria 6
    and 7. Mutation: `docker compose -f dev/docker-compose.yml exec dev scripts/mutants.sh
    --file crates/pstore-server/src/lib.rs crates/pstore-server/src/types.rs
    crates/pstore-query/src/run.rs` — **27 of 27
    viable mutants caught**, 55 unviable — re-swept after code review, over the query
    module the fix moved work into as well as the handlers. ⚠️ The first sweep missed one and it was worth
    having: the defaulted result count survived being mutated to **1**, because the test asserted
    `results.len() <= 10`.
    A **bound** is not a number — a default of 1 silently truncates every client that leaves
    the result count unset, and satisfies that assertion. Three matching documents, three results,
    observed red against the mutant.

11. **A query's sequential depth is 4, and flat in the segment count** —
    `a_query_costs_four_round_trips_however_many_segments_it_has`
    (`cargo test -p pstore-server --test depth`), measured through the router against
    `DepthCounting`. ⚠️ **Code review measured the first implementation at `3 + 2 × segments`
    — 19 sequential round trips at eight segments**, because resolving an id re-opened every
    segment and read a block from each, serially. Nothing asserted depth: the closest was
    `blob_reads > 1`, which a depth of 19 satisfies. Resolution now happens inside the query,
    over segments already open, in one fan-out round. ⚠️ **4 exceeds D-34's 3 and the ledger
    says so rather than the test being lenient**: the ranking is three and carrying the ids is
    a fourth, because a payload's address is not knowable until the ranking exists. The route
    to three is a format change — backlog.

## What this milestone found, and did not smuggle

⚠️ **A wrong-dimension query returned scored, ranked results with a `200`.** The dense leg
built its index with `query.len()` as the dimension, so it took the dimension **from the
query**: a two-dimensional vector over a four-dimensional segment came back with plausible
rows. The exact path had always refused it (`Segment::search` compares against the row it
read), so the two paths disagreed — which is worse than either being wrong, because whichever
one a query takes depends on whether a centroid table exists. Found through the API, fixed in
`pstore-query` where the dimension now comes from the segment's field layout, and pinned by
`a_query_vector_of_the_wrong_dimension_is_refused_by_the_dense_leg` at that layer as well as
through the API. **This was not in the spec**, and the spec is amended rather than the fix
being folded in silently.

⚠️ **Two blocking findings in code review, both measured rather than argued**, and both of
them properties this milestone's own criteria failed to state: a query's depth grew with the
segment count, and the write door compared vector widths **within a request** instead of
against the index — so two batches, each consistent with itself, produced an index of mixed
widths in which a two-dimensional document outranked an exact four-dimensional match. The
width is now checked against the rows the process holds; after a fold no process can know it
without a read, so that case is **loud at the next query** instead, where the dense leg
refuses per segment.

⚠️ **The engine could not resolve its own query results.** `Answer` carries `(segment, row)`
and the only public way to reach an id was `scan`, which reads every unpruned block — a
request that scales with documents. `Segment::ids_at` reads only the blocks holding the wanted
rows, in one round, touching no vector section. Also a spec amendment.

## Stated non-properties

These are what M7c is **not**, recorded here so that nothing downstream reads the existence of
a server as more than it is:

- **No authentication and no quota.** A tenant is whatever the header says. `pstore-meter`
  exists and is not wired. This server is not deployable to anyone.
- **The engine registry is unbounded.** One `Engine` per tenant, held forever; eviction is a
  decision about the memtable's durability contract, not an LRU.
- **`cost` cross-attributes under concurrency.** It is a difference of the tenant's monotone
  counters, exact for one request in flight.
- **Nothing detects two servers on one lane.** `PSTORE_LANE` makes it an operator's explicit
  choice; a bundle is still an unconditional `put`, so a collision is silent. Backlog.
- **A query is four sequential round trips, not three.** Flat in the segment count, and one
  more than D-34 allows. The ids are what cost it.
- **After a fold, a process cannot know an index's vector width without a read**, so a
  wrong-width write is accepted there and refused at the next query rather than at the door.
- **The binary serves an in-memory store**, so its own durability claim ends at the process.
  Criterion 3 proves the architecture through two `Api`s over one backend; a real backend is
  a milestone with a cloud account in it.
