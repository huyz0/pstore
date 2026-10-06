# M54 — One index searched by several servers at once

**Serves:** the project owner's request of 2026-10-05: single-index, multi-node parallel
search. Today one query of one index runs on whichever server received it, however large
the index.

## What is true today

Read from the tree at M53 (`d350f6c`), nothing measured:

- **One server answers the whole query.** `POST /v1/indexes/{index}/query` reaches
  `Engine::answer`, which hands every segment of the index to `pstore_query::run`:
  1. one round opens every segment;
  2. one round runs every leg over every segment;
  3. one round fetches the ranked rows.

  With HEAD, that is D-34's four rounds, flat in the segment count (M47). The rounds are wide,
  but every byte of every leg is read and scored by the one server.
- **`pstore-server` knows no other server.** It does not depend on `pstore-cluster` or
  `pstore-gossip`, and it has no HTTP client. `docs/deploy.md` says nothing routes a tenant to
  a server.
- **Write-time sharding is declined** (`turbopuffer-api-parity.md`, `roadmap.md`). This
  milestone does not reopen it. The unit of parallelism here is the **segment** an index
  already has, so no data moves, and no server owns any of it.
- **What can be split, and what cannot:**
  - **Dense and sparse legs score a row by itself.** A leg's hits over segment A are comparable
    with its hits over segment B, which is what `run` already relies on when it merges segments.
  - **A text leg cannot be split for free.** BM25 scores against statistics summed over every
    segment (`Stats::merge`, D-30). A server holding a subset of segments would score with that
    subset's IDF, which gives the wrong ranking, and gathering the global statistics first costs
    a round D-34 does not have.

## Delta

**A server configured with peers sends each segment's dense and sparse legs to the server
the segment is assigned to, and merges what comes back exactly as it merges its own segments
today.** Text legs, fusion, shadowing and fetching rows stay on the receiving server, the
**coordinator**.

1. **Assignment, by rendezvous hash over the segment key.** The servers form a fixed list of
   base URLs, and each server knows which entry is itself.
   - URLs are compared with any trailing `/` removed, and a duplicate URL refuses to start.
   - A segment is assigned to the server with the highest `mix(fnv(url ‖ 0 ‖ segment key))`,
     where `mix` is a 64-bit finaliser, since FNV alone mixes poorly (spec review).
   - So every coordinator assigns a segment to the same server. That server's read cache keeps
     the segment warm whoever coordinates.
   - Adding a server moves about 1/N of the segments.
   - Assignment is a cache preference, never ownership. Any server can scan any segment
     (rule 5).
2. **Who does what:**
   - **The coordinator:**
     - reads HEAD;
     - opens every segment with only the sidecars its own legs need, which is just the footer
       and delete vector for a segment whose vector legs run elsewhere;
     - runs the text legs over every segment, with the global statistics;
     - runs the vector legs over its own segments and the fresh segment;
     - merges each leg's hits across segments, applies the shadow, fuses, and fetches the
       ranked rows, all as today.
   - **A peer**, given its segments, the vector legs, the filter, the full-text schema and the
     shadow's size:
     - opens them;
     - runs each leg widened or exhaustive by `run`'s own rule;
     - masks by the filter and drops deleted rows;
     - cuts each list to what can survive the shadow;
     - returns per `(segment, leg)` the hits, as row and score bits, numbered by the
       coordinator's ordinals.
   - Both sides call the same function, so the per-segment work is defined once.
   - **What a part carries** (spec review). The peer derives nothing the coordinator decided:
     - each segment as the coordinator built it from HEAD: key, length, whether it has a
       centroid table (none below the exact-scan threshold means scan exactly, M27), whether
       it has a sparse dictionary, its delete vector's key, and whether it is shadowed;
     - each leg with its position, and the whole `vec_index::Query`. Vectors are sent **after
       the metric's transform**, as `f32` bits, and the peer does not transform them again;
     - the raw `filters` JSON and the index's `FullText` schema. The peer parses the filter
       with the public parser and binds it to that schema's analyzer, as the coordinator does
       (M14.2);
     - `|shadow|`;
     - a **protocol version**. A peer on another version refuses with `409`, and the
       coordinator runs that part itself (rule 5): a rolling deploy never mixes two builds'
       scoring.
3. **Exact, not approximate.**
   - A distributed answer equals the single-server answer, bit for bit: ids, order, scores,
     `$dist` and attributes.
   - Every per-segment cut is the one `run` makes today, the hits keep the coordinator's
     ordinals, so ties break the same way, and scores travel as `f32` bits.
4. **Depth is unchanged.**
   - The peers are called as soon as HEAD names the segments, alongside the coordinator's open
     round.
   - A peer's part is two rounds on its own store: open and legs. So the slowest path is HEAD,
     open, legs, then the row fetch: four rounds, D-34 as restated by M47.
   - The coordinator fetches rows from segments it has already opened, so no segment is opened
     twice on its side.
5. **A peer that fails costs rounds, never answers.** A part that errors, times out
   (`PSTORE_PEER_TIMEOUT_MS`, default 2,000), or is refused, is run by the coordinator itself
   over the same segments.
   - ⚠️ **This is an exception to D-34, on failure only.** It re-opens those segments with
     their vector sidecars: two more rounds, plus the timeout when the peer is slow.
   - A transport error, timeout, version refusal (`409`), guard refusal (`400`, which means a
     coordinator bug) or `5xx` is counted in `GET /metrics` as `pstore_peer_parts_failed`.
   - A query error the peer reports (`422`: wrong dimension, unknown field) is not counted.
     The coordinator still runs the part itself, so that the client sees the error the
     single-server path raises, mapped by the coordinator.
   - A `bounded` read whose cached HEAD names a reaped segment fails on the peer, falls back,
     fails again and retries from a fresh HEAD (M11.2). That is correct, and it shows in the
     failure count.
6. **The endpoint:** `POST /v1/internal/part`, tenant by header as every endpoint.
   - **Every key it is given must be one HEAD can name.** It must lie under that tenant's
     prefix (`{:04x}/tnt/{tenant}/idx/`) and contain no `..`. A segment key must end in `.seg`,
     and a delete vector's key must start with its segment's key and end in `.dv`. Any other
     key refuses the whole part with `400`, before any read.
   - HEAD names nothing else: replicas are copied into the destination tenant's own `seg/R/`.
   - Centroid and dictionary keys are not sent; they are derived from the segment key, which
     is what HEAD does too. Whether to read them is the coordinator's flag.
   - **Bounded inputs:** at most `MAX_LEGS` legs and 4,096 segments, and the body under
     axum's default limit. A coordinator whose share for one peer would exceed 4,096 segments
     does not split that query.
   - **Statuses:** `200` with the hits; `400` for the key or bounds guard; `409` for another
     protocol version; `422` for a query error (a leg over a field or of a dimension the
     segment does not have); `500` for a read that failed.
   - ⚠️ It authenticates the caller no more than the public API does, which takes the tenant
     from a header ("`auth` — the tenant is whatever the header says", `deploy.md`). It
     exposes nothing a query cannot already reach.
7. **When a query is split:**
   - Only when peers are configured, it has a dense or sparse leg, and its index has at least
     2 folded segments of which at least one is assigned elsewhere.
   - Never:
     - `order_by`;
     - aggregations;
     - text-only queries, including `sum` and `max` fusion, which are text-only;
     - `as_of` queries, which take their own path, `query_as_of_filtered`.

     These run as today.
   - A multi-query splits each sub-query by this rule: one part per peer per sub-query.
8. **Configuration:**
   - `PSTORE_PEERS`: every server's base URL, comma-separated, this one included.
   - `PSTORE_PEER_SELF`: this server's own URL, which must be in the list.
   - Both or neither, otherwise the server refuses to start. Neither means a single server, as
     today.
   - Documented in `docs/deploy.md`.

**Not changed:** the write path, HEAD, the format, the public API's requests and responses,
and every single-server answer.

**Not covered, and the ledger says so:**
- **The peer call is a node-to-node round trip** (~0.2 ms in a zone, `routing-and-placement.md`).
  D-34 counts blob rounds, and this adds none, but it is not free.
- **`meta.cost` counts the coordinator's requests only.** A split query under-reports the
  requests its peers made.
- **Text legs are not split.** That needs global BM25 statistics first, which is a round or a
  per-segment statistics summary in HEAD. That is a follow-up, and it is in the backlog.
- **Latency.** Spreading the scan helps only where one server's bandwidth or CPU binds. That
  is a measurement on real cloud machines (`slos.md`'s blocked rows), not on this container.
  Here the evidence is who read what.
- **Membership.** The peer list is static. Taking it from gossip (M50–M53) is a follow-up.
- **A filter with a hybrid query computes its mask twice** for a segment whose vector legs
  run elsewhere: once on the peer, once on the coordinator for the text legs. That costs
  bytes, not rounds.

## Acceptance criteria

1. **The answer is the single-server answer, through the API.**
   - Setup: three servers on real sockets share one store, each with the other two as peers,
     and index parameters that cluster every segment. A fourth server shares the store with
     no peers.
   - The corpus has at least 8 folded segments and some deleted rows, and its keys are chosen
     so that `assign` gives every server at least one segment. The test checks this with
     `assign` itself.
   - Each query below is asked of a peered server and of the unpeered one, and the two
     responses' `results` (ids, order, score and `$dist` bits, attributes) and `epoch` are
     equal:
     - dense, with no filter, with a filter, and with `exact`;
     - dense and text, fused by RRF and by weighted RRF, with and without a filter;
     - a multi-query of two of these.
   - `meta.cost`, `session` and `staleness_ms` are per process, and are not compared.
   - Test: `a_split_query_answers_exactly_as_one_server_does`, in
     `crates/pstore-server/tests/peers.rs`.
1b. **The answer is the single-server answer, in the query layer**, for what the API cannot
   ask:
   - sparse;
   - two dense legs;
   - a shadow of unfolded ids over shadowed segments;
   - deleted rows;
   - a filter.

   An engine's `query_split_as`, each share run by `Engine::part` on another engine over the
   same store (the call the endpoint makes), equals its `query_filtered_as`, hit for hit, in
   score and `$dist` bits, ids and attributes. Test: `a_split_query_equals_the_unsplit_one`, in
   `crates/pstore-engine/tests/split.rs` (amended in implementation: real folds build real
   clustered and sparse segments, which the query layer's tests would have to hand-build).
2. **Each segment is scanned once, by its assigned server.** In AC1's setup, each server's
   store is wrapped to record the keys it reads. For a dense query asked of server A:
   - each segment's centroid table (`.cen`) is read by exactly the server `assign` names, and by
     no other: a `.cen` is read only by the dense leg's open, so it marks where the leg ran;
   - A reads every segment's footer (its row fetch needs them) and no other server's `.cen`;
   - every server reads at least one `.cen`;
   - `pstore_peer_parts_served` rises by one on each peer, and `pstore_peer_parts_sent` by
     two on A.

   Test: `each_segment_is_scanned_once_by_its_server`.
3. **Depth, end to end.** Each server's view of the shared store waits 250 ms before every
   request, so rounds cannot overlap by accident, and all caches are cold. (Amended in
   implementation: a wait switched on after the fixture is written, which `Faulty`'s latency,
   fixed at construction, cannot do.)
   - A split dense query completes in under 4.5 × 250 ms. A chain of five rounds, such as the
     peers called only after the coordinator's open, takes at least 1,250 ms.
   - The test is seen to fail with the peer call moved after the open.
   - Test: `a_split_query_keeps_the_round_trip_budget`.
4. **A failed peer.** Three cases, each answering equal to the unpeered server, as in AC1:
   - one peer's URL points at a closed port;
   - another peer's store fails every read;
   - a peer reports another protocol version.

   `pstore_peer_parts_failed` rises in each case. Test: `a_failed_peer_costs_rounds_not_answers`.
4b. **A query error is the client's, and not a failure.** A dense query of the wrong dimension
   over a split index gets the same status and code as from the unpeered server, and
   `pstore_peer_parts_failed` does not move. Test: `a_query_error_on_a_peer_is_the_clients`.
5. **Assignment**, as a unit test:
   - the same key gets the same server whatever the list order, and with a trailing `/`;
   - over 10,000 keys and 4 servers, each server holds 25% ± 3%;
   - a fifth server takes 20% ± 3% of the keys, and every moved key moved to it.

   Test: `assignment_is_stable_balanced_and_moves_only_to_a_new_server`.
6. **The endpoint guards its keys and bounds.**
   - A part naming a key outside the tenant's prefix, with `..`, a segment key not ending in
     `.seg`, or a delete vector not under its segment is refused with `400`, and its store
     sees no read.
   - So is a part with more than `MAX_LEGS` legs.
   - Test: `a_part_outside_its_tenant_is_refused`.
7. **What is not split, is not.** Each of these sends no part, and `pstore_peer_parts_sent` is
   unchanged: `order_by`, an aggregation, a text-only query, `sum` fusion, `as_of`, and an
   index of one segment. An unpeered server answers as before, and the existing suite passes.
   Test: `queries_that_cannot_be_split_run_on_one_server`.
8. **Configuration.**
   - Both variables set and consistent: accepted.
   - Refused: either variable alone, `PSTORE_PEER_SELF` absent from `PSTORE_PEERS`, a
     duplicate URL, or a timeout that is not a positive number.
   - Neither set: single-server.
   - Test: `peers_are_both_or_neither_and_include_self`, in the server's config tests.
9. **Gates:**
   - `./scripts/gates.sh` passes;
   - the incremental mutation sweep of the changed lines misses 0
     (`--no-config --profile mutants`, `./scripts/mutants.sh`);
   - `cargo deny check` passes its licence and ban checks with any new dependency.
